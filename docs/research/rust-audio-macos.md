# macOS audio capture from Rust — research (2026)

Companion to `audio-recording-and-processing.md` (cross-platform/Python-era prior research)
and `quill-prior-art.md` (shipped Swift implementation). This doc translates that prior art
into the concrete Rust crate/API story for `meetrs` and re-verifies the load-bearing claims
against current crates.io/docs.rs/Apple state as of 2026-08. Local dev machine: macOS 26.5.2
(`sw_vers`).

## Recommendation

Two independent capture graphs, quill's shape, ported to Rust with `objc2`:

1. **System audio: Core Audio process tap**, not ScreenCaptureKit. Bind directly via
   `objc2-core-audio` (0.3.2) — it already exports `AudioHardwareCreateProcessTap`,
   `CATapDescription`, `AudioHardwareCreateAggregateDevice`, and
   `AudioDeviceCreateIOProcIDWithBlock` as safe-signature `unsafe extern "C-unwind" fn`s. No
   hand-written FFI needed for the happy path. Wrap the tap in a private aggregate device
   whose **main sub-device is a real output device** (not the tap alone — a tap-only
   aggregate silently yields zero samples). Pass an explicit **non-nil serial
   `dispatch2::DispatchQueue`** to `AudioDeviceCreateIOProcIDWithBlock` — required
   unconditionally on macOS 26, previously optional.
2. **Microphone: `cpal`** (0.18.1) for the raw path — it's the standard, portable, actively
   maintained Rust audio-I/O crate and needs no Apple-specific glue for a plain input
   stream. Apple's Voice-Processing AEC has no `cpal` hook; if you want it, that means
   dropping to `objc2-avf-audio`'s `AVAudioEngine`/`AVAudioInputNode.setVoiceProcessingEnabled_error`
   directly (own graph, own risk — see quill's RCA in the prior-art doc) or skip AEC and do
   transcript-level echo suppression instead, which the prior art already recommends and
   Rust's two-track design gives you for free.
3. **Do not attempt a single aggregate device with drift compensation across mic + tap.**
   quill's finding holds in Rust too, more strongly: `cpal`'s stream is a separate
   CoreAudio object graph from the tap's IOProc, and there's no portable Rust API that lets
   you retarget one onto the other's aggregate. Two clocks, wall-clock offset correction at
   session-start, same as quill.
4. **Minimum macOS version: 14.4** for the process-tap path — verified: `AudioHardwareCreateProcessTap`
   and `CATapDescription` were introduced in **macOS 14.2** (confirmed via Apple's own
   availability docs and corroborated by multiple sample projects/forum threads), but both
   AudioCap (targets 14.4+) and the DGR Labs source recommend a **14.4+ deployment floor** for
   a specific, named reason, not a vague "rough edge": on 14.2/14.3 the process-tap permission
   request lands in a different/inconsistent TCC categorization and the prompt copy diverges
   from 14.4+'s stable `SystemAudioCaptureRequests` behavior. If you need Sonoma-only
   distribution this is achievable; if you're fine
   requiring **15.0+** you additionally unlock `SCStream`'s `captureMicrophone`, which would
   let ScreenCaptureKit alone do both — but that pulls in the screen-recording TCC prompt
   and menu-bar recording indicator for an audio-only feature, which is the reason to avoid
   SCK here (matches the OBS study in the prior research).
5. Do the FFI work by hand in `unsafe` blocks per the sketches below; do not adopt `cidre`
   (pre-1.0, single-maintainer, "personal research project" per its own README) or
   `coreaudio-rs`/`coreaudio-sys` (the former's own docs now point users at `objc2-core-audio`
   for direct Core Audio access; the latter has no process-tap bindings — this is a newer
   API than its bindgen scope).
6. Ship unbundled to start (ad-hoc `codesign -s -`, like quill); a `__TEXT,__info_plist`
   section built via `build.rs` gets you `NSMicrophoneUsageDescription` /
   `NSAudioCaptureUsageDescription` TCC attribution without an `.app` bundle, same trick
   quill uses. For distribution, `cargo-bundle` (actively updated, last crates.io release
   2026-05-30) gets you a real `.app` + Info.plist; Developer ID signing + hardened runtime
   + notarization via `apple-codesign` (works cross-platform, no Xcode needed) or plain
   `codesign`/`xcrun notarytool`.

## Binding crates

| Crate | Version (crates.io, 2026-08) | Role | Maturity |
|---|---|---|---|
| `objc2` | 0.6.4 | Objective-C runtime + message-send core | Mature. ~90M downloads. The foundation everything below builds on. |
| `objc2-core-audio` | 0.3.2 | `AudioHardwareCreateProcessTap`, `CATapDescription`, aggregate-device functions, `AudioDeviceCreateIOProcIDWithBlock` | Auto-generated from Apple headers, covers the tap API fully (verified below). Small download count (2.8M) relative to `objc2-foundation` reflects narrowness of use case, not risk. |
| `objc2-core-audio-types` | 0.3.2 | Supporting types (`AudioStreamBasicDescription`, etc.) for the above | Same generation pipeline. |
| `objc2-audio-toolbox` | latest 0.3.x | `AudioQueue`, `AudioConverter`, higher-level AudioToolbox surface if needed | Exists; not required for the tap path itself. |
| `objc2-screen-capture-kit` | 0.3.2 | `SCStream`, `SCStreamConfiguration`, delegate protocols | Only needed if you go the SCK route (not recommended here). |
| `objc2-avf-audio` | 0.3.2 | `AVAudioEngine`, `AVAudioInputNode`, voice processing (verified: these types live in `objc2-avf-audio`, **not** `objc2-av-foundation` — see correction below) | Only needed if you add Apple AEC to the mic path. |
| `objc2-av-foundation` | 0.3.2 | `AVAsset`/`AVAssetReader`/export/media-file surface — no audio-engine types here | Not needed for either capture path; listed only because it's easy to confuse with `objc2-avf-audio` above. |
| `objc2-core-media` | 0.3.2 | `CMSampleBuffer` accessors | Only needed alongside SCK. |
| `objc2-core-foundation` | 0.3.2 | `CFDictionary`, `CFString`, etc. — aggregate-device description dict | Needed for both paths (property dictionaries are `CFDictionary`). |
| `block2` | 0.6.2 | Objective-C/C blocks as Rust closures, for `AudioDeviceIOBlock`, SCK stream-output block, dispatch block APIs | Mature, ~79M downloads, same authorship as `objc2`. |
| `dispatch2` | 0.3.1 | GCD queues (`DispatchQueue::new(..., DispatchQueueAttr::SERIAL)`) — required by the tap IOProc on macOS 26 | Mature, actively updated (2026-02). |
| `cpal` | 0.18.1 | Cross-platform mic (and generic output) capture | Mature, RustAudio org, ~17M downloads, actively released (2026-06). |
| `coreaudio-rs` / `coreaudio-sys` | 0.14.2 / 0.2.18 | Older AudioUnit-era wrapper/raw bindings | Still maintained (releases into 2026) — verified: `coreaudio-rs`'s own README says, verbatim, "If you just want direct access to the Core Audio APIs, use the appropriate crates of the objc2 project" (a general pointer to the `objc2-*` family, not specifically naming `objc2-core-audio`). No process-tap bindings observed. Fine for classic `AudioUnit` work, not this feature. |
| `screencapturekit` (doom-fish) | 8.0.1 | High-level safe SCK wrapper: capture, `with_captures_audio`, mic input (macOS 15+), `CMSampleBuffer` handlers | Actively maintained, used by 50+ OSS projects (AFFiNE, Cap). The pragmatic choice *if* you decide SCK is worth it. |
| `cidre` | 0.16.1 | Multi-framework Apple bindings incl. Core Audio and (per its changelog) a system-audio-capture helper | Pre-1.0, single-maintainer "personal research project" per its own README. Interesting design (zero-cost async blocks) but not a dependency to build a product on yet. `[unverified]`: exact tap-API coverage in its current release — not independently confirmed against source. |
| `cocoa` / `objc` (legacy) | 0.26.1 / 0.2.7 | Pre-`objc2` Cocoa/runtime bindings | Verified: `objc` 0.2.7 last published 2019-10-18 (crates.io API), no newer version exists. `cocoa`'s own README (servo/core-foundation-rs) opens with an explicit statement, stronger than "defers new work" — **"This crate has been deprecated in favour of the `objc2` crates."** Treat as legacy — don't start new code on them. |
| `core-foundation` / `core-foundation-sys` | 0.10.1 | Older CF bindings, independent of `objc2` | Still widely used (381M downloads) but for *new* Apple-framework work in 2026, `objc2-core-foundation` is the modern equivalent and interoperates directly with the `objc2-*` family. |

## ScreenCaptureKit from Rust

`SCStream` audio is genuinely usable through `screencapturekit` (doom-fish/screencapturekit-rs,
v8.0.1, dual MIT/Apache-2.0, 230 GitHub stars, 46 forks). It exposes:

- `SCStreamConfiguration::with_captures_audio(true)`, `.with_sample_rate(...)`,
  `.with_channel_count(...)` — audio needs the `macos_13_0` feature flag; `captureMicrophone`
  needs `macos_15_0`.
- Registered output handlers (trait-based or closure) receive `CMSampleBuffer` for
  `SCStreamOutputType::Audio`; the crate's README states handlers "must be `Send + Sync`" and
  provides extension methods to pull frame/PCM data out, meaning the delegate/callback
  wiring across the Rust↔ObjC boundary is done for you rather than something to hand-roll.
- The crate claims "memory safe — proper retain/release, leak-tested," i.e. it isn't a thin
  `unsafe` shim; it's closer to a real safe wrapper.
- The **1-2 channel limit** on SCK audio and per-app audio filtering (via
  `SCContentFilter`'s app/window list) are framework-level constraints, not Rust-side ones —
  they show up identically whether driven from Swift or from `objc2-screen-capture-kit`
  directly.

The reason this doc doesn't recommend the SCK path for meetrs isn't crate maturity — it's the
same tradeoff the prior research already reached studying OBS: SCK for audio pulls in the
screen-recording TCC category and its always-visible recording indicator, for a feature
that's audio-only. If a future meetrs version wants per-app audio filtering specifically
(e.g. "capture only Zoom's audio"), SCK is the correct tool and `screencapturekit-rs` is a
credible dependency to reach for at that point.

If instead binding `objc2-screen-capture-kit` directly (for tighter control, or to avoid a
second audio path's dependency tree), the shape is:

```rust
use objc2_screen_capture_kit::{SCStreamConfiguration, SCStream, SCStreamOutputType};
use objc2_core_media::CMSampleBuffer;
use block2::RcBlock;

// SCStreamOutput delegate registration is done via a protocol object in objc2's
// generated bindings — construct an SCStreamDelegate-conforming object (objc2 supports
// defining Objective-C subclasses via `define_class!`) and hand it to
// `add_stream_output(_, type_, sample_handler_queue:)`. The actual sample callback
// (`- (void)stream:didOutputSampleBuffer:ofType:`) arrives on the dispatch queue you pass,
// as a CMSampleBuffer; extract the AudioBufferList via
// CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer to get raw PCM float32.
```

`[unverified]`: exact `define_class!`-based delegate pattern for `SCStreamOutput` in current
`objc2-screen-capture-kit` — sketch only; verify signatures against `docs.rs/objc2-screen-capture-kit`
before implementing, this doc did not exercise the delegate protocol end to end.

## Core Audio process taps from Rust

This is the path this doc recommends, and it's bound further than the prior research
assumed — the symbols exist as real `objc2-core-audio` functions, not something you need to
declare yourself.

Verified function signatures (from docs.rs, `objc2-core-audio` 0.3.2, feature `AudioHardware`):

```rust
pub unsafe extern "C-unwind" fn AudioHardwareCreateProcessTap(
    in_description: Option<&CATapDescription>,
    out_tap_id: *mut AudioObjectID,
) -> i32;

pub unsafe extern "C-unwind" fn AudioHardwareCreateAggregateDevice(
    in_description: &CFDictionary,
    out_device_id: NonNull<AudioObjectID>,
) -> i32;

pub unsafe extern "C-unwind" fn AudioDeviceCreateIOProcIDWithBlock(
    out_io_proc_id: NonNull<AudioDeviceIOProcID>,
    in_device: AudioObjectID,
    in_dispatch_queue: Option<&DispatchQueue>,   // dispatch2::DispatchQueue
    in_io_block: AudioDeviceIOBlock,              // block2-backed block type
) -> i32;
```

`CATapDescription` itself and `CATapMuteBehavior` are bound as real `objc2` types (not
opaque `CFTypeRef`s), gated behind the same `AudioHardware` + `objc2` feature pair — meaning
you construct/configure them with ordinary Rust method calls. **Verified against
docs.rs/objc2-core-audio 0.3.2 directly (method list + individual method signatures, not
inferred)** — the earlier draft of this doc used a snake_case method name
(`init_stereo_global_tap_but_exclude_processes`) that doesn't exist; `objc2`'s Apple-framework
bindings preserve the original camelCase ObjC selector as the Rust method name, and the
initializer's second parameter is `&NSArray<NSNumber>`, not a bare `&NSArray`:

```rust
use objc2_core_audio::{CATapDescription, CATapMuteBehavior, AudioHardwareCreateProcessTap,
                        AudioObjectID};
use objc2_foundation::{NSArray, NSNumber};

unsafe {
    // Confirmed signature: pub unsafe fn initStereoGlobalTapButExcludeProcesses(
    //     this: Allocated<Self>, processes_object_i_ds_to_exclude_from_tap: &NSArray<NSNumber>,
    // ) -> Retained<Self>
    let desc = CATapDescription::initStereoGlobalTapButExcludeProcesses(
        CATapDescription::alloc(),
        &NSArray::<NSNumber>::from_slice(&[]), // exclude none => tap everything
    );
    desc.setPrivate(true);
    desc.setMuteBehavior(CATapMuteBehavior::Unmuted);
    // Do NOT touch `isExclusive`/`setExclusive` after this initializer — see Corrections
    // section. (Confirmed: `isExclusive`'s doc comment on docs.rs reads "True if this
    // description should tap all processes except the process listed in the 'processes'
    // property" — matches the folklore claim exactly, not just repeating it uncritically.)

    let mut tap_id: AudioObjectID = 0;
    let status = AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id);
    assert_eq!(status, 0 /* noErr */);
}
```

Aggregate device creation still goes through a `CFDictionary` description (there's no
strongly-typed wrapper for the aggregate-device property dictionary — you build it with
`objc2-core-foundation`'s `CFDictionary`/`CFMutableDictionary`, same shape as the Swift
`[String: Any]` dictionary quill builds):

```rust
use objc2_core_foundation::{CFDictionary, CFString, CFBoolean, CFArray};

// Keys from objc2_core_audio: kAudioAggregateDeviceIsPrivateKey,
// kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceSubDeviceListKey,
// kAudioAggregateDeviceTapListKey. The DGR Labs source states flatly that the aggregate's
// SubDeviceList MUST name a real output device as kAudioAggregateDeviceMainSubDeviceKey,
// and that "tap as the main sub-device with an empty sub-device list silently produces
// zero samples" -- i.e. per that source, a tap-only aggregate does NOT work. quill's own
// shipped, working configuration uses exactly that shape (empty SubDeviceList, tap only
// in TapList) -- see Corrections/Open-questions for this direct, unresolved contradiction
// between a live shipped implementation and the DGR Labs claim.
```

`AudioDeviceCreateIOProcIDWithBlock` takes a `dispatch2::DispatchQueue` and a block built
from a Rust closure via `block2`; this is where the mandatory-serial-queue requirement (see
Corrections) actually gets enforced in code:

```rust
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use block2::RcBlock;
use objc2_core_audio::{AudioDeviceCreateIOProcIDWithBlock, AudioDeviceIOProcID};

let queue = DispatchQueue::new(c"com.meetrs.tap-ioproc", DispatchQueueAttr::SERIAL);

let io_block = RcBlock::new(
    move |_now, in_data, _in_time, _out_data, _out_time| {
        // REALTIME CONTEXT — see "Realtime constraints" below.
        // in_data: NonNull<AudioBufferList> — read PCM here, hand off via a
        // pre-allocated lock-free ring buffer. No allocation. (Verified exact
        // AudioDeviceIOBlock signature below — all five params are NonNull<_>,
        // not raw pointers as an earlier draft of this sketch implied.)
        //
        // Wrap the actual body in catch_unwind so a panic can never reach the
        // extern "C-unwind" boundary and unwind into Core Audio's own frames —
        // see "Realtime constraints" for why that's the primary mitigation,
        // with `panic = "abort"` as defense-in-depth, not a substitute for this:
        // let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        //     /* real callback body */
        // }));
    },
);

let mut io_proc_id: AudioDeviceIOProcID = std::ptr::null_mut();
unsafe {
    AudioDeviceCreateIOProcIDWithBlock(
        NonNull::new(&mut io_proc_id).unwrap(),
        aggregate_device_id,
        Some(&queue),      // non-nil is now mandatory on macOS 26 -- see Corrections
        &io_block,
    );
}
```

**Verified** (fetched `docs.rs/objc2-core-audio/0.3.2/objc2_core_audio/type.AudioDeviceIOBlock.html`
directly): the exact type is

```rust
pub type AudioDeviceIOBlock = *mut DynBlock<
    dyn Fn(
        NonNull<AudioTimeStamp>,   // in_now
        NonNull<AudioBufferList>,  // in_input_data
        NonNull<AudioTimeStamp>,   // in_input_time
        NonNull<AudioBufferList>,  // out_output_data
        NonNull<AudioTimeStamp>,   // in_output_time
    ),
>;
```

five parameters, matching the sketch's shape, but every parameter is `NonNull<_>` — not a
raw `*const`/`*mut` pointer as an earlier draft of this doc implied for `in_data`.

Re-verification of prior claims:

- **`isExclusive` inversion — confirmed, still true.** `CATapDescription.isExclusive` is a
  *direction* flag on the process list, not an access-lock toggle. The
  `stereoGlobalTapButExcludeProcesses:` initializer sets it `true` by default, meaning "tap
  everything **except** the listed PIDs." Flipping it to `false` afterward silently inverts
  that to "tap **only** the listed PIDs" — with an empty exclude list that becomes "tap
  nothing," a config that still returns `noErr` everywhere. Confirmed independently by both
  AudioCap's Apple-adjacent sample code and a 2026-04 blog write-up (DGR Labs). Do not touch
  `isExclusive` after using the convenience initializer.
- **Mandatory aggregate wrapper — confirmed, refined, and now flagged as an open
  contradiction rather than a settled nuance.** It's not just "wrap the tap in an
  aggregate" — the DGR Labs source states the aggregate's `SubDeviceList` **must** include
  a real output device as `kAudioAggregateDeviceMainSubDeviceKey`, and says explicitly:
  "tap as the main sub-device with an empty sub-device list silently produces zero samples"
  — read literally, that source claims tap-only aggregates (tap in `TapList`, empty
  `SubDeviceList`) **do not work at all**, full stop. That is a direct contradiction of
  quill's own shipped, apparently-working production code
  (`SystemAppRecorder.swift`/`quill-prior-art.md` §2–3), which uses exactly that shape —
  empty `SubDeviceList`, tap only in `TapList` — and is a real product, not a sample. The
  prior draft of this doc resolved this tension by asserting quill's shape is "a different,
  valid shape" that "can work" — that assertion is **not supported** by anything actually
  fetched from DGR Labs; it was an inference, not a verified fact, and it directly
  contradicts DGR Labs' own words. Correct framing: this is an unresolved conflict between
  a named source and a shipped implementation, not a reconciled nuance — do not trust
  either claim over the other without testing quill's literal aggregate-dictionary shape on
  a real macOS 26 machine (see Open questions).
- **Zero-samples bug — confirmed live, not fixed, and now has three named causes.**
  Multiple independent 2026 sources describe the failure persisting through macOS 26.0–26.5
  betas: streams that run healthy for minutes then transition to all-`0.0f` with correct
  timestamps and cadence. The DGR Labs write-up (2026-04) enumerates three independent root
  causes producing the same symptom: (1) driving the tap through `AVAudioEngine`, which
  cannot retarget onto a tap-backed device despite returning `noErr`; (2) the tap-as-main-
  sub-device aggregate misconfiguration above; (3) the `isExclusive` inversion. This is
  broader than the prior research's single "zero-samples bug" framing — it's at least three
  bugs/misconfigurations that present identically, which is exactly why quill's
  first-second-liveness-check pattern (verify signal, don't trust callbacks) is the right
  mitigation regardless of which cause is at fault. Separately, macOS 26 shipped real audio
  bug fixes (Rogue Amoeba's write-up, confirmed by Apple's own release notes) for
  multi-device sample-rate mismatches — orthogonal to the tap-specific zero-sample issue,
  fixed in 26.1, not the same bug.
  **Cross-check against `rust-audio-processing.md`'s claim that cpal 0.18.0 "fixed" a
  silent-tap-returns-zeros bug:** cpal's fetched CHANGELOG entries for 0.18.0 list "Fix
  loopback capture returning silence due to disabled tap auto-start," "Fix loopback
  aggregate device UID collisions," and "Fix undefined behavior and silent failure in
  loopback device creation." None of these map cleanly onto DGR Labs' three named causes:
  cpal's internal loopback implementation doesn't drive the tap through `AVAudioEngine`
  (rules out cause 1) and doesn't expose `isExclusive` to the caller (rules out cause 3).
  The "disabled tap auto-start" fix is *adjacent to* cause 2 (aggregate misconfiguration) —
  same failure family, same symptom — but it's a narrower, distinct bug
  (`kAudioAggregateDeviceTapAutoStartKey` not set, vs. tap-as-main-sub-device) and there's no
  evidence cpal's fix touches the main-sub-device requirement at all. **Conclusion: cpal
  0.18.0 fixed a related zero-samples bug inside its own loopback path, not any of the three
  causes documented here for a hand-rolled tap** — a hand-rolled implementation is not
  protected by cpal's fix and must still get the aggregate shape, `isExclusive`, and (if
  applicable) `AVAudioEngine` avoidance right on its own.
- **macOS 26 requiring an explicit dispatch queue — confirmed, new information.** The DGR
  Labs source states plainly: "the dispatch queue must be non-nil. Passing nil 'to use the
  default real-time thread' silently fails to register the block on macOS 26." This means
  code that has run for two OS releases on `nil` (following Apple's own doc language that
  `nil` is legal and uses an internal realtime thread) can start silently no-op'ing on
  Tahoe. **Always pass an explicit serial `dispatch2::DispatchQueue`** — this is not
  optional defensive coding, it is now load-bearing.

## Dispatch queues from Rust

Both the tap IOProc and (if you go that route) `SCStream`'s output-handler registration
want a GCD queue. `dispatch2` (0.3.1, actively maintained, part of the `objc2` project) is
the crate: `DispatchQueue::new(label, DispatchQueueAttr::SERIAL)` creates one, and
`block2::RcBlock` wraps a Rust closure as the Objective-C/C block type Core Audio's
`AudioDeviceIOBlock` (and SCK's stream-output block) expect. The older standalone `dispatch`
crate (0.2.0, last released 2020) is legacy relative to `dispatch2` — same relationship as
`objc`→`objc2`: don't start new code on it.

The closure captured in `RcBlock::new` must be `Send` (GCD may run it on any thread in the
pool backing the queue, though a `SERIAL` queue guarantees ordering, not thread identity),
and — critically for realtime correctness — **must not allocate, lock, or panic** once
invoked in the audio callback path (see Realtime constraints below).

## AudioTee and reference helper binaries

**AudioTee** (`makeusabrew/audiotee`) is a standalone Swift binary using the same
`AudioHardwareCreateProcessTap` API (macOS 14.2+), writing PCM chunks to stdout with logging
on stderr — a subprocess-friendly design matching the "native helper binary, subprocessed"
shape the prior Python research already recommended. It's used today via a Node.js wrapper
(`audioteejs`) that spawns it and relays stdout as data events.

Whether wrapping AudioTee beats binding directly, for meetrs specifically: **bind directly.**
meetrs is already a native Rust binary — subprocessing a separate Swift helper adds a
process boundary, a second build toolchain (Swift + Xcode), and an IPC/framing protocol for
no benefit meetrs doesn't already get from being Rust in-process. AudioTee's design makes
sense for callers who *aren't* native (the Python prior research's rationale, and
audioteejs's use case) — meetrs isn't in that position. Read AudioTee's source as a second,
independently-shipped confirmation of the tap/aggregate/IOProc recipe (it corroborates
AudioCap and the DGR Labs source), not as a dependency.

**AudioCap** (`insidegui/AudioCap`) is Apple-ecosystem-adjacent sample code (not
Apple-authored, but closely mirrors what Apple's own WWDC session on the API would show),
targeting macOS 14.4+, and is the clearest single reference for the create-tap →
create-aggregate → `AudioDeviceCreateIOProcIDWithBlock` sequence, including the
`kAudioAggregateDeviceIsPrivateKey` detail quill also uses.

## Permissions/TCC from Rust

- **Core Audio process taps need no entitlement.** Unlike ScreenCaptureKit (no entitlement,
  but a screen-recording TCC prompt) or microphone (entitlement + TCC), the DGR Labs source
  states process taps need `NSAudioCaptureUsageDescription` and a signed binary, but this is
  a *different* TCC category (`SystemAudioCaptureRequests`) from screen recording, reset via
  `tccutil reset SystemAudioCaptureRequests <bundle-id>`. This matches and refines the prior
  research's framing ("ScreenCaptureKit needs no entitlement, only TCC") — the process-tap
  path has its own, narrower TCC category, separate from both mic and screen-recording.
- **TCC is keyed to code-signing identity — reconfirmed.** An unsigned `cargo run` binary is
  never prompted at all; ad-hoc signing (`codesign -s -`) is required even for local dev, and
  the permission grant resets whenever the signing identity changes (rebuild-with-different-
  cert, etc.). quill's approach — embed the `Info.plist` into `__TEXT,__info_plist` via
  `-sectcreate` at link time, no `.app` bundle needed — is a documented, legitimate Apple
  technique and is exactly what an unbundled Rust binary needs to do the same thing. Wire it
  through `build.rs` emitting `cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,<path>`
  (or the equivalent `RUSTFLAGS`), pointing at a plist containing at minimum
  `CFBundleIdentifier`, `CFBundleName`, `NSMicrophoneUsageDescription`, and
  `NSAudioCaptureUsageDescription`.
- **Mic still needs entitlement + TCC + usage string** if going through `AVAudioEngine`
  voice processing; a plain `cpal` input stream needs the `NSMicrophoneUsageDescription` key
  and TCC consent, no special entitlement beyond what any mic-using app needs
  (`com.apple.security.device.audio-input` only matters under the App Sandbox, which meetrs
  as an unsandboxed daemon doesn't use).
- **Shipping** requires a Developer ID Application certificate, the hardened runtime
  (`codesign --options runtime`), and notarization (`xcrun notarytool submit ... && xcrun
  stapler staple`). From Rust, either shell out to Apple's own `codesign`/`notarytool`, or use
  `apple-codesign` (crates.io, cross-platform, can sign/notarize without Xcode installed —
  useful for CI on non-Mac runners, irrelevant if building on a Mac already; **verified: its
  last crates.io release is 0.29.0 from 2024-11-29** — nearly two years stale as of 2026-08,
  so confirm it still notarizes successfully against Apple's current API before depending on
  it for a release pipeline, or default to shelling out to Apple's own `codesign`/`notarytool`
  which can't go stale the same way). Packaging into
  a `.app`: `cargo-bundle` (actively released, last update 2026-05-30) is the closest
  equivalent to Xcode's product bundling for a Rust binary and supports extra
  `Info.plist` keys via `osx_info_plist_exts`; `cargo-packager` (0.11.8, last release
  2025-11) is the newer, more actively-marketed alternative with broader installer-format
  support (DMG, MSI, etc.) — either is viable, `cargo-bundle`'s narrower macOS-only focus is
  arguably the better fit if meetrs stays Mac-only. `create-dmg` (a shell script, not a Rust
  crate) is the standard way to wrap the finished `.app` into a distributable disk image.

## Realtime constraints

The IOProc block and SCK's sample-output callback both run on a **realtime-priority audio
thread** managed by Core Audio/GCD — the same constraints as any real-time audio callback in
any language apply, and Rust doesn't relax them:

- **No heap allocation.** `Vec::push`, `Box::new`, `String` formatting, etc. can call into
  `malloc`, which is not lock-free and can block for an unbounded time if another thread
  holds the allocator lock — the classic priority-inversion audio glitch. Pre-allocate every
  buffer before the callback is registered; hand data off via a lock-free SPSC ring buffer
  (e.g. `rtrb`, `ringbuf`, or a hand-rolled one) to a consumer thread that does the actual
  file I/O / resampling / anything allocating.
- **No locks (mutexes).** Same priority-inversion argument as allocation. Use atomics or a
  lock-free structure for the handoff.
- **No panics across the callback boundary — this is where Rust adds a wrinkle Swift/ObjC
  don't have.** The bound function signatures above are `unsafe extern "C-unwind" fn`.
  **Verified against RFC 2945 (the RFC that introduced the ABI) and the Rust reference:**
  the prior draft's claim that unwinding through the boundary is "undefined by C++/ObjC-
  runtime unwind semantics" overstates the language-level story and should be corrected —
  RFC 2945 exists specifically to make this *well-defined* at the Rust-ABI level: with
  `extern "C"` (no `-unwind`), a panic that would escape the function is caught and the
  process aborts, deterministically, by design (not UB, by current Rust semantics); with
  `extern "C-unwind"`, an unwinding panic **is allowed to propagate** through the boundary,
  and this is safe *specifically when* the intervening foreign frames also use a compatible
  native unwind mechanism (RFC 2945's own framing: Rust frames "sandwiched" between
  unwind-aware C++/native frames). The real risk here isn't Rust-level UB — it's that
  **Apple's Core Audio C/ObjC runtime is not documented by Apple as unwind-safe**, so even
  though the ABI's *Rust-side* behavior is defined, what happens inside Core Audio's own
  frames once a foreign unwind passes through them is unspecified by Apple and shouldn't be
  relied on; in practice this risks corrupted audio-thread/runtime state or a hard crash
  rather than a clean error. **The primary mitigation is `std::panic::catch_unwind` wrapped
  around the closure body itself** — turn a would-be panic into a caught `Result` and log
  or no-op instead of ever letting it reach the `extern "C-unwind"` boundary at all; this is
  the standard, generally-recommended pattern for FFI callbacks, not just an alternative to
  `panic = "abort"`. **Setting `panic = "abort"` in the release profile is defense-in-depth
  on top of that**, not a substitute for it: if a panic somehow still escapes the
  `catch_unwind` wrapper (e.g. a panic during unwinding itself, or code added later that
  forgets the wrapper), `panic = "abort"` guarantees an immediate, deterministic process
  abort instead of an unwind whose behavior inside Core Audio's frames is unverified —
  trading "the daemon dies" for "the daemon corrupts state and then dies less predictably."
  Either way, the actual fix is: **the callback body must not be able to panic** — no
  `.unwrap()`, no array-index-out-of-bounds, no integer overflow in debug builds, validate
  all buffer-size assumptions with `if`/`return` rather than `assert!`. quill's own crash
  class (an uncatchable ObjC exception deep in the AVFoundation resampler from a zero-frame
  file) is the same lesson from the other side of the FFI boundary: framework code you call
  *from* the callback can throw/abort in ways your Rust `catch_unwind` never sees either.
- Everything downstream of the ring-buffer handoff (file writes, resampling, VAD, the liveness
  check itself) can be normal Rust on a normal thread — only the code that runs *inside* the
  IOProc/SCK-output block is under the realtime constraint.

## Corrections to prior research

- **`audio-recording-and-processing.md`'s single-aggregate recommendation is superseded by
  quill's two-graph finding for a text-output product**, and that finding transfers to Rust
  unchanged (arguably strengthened: `cpal` gives no path to retarget onto a tap-backed
  aggregate any more than `AVAudioEngine` does). Recommendation 3 above adopts quill's
  two-graph shape rather than the earlier single-aggregate one.
- **"No crate wraps `CATapDescription` yet" (quill-prior-art.md §9) is now false.**
  `objc2-core-audio` 0.3.2 wraps `CATapDescription`, `CATapMuteBehavior`, and the process-tap
  /aggregate-device functions directly as typed Rust bindings, not raw C shims you declare
  yourself. This table entry should be read as outdated — written before, or without
  checking, current `objc2-core-audio` coverage.
- **The zero-samples bug is not one bug.** The earlier framing (both prior docs) treats it as
  a single named phenomenon ("a tap can look healthy... and deliver pure 0.0f"). Current
  sourcing (DGR Labs, 2026-04) identifies at least three independent causes producing the
  identical symptom: `AVAudioEngine` retargeting, tap-as-main-sub-device misconfiguration,
  and `isExclusive` inversion. The mitigation (verify signal, don't trust callbacks) is
  unchanged and still correct, but "fix the bug" isn't a single fix.
- **macOS 26's dispatch-queue requirement is new information neither prior doc had** —
  `audio-recording-and-processing.md` doesn't mention it at all, and `quill-prior-art.md`'s
  own source code passes a real dispatch queue already (so quill was never exposed to this),
  but doesn't call out that `nil` used to be legal and stopped being effectively honored on
  26. Worth a code comment wherever meetrs creates the IOProc.
- **`coreaudio-rs`/`coreaudio-sys` framing in `quill-prior-art.md` §9** ("reachable via
  `coreaudio-sys`/`objc2` bindings, but... no crate wraps `CATapDescription`... unverified")
  should be replaced with: use `objc2-core-audio` directly; `coreaudio-sys`/`coreaudio-rs`
  are the wrong layer for this specific API and their own current docs say so.

## Open questions

- Exact `AudioDeviceIOBlock` closure signature in `objc2_core_audio` — **resolved** during
  this fact-check pass, see the verified type definition above; no longer open.
- Exact `SCStreamOutput` delegate-registration pattern in current `objc2-screen-capture-kit`
  (relevant only if the SCK path is ever revisited) — not exercised end-to-end here.
- Whether `cidre`'s system-audio-capture addition (mentioned in a 2026 forum thread re: its
  changelog) is a full CATapDescription wrapper or a narrower helper — not independently
  confirmed against its source; irrelevant to the recommendation above either way since
  `cidre` isn't recommended as a dependency yet.
- Whether the tap-as-main-sub-device zero-sample failure and quill's own aggregate shape
  (empty `SubDeviceList`, tap in `TapList` only) are actually the same configuration or
  meaningfully different — the sourcing here doesn't reconcile them precisely enough to say
  quill's exact shape is provably safe under the DGR Labs failure description; test quill's
  literal aggregate-dictionary shape against a real macOS 26 machine before trusting it
  verbatim.
- Whether Apple's voice-processing AEC (`AVAudioInputNode.setVoiceProcessingEnabled_error` in
  the `objc2-avf-audio` binding — verified exact method name; Swift's public API surfaces this
  as the throwing `setVoiceProcessingEnabled(_:)`) is worth attempting at all in meetrs given
  quill shipped it **disabled by default** after discovering it can't fully suppress ducking
  — this doc did not re-examine that tradeoff, it's carried over from the prior-art teardown
  unchanged.
- No independent benchmark of `cpal`'s mic-capture latency/reliability alongside a live
  Core Audio tap on the same machine — both prior docs and this one are desk research, not
  measurement.
- Whether `apple-codesign` (last released 2024-11-29, per this pass's crates.io check) still
  notarizes successfully against Apple's current notarization API — not tested here; verify
  before depending on it in a CI pipeline, or default to Apple's own `notarytool`.

## Sources

- [objc2 — crates.io](https://crates.io/crates/objc2), [objc2-core-audio — crates.io](https://crates.io/crates/objc2-core-audio), [objc2-screen-capture-kit — crates.io](https://crates.io/crates/objc2-screen-capture-kit), [objc2-av-foundation — crates.io](https://crates.io/crates/objc2-av-foundation), [objc2-core-media — crates.io](https://crates.io/crates/objc2-core-media), [objc2-core-foundation — crates.io](https://crates.io/crates/objc2-core-foundation), [block2 — crates.io](https://crates.io/crates/block2), [dispatch2 — crates.io](https://crates.io/crates/dispatch2), [cpal — crates.io](https://crates.io/crates/cpal), [coreaudio-rs — crates.io](https://crates.io/crates/coreaudio-rs), [coreaudio-sys — crates.io](https://crates.io/crates/coreaudio-sys), [screencapturekit — crates.io](https://crates.io/crates/screencapturekit), [cidre — crates.io](https://crates.io/crates/cidre), [cocoa — crates.io](https://crates.io/crates/cocoa), [cargo-bundle — crates.io](https://crates.io/crates/cargo-bundle), [cargo-packager — crates.io](https://crates.io/crates/cargo-packager), [apple-codesign — crates.io](https://crates.io/crates/apple-codesign) — direct crates.io API queries, 2026-08-03.
- [objc2_core_audio — docs.rs](https://docs.rs/objc2-core-audio/latest/objc2_core_audio/) and its `AudioHardwareCreateProcessTap`, `AudioHardwareCreateAggregateDevice`, `AudioDeviceCreateIOProcIDWithBlock` function pages — function signatures.
- [GitHub - madsmtm/objc2](https://github.com/madsmtm/objc2)
- [GitHub - doom-fish/screencapturekit-rs](https://github.com/doom-fish/screencapturekit-rs)
- [GitHub - RustAudio/coreaudio-rs](https://github.com/RustAudio/coreaudio-rs)
- [GitHub - RustAudio/coreaudio-sys](https://github.com/RustAudio/coreaudio-sys)
- [GitHub - insidegui/AudioCap](https://github.com/insidegui/AudioCap)
- [GitHub - makeusabrew/audiotee](https://github.com/makeusabrew/audiotee), [GitHub - makeusabrew/audioteejs](https://github.com/makeusabrew/audioteejs)
- [GitHub - marysaka/dispatch2](https://github.com/marysaka/dispatch2)
- [GitHub - burtonageo/cargo-bundle](https://github.com/burtonageo/cargo-bundle)
- [Capturing System Audio on macOS in 2026: What an iOS Dev Needs to Know — DGR Labs](https://dgrlabs.co/blog/2026-04-25-capturing-system-audio-on-macos-in-2026.html) — primary source for isExclusive semantics, macOS 26 dispatch-queue requirement, three-cause zero-samples breakdown, aggregate-device main-sub-device requirement, TCC category detail.
- [MacOS 26 (Tahoe) Includes Important Audio-Related Bug Fixes — Rogue Amoeba](https://weblog.rogueamoeba.com/2025/11/04/macos-26-tahoe-includes-important-audio-related-bug-fixes/) — corroborates macOS 26.0→26.1 audio bug fixes (distinct issue from the tap zero-samples bug).
- [AudioHardwareCreateProcessTap(_:_:) — Apple Developer Documentation](https://developer.apple.com/documentation/coreaudio/audiohardwarecreateprocesstap(_:_:))
- Local: `sw_vers` on the research machine (macOS 26.5.2, build 25F84).
- Prior art (read in full before this research): `/Users/kerry.hatcher/projects/meetrs/docs/research/audio-recording-and-processing.md`, `/Users/kerry.hatcher/projects/meetrs/docs/research/quill-prior-art.md`.
- Fact-check pass additionally used: direct `curl` against the crates.io API (with a
  descriptive User-Agent — anonymous requests are rejected) for every crate version claim;
  `docs.rs` HTML fetched and parsed directly (not paraphrased) for `CATapDescription`'s
  method list and `AudioDeviceIOBlock`'s type definition; raw `README.md`/`CHANGELOG.md`
  fetched from GitHub for `cidre`, `coreaudio-rs`, `cocoa` (`servo/core-foundation-rs`), and
  `cpal`; [RFC 2945 — "C-unwind" ABI](https://rust-lang.github.io/rfcs/2945-c-unwind-abi.html)
  for the panic/FFI-unwind claims; a second WebSearch pass corroborating the DGR Labs
  quotations already cited above.

## Fact-check log (2026-08-03)

**CONFIRMED**
- `objc2-core-audio` 0.3.2 is real and current on crates.io; `AudioHardwareCreateProcessTap`,
  `AudioHardwareCreateAggregateDevice`, and `AudioDeviceCreateIOProcIDWithBlock` signatures
  as stated in the doc are byte-for-byte correct (fetched directly from docs.rs).
- Every other version number in the "Binding crates" table checked directly against the
  crates.io API: `objc2` 0.6.4, `objc2-screen-capture-kit` 0.3.2, `objc2-av-foundation` 0.3.2,
  `objc2-avf-audio` 0.3.2, `objc2-core-media` 0.3.2, `objc2-core-foundation` 0.3.2,
  `objc2-audio-toolbox` 0.3.2, `block2` 0.6.2, `dispatch2` 0.3.1, `cpal` 0.18.1,
  `coreaudio-rs` 0.14.2, `coreaudio-sys` 0.2.18, `core-foundation` 0.10.1, `cocoa` 0.26.1,
  `objc` 0.2.7 (released 2019-10-18, no newer version exists), `cidre` 0.16.1,
  `screencapturekit` 8.0.1, `cargo-bundle` 0.11.0 (last release 2026-05-30), `cargo-packager`
  0.11.8 (last release 2025-11-27) — all match the doc exactly.
- `cocoa`'s README states the crate is deprecated in favor of `objc2` (stronger than the
  doc's original "defers new work to" phrasing — corrected).
- `coreaudio-rs`'s README does point users at the `objc2` project for direct Core Audio
  access (general pointer, not naming `objc2-core-audio` specifically — corrected).
- `cidre`'s own README states "This is personal research project" — matches the doc's claim
  verbatim.
- `isExclusive`'s doc comment on docs.rs ("True if this description should tap all processes
  except the process listed in the 'processes' property") confirms the doc's inversion
  claim is not folklore — it's the documented behavior.
- The DGR Labs 2026-04 blog post is real and its content matches every quotation attributed
  to it in the doc: `isExclusive` semantics, the mandatory non-nil dispatch queue on macOS
  26, the three named zero-samples root causes, the aggregate main-sub-device requirement,
  and the `SystemAudioCaptureRequests` TCC category / `tccutil reset` command.
- `AudioHardwareCreateProcessTap`/`CATapDescription` were introduced in macOS 14.2 (confirmed
  via search of Apple's own availability annotations and multiple corroborating sources);
  AudioCap's own repo title independently confirms a 14.4+ target.
- cpal's CHANGELOG entries for 0.17.0/0.18.0 (loopback support, tap-auto-start fix, UID
  collision fix, undefined-behavior fix) match `rust-audio-processing.md`'s citations
  exactly, fetched directly from GitHub raw content.
- RFC 2945 confirms `extern "C-unwind"` is the correct mechanism referenced, and that
  `panic = "unwind"` (default) plus `extern "C-unwind"` is what allows a panic to propagate
  across the FFI boundary at all — the doc's core recommendation direction (guard against
  panics, prefer deterministic abort over undefined unwind behavior) is correct engineering
  advice.
- Apple's process-tap TCC category, entitlement-free requirement, and the
  `NSAudioCaptureUsageDescription` framing are consistent with the DGR Labs source.

**CORRECTED (said → true, with source)**
- `CATapDescription::init_stereo_global_tap_but_exclude_processes(...)` → the actual bound
  method is `CATapDescription::initStereoGlobalTapButExcludeProcesses(this, &NSArray<NSNumber>)`
  — `objc2`'s Apple-framework bindings keep the ObjC selector's camelCase name verbatim, they
  are not converted to snake_case. Source: `docs.rs/objc2-core-audio/0.3.2` method list,
  fetched and parsed directly.
- `objc2-av-foundation` listed in the binding table as the crate providing `AVAudioEngine`/
  `AVAudioInputNode` → those types live in **`objc2-avf-audio`**, a separate crate;
  `objc2-av-foundation` covers `AVAsset`/`AVAssetReader`/export/media-file APIs and has no
  audio-engine surface. (The doc's own Recommendation section already correctly named
  `objc2-avf-audio` in prose — only the binding table had the wrong crate.) Source:
  `docs.rs/objc2-avf-audio` and `docs.rs/objc2-av-foundation` struct listings, fetched
  directly and diffed.
- `AVAudioInputNode.setVoiceProcessingEnabled` (as a directly-callable Rust method) →
  the actual `objc2-avf-audio` binding is `setVoiceProcessingEnabled_error` (there is no
  bare `setVoiceProcessingEnabled` setter in the generated bindings; Swift's throwing
  setter sugar maps to the ObjC `setVoiceProcessingEnabled:error:` selector). Source:
  `docs.rs/objc2-avf-audio/0.3.2/objc2_avf_audio/struct.AVAudioInputNode.html` method list.
- `AudioDeviceIOBlock`'s closure parameters described loosely as pointers (`in_data: *const
  AudioBufferList`) → the verified type is
  `*mut DynBlock<dyn Fn(NonNull<AudioTimeStamp>, NonNull<AudioBufferList>,
  NonNull<AudioTimeStamp>, NonNull<AudioBufferList>, NonNull<AudioTimeStamp>)>` — five
  parameters as sketched, but every one is `NonNull<_>`, not a raw pointer. This was
  previously marked `[unverified]`; now resolved. Source: `docs.rs/objc2-core-audio/0.3.2`
  `type.AudioDeviceIOBlock.html`, fetched and parsed directly.
- The 14.4-floor rationale stated as a vague "rough edge in 14.2/14.3" → DGR Labs gives a
  specific, named reason: on 14.2/14.3 the process-tap permission request lands in a
  different/inconsistent TCC categorization with divergent prompt copy versus the stable
  14.4+ behavior. Corrected to cite the actual reason instead of hand-waving it.
- The claim that quill's tap-only aggregate shape (empty `SubDeviceList`, tap only in
  `TapList`) "is a different, valid shape" that "can work" alongside the DGR Labs
  main-sub-device requirement → **this was an unsupported inference, not a verified fact**,
  and it directly contradicts DGR Labs' own words ("tap as the main sub-device with an empty
  sub-device list silently produces zero samples," read as a blanket statement about
  tap-only aggregates). Corrected to state this as an open, unresolved contradiction between
  a named source and quill's shipped implementation, not a reconciled nuance — this is
  probably the single highest-value correction in this pass, since the original text
  quietly talked the reader out of exactly the tension the sourcing can't actually resolve.
- The C-unwind/panic-safety reasoning stated that unwinding through the FFI boundary is
  "undefined by C++/ObjC-runtime unwind semantics not designed for this" → RFC 2945 defines
  this behavior at the Rust-ABI level (that's the RFC's whole purpose); the real,
  still-correct risk is that Apple's Core Audio runtime is not *documented* as unwind-safe,
  which is a narrower and more accurate claim. Also added: `std::panic::catch_unwind` around
  the callback body is the primary mitigation, with `panic = "abort"` as defense-in-depth —
  the original text presented `panic = "abort"` as the main fix, which is incomplete advice.
- `apple-codesign` described only as "works cross-platform, no Xcode needed" with no
  maintenance caveat → its last crates.io release is 0.29.0 from 2024-11-29, nearly two years
  stale as of 2026-08. Added a caution to verify it before depending on it in a release
  pipeline.
- The cpal-0.18.0-fixed-the-zero-samples-bug claim from `rust-audio-processing.md` was never
  reconciled against this doc's three named causes → added an explicit reconciliation:
  cpal's fix (disabled tap auto-start) is adjacent to but distinct from DGR Labs' cause 2
  (main-sub-device misconfiguration), and doesn't touch causes 1 (`AVAudioEngine`
  retargeting) or 3 (`isExclusive` inversion) at all, since cpal's own loopback path uses
  neither mechanism. A hand-rolled tap implementation gets no protection from cpal's fix.

**STILL UNVERIFIED**
- Exact `SCStreamOutput` delegate-registration pattern in `objc2-screen-capture-kit` 0.3.2 —
  not exercised end-to-end in this pass either; the SCK code sketch remains a sketch.
- Whether quill's literal shipped aggregate-device dictionary shape actually produces
  non-zero samples on a real macOS 26.5 machine — the contradiction with DGR Labs above is
  now sharper, but still not resolved by any source fetched in this pass. Needs a hands-on
  test, not more reading.
- Whether `cidre`'s system-audio-capture changelog entry is a full `CATapDescription`
  wrapper or a narrower helper — not independently confirmed against its source in this pass
  either (irrelevant to the recommendation regardless).
- Whether `apple-codesign` 0.29.0 (2024-11-29) still successfully notarizes against Apple's
  current notarization service API — its staleness is now confirmed, but whether it's
  actually broken was not tested.
- No hands-on verification was performed of anything requiring a running macOS 26 tap/
  aggregate/IOProc — every correction above is a documentation/source-text verification, not
  a runtime test. The Recommendation's own repeated caveat ("test on a real macOS 26 machine
  before trusting it verbatim") remains the load-bearing caveat of this entire document.

**Recommendation: unchanged in substance.** Bind `objc2-core-audio` directly, avoid
`AVAudioEngine` for the tap, pass a non-nil serial dispatch queue, avoid `cidre`/
`coreaudio-rs` for this specific API — all of that holds up. The corrections tighten
precision (exact method/type names, the `objc2-av-foundation` vs `objc2-avf-audio` mixup,
the `catch_unwind`-before-`panic=abort` ordering) and sharpen one genuine open risk (the
tap-only-aggregate contradiction) that the prior draft had incorrectly smoothed over into a
non-issue. Anyone implementing this should read the "Mandatory aggregate wrapper" bullet
under "Re-verification of prior claims" before assuming quill's exact aggregate shape is
safe to copy verbatim.
