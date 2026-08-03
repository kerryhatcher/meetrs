//! macOS system-audio + microphone capture.
//!
//! One aggregate device = one global process tap (system audio, excluding our
//! own process to avoid feedback) + the default input device (mic), with
//! drift compensation on both legs. One IOProc on that aggregate means Core
//! Audio does the clock sync for us. See docs/research/rust-audio-macos.md
//! for the sourcing behind every non-obvious choice below (isExclusive
//! inversion, mandatory dispatch queue, tap-only-aggregate zero-samples risk).
//!
//! CONTRACT (do not change):
//!   pub fn start(ring_samples: usize) -> Result<(CaptureInfo, rtrb::Consumer<f32>)>
//!   pub fn stop()

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{Result, anyhow};
use block2::{DynBlock, RcBlock};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::AnyThread;
use objc2_core_audio::{
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertySelector, CATapDescription,
    CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePermissionsError, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyStreams, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectSystemObject,
    kAudioStreamPropertyVirtualFormat, kAudioSubDeviceDriftCompensationKey, kAudioSubDeviceUIDKey,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSNumber};

use crate::types::{CaptureInfo, SAMPLE_RATE};

/// The IOProc block's exact `Fn` shape (must match `AudioDeviceIOBlock`).
type IoBlockDyn = dyn Fn(
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
);

// ponytail: RcBlock<F> has no Send/Sync impls (raw pointer inside). We never
// call it ourselves -- Core Audio does, via the pointer handed to
// AudioDeviceCreateIOProcIDWithBlock -- we only hold it here so the retain
// stays alive until stop(), which may run on a different thread than
// start(). That retain/release is atomic at the ObjC-runtime level, so
// asserting Send+Sync for that sole use is sound.
// `RcBlock`'s Drop impl is the whole reason to hold it; nothing reads out of
// the tuple field.
#[allow(dead_code)]
struct SendBlock(RcBlock<IoBlockDyn>);
unsafe impl Send for SendBlock {}
unsafe impl Sync for SendBlock {}

// `stop()` is unreachable dead code until main.rs/ui.rs wire up shutdown
// (those are still todo!() stubs owned by other agents) -- until then the
// dead_code analyzer also can't see `State`'s fields as "read" inside it.
#[allow(dead_code)]
struct State {
    tap_id: AudioObjectID,
    aggregate_id: AudioObjectID,
    io_proc_id: AudioDeviceIOProcID,
    // Kept alive only so they aren't dropped early; Core Audio retains its
    // own copies internally (per AudioDeviceCreateIOProcIDWithBlock's docs).
    _queue: DispatchRetained<DispatchQueue>,
    _block: SendBlock,
}

static STATE: OnceLock<Mutex<Option<State>>> = OnceLock::new();
static DROPPED_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Samples dropped because the ring buffer was full. The writer should poll
/// this and surface `Status::Overrun` on increase.
pub fn dropped_samples() -> u64 {
    DROPPED_SAMPLES.load(Ordering::Relaxed)
}

/// Aggregate devices with more legs than this are refused rather than
/// silently truncated -- the realtime callback walks buffers with a plain
/// loop bound to this, not a heap allocation.
const MAX_STREAMS: usize = 8;

/// Tears down whatever partial state `start()` built up if it bails out
/// early. Every fallible step below registers its resource here first, so a
/// `?` anywhere just works instead of needing a bespoke cleanup block at
/// each call site (this used to be four copy-pasted unsafe blocks).
struct Guard {
    tap_id: Option<AudioObjectID>,
    aggregate_id: Option<AudioObjectID>,
    io_proc: Option<(AudioObjectID, AudioDeviceIOProcID)>,
    disarmed: bool,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        unsafe {
            if let Some((agg, proc_id)) = self.io_proc {
                AudioDeviceStop(agg, proc_id);
                AudioDeviceDestroyIOProcID(agg, proc_id);
            }
            if let Some(agg) = self.aggregate_id {
                AudioHardwareDestroyAggregateDevice(agg);
            }
            if let Some(tap) = self.tap_id {
                AudioHardwareDestroyProcessTap(tap);
            }
        }
    }
}

pub fn start(ring_samples: usize) -> Result<(CaptureInfo, rtrb::Consumer<f32>)> {
    let state_cell = STATE.get_or_init(|| Mutex::new(None));
    let mut state_guard = state_cell.lock().unwrap();
    if state_guard.is_some() {
        return Err(anyhow!("capture already started; call stop() first"));
    }

    // --- tap: global, excluding our own process (no self-feedback) ---
    let our_pid = std::process::id() as i32;
    let our_process_object: AudioObjectID = get_property(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyTranslatePIDToProcessObject,
        kAudioObjectPropertyScopeGlobal,
        Some(&our_pid.to_ne_bytes()),
    )
    .unwrap_or(0);

    let exclude_us = NSNumber::new_u32(our_process_object);
    let exclude_array = if our_process_object != 0 {
        NSArray::<NSNumber>::from_slice(&[&*exclude_us])
    } else {
        NSArray::<NSNumber>::from_slice(&[])
    };

    // SAFETY: `alloc()` + `init...` is the standard objc2 two-step Cocoa
    // initializer pattern; the description outlives the tap it creates.
    let tap_desc = unsafe {
        CATapDescription::initStereoGlobalTapButExcludeProcesses(
            CATapDescription::alloc(),
            &exclude_array,
        )
    };
    // Do NOT touch isExclusive: the convenience initializer already set it
    // correctly, and flipping it inverts to "tap only the excluded PIDs",
    // which with an empty list is "tap nothing" while still returning noErr.
    unsafe {
        tap_desc.setPrivate(true);
        tap_desc.setMuteBehavior(CATapMuteBehavior::Unmuted);
    }
    let tap_uid = unsafe { tap_desc.UUID().UUIDString() }.to_string();

    let mut tap_id: AudioObjectID = 0;
    let status = unsafe { AudioHardwareCreateProcessTap(Some(&tap_desc), &mut tap_id) };
    check(status, "AudioHardwareCreateProcessTap")?;
    let mut guard = Guard {
        tap_id: Some(tap_id),
        aggregate_id: None,
        io_proc: None,
        disarmed: false,
    };

    // --- default input device (mic) ---
    let default_input: AudioObjectID = get_property(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDefaultInputDevice,
        kAudioObjectPropertyScopeGlobal,
        None,
    )?;
    if default_input == 0 {
        return Err(anyhow!("no default input device (microphone) is available"));
    }
    let device_uid = device_uid_string(default_input)?;

    // --- aggregate device: sub-device (mic) + tap (system), drift-compensated ---
    let sub_device_dict = CFDictionary::<CFType, CFType>::from_slices(
        &[
            cfkey(kAudioSubDeviceUIDKey).as_ref(),
            cfkey(kAudioSubDeviceDriftCompensationKey).as_ref(),
        ],
        &[
            CFString::from_str(&device_uid).as_ref(),
            CFBoolean::new(true).as_ref(),
        ],
    );
    let sub_tap_dict = CFDictionary::<CFType, CFType>::from_slices(
        &[
            cfkey(kAudioSubTapUIDKey).as_ref(),
            cfkey(kAudioSubTapDriftCompensationKey).as_ref(),
        ],
        &[
            CFString::from_str(&tap_uid).as_ref(),
            CFBoolean::new(true).as_ref(),
        ],
    );
    let sub_device_list = CFArray::<CFDictionary>::from_objects(&[sub_device_dict.as_ref()]);
    let tap_list = CFArray::<CFDictionary>::from_objects(&[sub_tap_dict.as_ref()]);
    let agg_uid = format!("com.meetrs.aggregate.{}", std::process::id());

    let top_dict = CFDictionary::<CFType, CFType>::from_slices(
        &[
            cfkey(kAudioAggregateDeviceUIDKey).as_ref(),
            cfkey(kAudioAggregateDeviceNameKey).as_ref(),
            cfkey(kAudioAggregateDeviceIsPrivateKey).as_ref(),
            cfkey(kAudioAggregateDeviceTapAutoStartKey).as_ref(),
            cfkey(kAudioAggregateDeviceMainSubDeviceKey).as_ref(),
            cfkey(kAudioAggregateDeviceSubDeviceListKey).as_ref(),
            cfkey(kAudioAggregateDeviceTapListKey).as_ref(),
        ],
        &[
            CFString::from_str(&agg_uid).as_ref(),
            CFString::from_str("meetrs-capture").as_ref(),
            CFBoolean::new(true).as_ref(),
            CFBoolean::new(true).as_ref(),
            CFString::from_str(&device_uid).as_ref(),
            sub_device_list.as_ref(),
            tap_list.as_ref(),
        ],
    );

    let mut aggregate_id: AudioObjectID = 0;
    let status = unsafe {
        AudioHardwareCreateAggregateDevice(top_dict.as_ref(), NonNull::from(&mut aggregate_id))
    };
    check(status, "AudioHardwareCreateAggregateDevice")?;
    guard.aggregate_id = Some(aggregate_id);

    // --- discover the real per-stream layout (an aggregate of a stereo tap
    // + a mono/stereo mic presents as MULTIPLE streams, not one interleaved
    // blob -- kAudioDevicePropertyStreamFormat only ever returns stream 0's
    // ASBD, which is how the earlier version under-counted channels). ---
    let stream_ids = discover_input_streams(aggregate_id)?;
    if stream_ids.len() > MAX_STREAMS {
        return Err(anyhow!(
            "aggregate device has {} input streams, more than the {} this build supports",
            stream_ids.len(),
            MAX_STREAMS
        ));
    }
    let mut stream_channels: Vec<usize> = Vec::with_capacity(stream_ids.len());
    for &sid in &stream_ids {
        let format: AudioStreamBasicDescription = get_property(
            sid,
            kAudioStreamPropertyVirtualFormat,
            kAudioObjectPropertyScopeGlobal,
            None,
        )?;
        validate_stream_format(&format)?;
        stream_channels.push(format.mChannelsPerFrame as usize);
    }
    let total_channels: usize = stream_channels.iter().sum();

    // The tap's own channel count is known independently of stream order
    // (the stereo initializer always yields 2, but query it rather than
    // hardcode -- confirms the tap leg is the one we think it is).
    let tap_format: AudioStreamBasicDescription = get_property(
        tap_id,
        kAudioTapPropertyFormat,
        kAudioObjectPropertyScopeGlobal,
        None,
    )?;
    let tap_channels = tap_format.mChannelsPerFrame as usize;

    if stream_ids.len() < 2 {
        // Exactly the failure mode the coordinator's option 1 describes:
        // the tap leg never made it into the aggregate's stream list.
        return Err(anyhow!(
            "aggregate device only exposes {} input stream(s) -- the process tap \
             is missing from the aggregate (expected a mic stream and a tap stream)",
            stream_ids.len()
        ));
    }
    // Empirically (and per how AudioHardwareCreateAggregateDevice composes
    // streams): sub-device streams enumerate first, tap streams last. Verify
    // that assumption against the tap's own reported width instead of
    // trusting it blindly.
    if stream_channels.last() != Some(&tap_channels) {
        return Err(anyhow!(
            "aggregate stream order assumption violated: expected the last input \
             stream to be the {tap_channels}-channel tap, got channel layout {:?}",
            stream_channels
        ));
    }
    let mic_channels_total = total_channels - tap_channels;
    if mic_channels_total == 0 {
        return Err(anyhow!("aggregate device reports zero mic channels"));
    }
    let channels = total_channels as u16;

    let info = CaptureInfo {
        channels,
        sample_rate: SAMPLE_RATE,
        mic_channels: (0, mic_channels_total as u16 - 1),
        system_channels: (mic_channels_total as u16, channels - 1),
    };

    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_samples);
    let producer_cell = UnsafeCell::new(producer);

    let io_block: RcBlock<IoBlockDyn> = RcBlock::new(
        move |_now: NonNull<AudioTimeStamp>,
              in_data: NonNull<AudioBufferList>,
              _in_time: NonNull<AudioTimeStamp>,
              _out_data: NonNull<AudioBufferList>,
              _out_time: NonNull<AudioTimeStamp>| {
            // Realtime audio thread: no alloc, no locks, no I/O, no panics
            // escaping. catch_unwind is the primary guard; `panic = "abort"`
            // in Cargo.toml is the backstop if it's somehow bypassed.
            // `stream_channels`/`total_channels` are captured by value, built
            // once in start() -- reading them here allocates nothing.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: Core Audio never invokes an IOProc concurrently
                // with itself (one call in flight at a time, even across
                // dispatch-queue thread hops), so this is never aliased.
                let producer = unsafe { &mut *producer_cell.get() };
                let list = unsafe { in_data.as_ref() };
                let n_buffers = list.mNumberBuffers as usize;
                if n_buffers != stream_channels.len() {
                    return; // stream shape changed since start(); drop this cycle.
                }
                // AudioBufferList is a C flexible-array-member struct: the
                // Rust binding only declares `mBuffers: [AudioBuffer; 1]`,
                // so buffers beyond index 0 are read via pointer arithmetic
                // over the (guaranteed contiguous, #[repr(C)]) array.
                let bufs: *const AudioBuffer = list.mBuffers.as_ptr();

                // Per-buffer frame counts can disagree between streams; take
                // the min for actual interleaving and count the shortfall on
                // the longer buffer(s) as dropped.
                let mut min_frames = usize::MAX;
                let mut max_frames = 0usize;
                for (i, &ch) in stream_channels.iter().enumerate() {
                    let buf = unsafe { &*bufs.add(i) };
                    let frames = if ch == 0 || buf.mData.is_null() {
                        0
                    } else {
                        buf.mDataByteSize as usize / (ch * size_of::<f32>())
                    };
                    min_frames = min_frames.min(frames);
                    max_frames = max_frames.max(frames);
                }
                if min_frames == usize::MAX {
                    min_frames = 0;
                }
                if max_frames > min_frames {
                    DROPPED_SAMPLES.fetch_add(
                        ((max_frames - min_frames) * total_channels) as u64,
                        Ordering::Relaxed,
                    );
                }
                if min_frames == 0 {
                    return;
                }

                for frame in 0..min_frames {
                    for (i, &ch) in stream_channels.iter().enumerate() {
                        if ch == 0 {
                            continue;
                        }
                        let buf = unsafe { &*bufs.add(i) };
                        let data = buf.mData as *const f32;
                        for c in 0..ch {
                            let sample = unsafe { *data.add(frame * ch + c) };
                            if producer.push(sample).is_err() {
                                DROPPED_SAMPLES.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }));
        },
    );

    // Mandatory on macOS 26: a nil dispatch queue silently no-ops the IOProc.
    let queue = DispatchQueue::new("com.meetrs.tap-ioproc", DispatchQueueAttr::SERIAL);
    let block_ptr: objc2_core_audio::AudioDeviceIOBlock =
        (&*io_block as *const DynBlock<IoBlockDyn>) as *mut _;

    let mut io_proc_id: AudioDeviceIOProcID = None;
    let status = unsafe {
        AudioDeviceCreateIOProcIDWithBlock(
            NonNull::from(&mut io_proc_id),
            aggregate_id,
            Some(&queue),
            block_ptr,
        )
    };
    check(status, "AudioDeviceCreateIOProcIDWithBlock")?;
    guard.io_proc = Some((aggregate_id, io_proc_id));

    let status = unsafe { AudioDeviceStart(aggregate_id, io_proc_id) };
    check(status, "AudioDeviceStart")?;

    guard.disarmed = true;
    *state_guard = Some(State {
        tap_id,
        aggregate_id,
        io_proc_id,
        _queue: queue,
        _block: SendBlock(io_block),
    });

    Ok((info, consumer))
}

fn discover_input_streams(device_id: AudioObjectID) -> Result<Vec<AudioObjectID>> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: kAudioObjectPropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device_id,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    check(
        status,
        "AudioObjectGetPropertyDataSize(kAudioDevicePropertyStreams)",
    )?;
    let n = size as usize / size_of::<AudioObjectID>();
    let mut ids = vec![0u32; n];
    if n == 0 {
        return Ok(ids);
    }
    let mut io_size = size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut io_size),
            NonNull::new(ids.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    check(
        status,
        "AudioObjectGetPropertyData(kAudioDevicePropertyStreams)",
    )?;
    Ok(ids)
}

// Contract fn -- not yet called from main.rs/ui.rs (shutdown wiring owned by
// other agents), hence dead_code until that lands.
#[allow(dead_code)]
pub fn stop() {
    let Some(cell) = STATE.get() else { return };
    let mut guard = cell.lock().unwrap();
    let Some(state) = guard.take() else { return };
    unsafe {
        AudioDeviceStop(state.aggregate_id, state.io_proc_id);
        AudioDeviceDestroyIOProcID(state.aggregate_id, state.io_proc_id);
        AudioHardwareDestroyAggregateDevice(state.aggregate_id);
        AudioHardwareDestroyProcessTap(state.tap_id);
    }
}

/// Validates one input stream's format. Each stream in a multi-stream
/// aggregate has its own ASBD; there is no single "the aggregate's format"
/// (see module doc) so this is called once per stream, not once overall.
fn validate_stream_format(format: &AudioStreamBasicDescription) -> Result<()> {
    if format.mFormatID != kAudioFormatLinearPCM {
        return Err(anyhow!(
            "input stream format is not linear PCM (got {:#x})",
            format.mFormatID
        ));
    }
    let flags = format.mFormatFlags;
    let packed_float32 = flags & kAudioFormatFlagIsFloat != 0
        && flags & kAudioFormatFlagIsPacked != 0
        && flags & kAudioFormatFlagIsNonInterleaved == 0
        && format.mBitsPerChannel == 32;
    if !packed_float32 {
        return Err(anyhow!(
            "input stream format isn't packed float32 (flags {:#x}, {} bits/channel) \
             -- unsupported layout, see docs/research/rust-audio-macos.md",
            flags,
            format.mBitsPerChannel
        ));
    }
    if format.mSampleRate.round() as u32 != SAMPLE_RATE {
        return Err(anyhow!(
            "input stream negotiated {} Hz, need exactly {} Hz -- refusing to silently resample",
            format.mSampleRate,
            SAMPLE_RATE
        ));
    }
    Ok(())
}

fn device_uid_string(device_id: AudioObjectID) -> Result<String> {
    let ptr: *const CFString = get_property(
        device_id,
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
        None,
    )?;
    let ptr = NonNull::new(ptr as *mut CFString)
        .ok_or_else(|| anyhow!("device UID property returned null"))?;
    // SAFETY: AudioObjectGetPropertyData follows the "copy" rule for CF
    // properties -- the caller owns the returned object and must release it.
    let uid: CFRetained<CFString> = unsafe { CFRetained::from_raw(ptr) };
    Ok(uid.to_string())
}

/// Build a `CFString` from one of the `&CStr` property-key constants.
fn cfkey(key: &std::ffi::CStr) -> CFRetained<CFString> {
    CFString::from_str(key.to_str().expect("property keys are ASCII"))
}

/// Fetch a fixed-size Core Audio property. `T` must exactly match the
/// property's underlying C type (e.g. `u32` for an `AudioObjectID`,
/// `AudioStreamBasicDescription` for a stream format).
fn get_property<T>(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: objc2_core_audio::AudioObjectPropertyScope,
    qualifier: Option<&[u8]>,
) -> Result<T> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = size_of::<T>() as u32;
    let mut value: MaybeUninit<T> = MaybeUninit::uninit();
    let (q_size, q_ptr) = match qualifier {
        Some(bytes) => (bytes.len() as u32, bytes.as_ptr() as *const c_void),
        None => (0u32, std::ptr::null()),
    };
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            NonNull::from(&mut address),
            q_size,
            q_ptr,
            NonNull::from(&mut size),
            NonNull::new(value.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    check(status, "AudioObjectGetPropertyData")?;
    Ok(unsafe { value.assume_init() })
}

/// Map an `OSStatus` to an actionable error, decoding it as a four-char code
/// when printable (Core Audio statuses are almost always one) and calling
/// out permission failures by name instead of a bare number.
fn check(status: i32, what: &str) -> Result<()> {
    if status == 0 {
        return Ok(());
    }
    let bytes = (status as u32).to_be_bytes();
    let printable = bytes.iter().all(|&b| (0x20..=0x7e).contains(&b));
    let code = if printable {
        format!("'{}'", bytes.iter().map(|&b| b as char).collect::<String>())
    } else {
        status.to_string()
    };
    if status == kAudioDevicePermissionsError {
        return Err(anyhow!(
            "{what} failed: OSStatus {status} ({code}) -- permission denied. Grant \
             \"System Audio Recording\" (and Microphone) access to this binary in \
             System Settings > Privacy & Security, then relaunch."
        ));
    }
    Err(anyhow!("{what} failed: OSStatus {status} ({code})"))
}
