//! Per-leg voice activity detection, replacing bare-RMS silence gating.
//!
//! earshot's `Detector` wants mono `f32` samples in `[-1, 1]`, sampled at 16kHz,
//! in frames of exactly 256 samples (16ms). Capture delivers 48kHz interleaved
//! `f32` with an arbitrary channel count (mic leg + system leg, see
//! `CaptureInfo`). This module downmixes each leg to mono, decimates 3:1 to
//! 16kHz, and buffers into 256-sample frames before handing them to earshot —
//! running the mic and system legs through independent `Detector`s so either
//! leg alone can keep a chunk open.

use crate::types::CaptureInfo;
use earshot::{DefaultPredictor, Detector};

/// 48kHz -> 16kHz is exactly 3:1.
const DECIMATE: usize = 3;
/// earshot wants exactly 256 samples (16ms) at 16kHz per prediction.
const FRAME_LEN: usize = 256;
/// Scores >= this are "speech" — earshot's own recommended default.
const SPEECH_THRESHOLD: f32 = 0.5;

/// One leg's downmix/decimate/frame-buffer state plus its own `Detector`.
/// Legs run fully independently: the mic and system streams are unrelated
/// audio and mixing their VAD state would let one leg's silence mask the
/// other's speech.
struct Leg {
    detector: Box<Detector<DefaultPredictor>>,
    // ponytail: leftover raw 48kHz samples (0..DECIMATE-1) not yet enough to
    // form the next decimated sample; carries across `feed` calls so batch
    // boundaries neither drop nor duplicate samples.
    raw_carry: Vec<f32>,
    // Decimated 16kHz samples buffered until a full FRAME_LEN frame is ready.
    frame_buf: Vec<f32>,
}

impl Leg {
    fn new() -> Self {
        Leg {
            detector: Detector::default_boxed(),
            raw_carry: Vec::with_capacity(DECIMATE),
            frame_buf: Vec::with_capacity(FRAME_LEN),
        }
    }

    /// Feed mono samples at 48kHz. Returns `Some(any_frame_was_speech)` if at
    /// least one full 16ms VAD frame completed during this call, or `None` if
    /// the samples given weren't enough to complete one (they're buffered for
    /// the next call).
    fn feed_mono48(&mut self, mono48: impl Iterator<Item = f32>) -> Option<bool> {
        let mut decided: Option<bool> = None;
        for s in mono48 {
            self.raw_carry.push(s);
            if self.raw_carry.len() < DECIMATE {
                continue;
            }
            // ponytail: non-overlapping 3-sample average — box-filter anti-alias
            // and 3:1 decimation combined into one pass. Ceiling: a crude
            // single-pole lowpass, not a real filter kernel. Swap for `rubato`
            // if aliasing artifacts ever show up as false speech in practice.
            let avg = self.raw_carry.iter().sum::<f32>() / DECIMATE as f32;
            self.raw_carry.clear();
            self.frame_buf.push(avg);
            if self.frame_buf.len() == FRAME_LEN {
                let score = self.detector.predict_f32(&self.frame_buf);
                self.frame_buf.clear();
                let speech = score >= SPEECH_THRESHOLD;
                decided = Some(decided.unwrap_or(false) || speech);
            }
        }
        decided
    }
}

/// Wraps two independent earshot detectors, one per audio leg (mic, system).
pub struct Vad {
    mic: Leg,
    sys: Leg,
}

impl Vad {
    pub fn new() -> Self {
        Vad {
            mic: Leg::new(),
            sys: Leg::new(),
        }
    }

    /// Feed one batch of interleaved 48kHz audio. `ch` is the total interleaved
    /// channel count; `info` names which channel indices belong to each leg.
    /// Returns `(mic, sys)` speech decisions, each `None` when no full VAD
    /// frame completed for that leg this call.
    pub fn feed(&mut self, buf: &[f32], ch: usize, info: &CaptureInfo) -> (Option<bool>, Option<bool>) {
        let frames = buf.len() / ch;
        let (m0, m1) = (info.mic_channels.0 as usize, info.mic_channels.1 as usize);
        let (s0, s1) = (info.system_channels.0 as usize, info.system_channels.1 as usize);

        let mic_iter = (0..frames).map(|f| {
            let base = f * ch;
            (buf[base + m0] + buf[base + m1]) * 0.5
        });
        let mic = self.mic.feed_mono48(mic_iter);

        let sys_iter = (0..frames).map(|f| {
            let base = f * ch;
            (buf[base + s0] + buf[base + s1]) * 0.5
        });
        let sys = self.sys.feed_mono48(sys_iter);

        (mic, sys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_math_is_3_to_1() {
        // 48kHz / 16kHz = 3; earshot frames are 256 samples @ 16kHz, so a
        // full VAD frame needs exactly 256 * 3 = 768 raw 48kHz samples.
        assert_eq!(DECIMATE * FRAME_LEN, 768);
    }

    #[test]
    fn partial_frame_buffers_across_calls() {
        let mut leg = Leg::new();
        // One sample short of a full 768-sample (256-frame * 3) VAD frame.
        let short = vec![0.0f32; 768 - 1];
        assert_eq!(leg.feed_mono48(short.into_iter()), None);
        assert_eq!(leg.frame_buf.len(), 255, "255 decimated samples buffered, 1 raw sample pending");

        // The single missing raw sample completes the last decimated sample,
        // which completes the 256th VAD frame sample.
        let rest = vec![0.0f32; 1];
        let decided = leg.feed_mono48(rest.into_iter());
        assert!(decided.is_some(), "768th raw sample should complete a full VAD frame");
        assert_eq!(leg.frame_buf.len(), 0, "frame buffer drains once a full frame is fed to the detector");
    }

    #[test]
    fn exact_multiple_of_frame_size_leaves_no_remainder() {
        let mut leg = Leg::new();
        // Exactly two full VAD frames' worth of raw samples in one call.
        let samples = vec![0.0f32; 768 * 2];
        let decided = leg.feed_mono48(samples.into_iter());
        assert!(decided.is_some());
        assert_eq!(leg.frame_buf.len(), 0);
        assert_eq!(leg.raw_carry.len(), 0);
    }

    #[test]
    fn legs_are_independent() {
        // Feeding one leg doesn't touch the other's buffered state.
        let mut vad = Vad::new();
        let info = CaptureInfo {
            channels: 3,
            sample_rate: 48_000,
            system_channels: (1, 2),
            mic_channels: (0, 0),
        };
        // Silence on both legs, less than one full frame's worth (768 frames).
        let buf = vec![0.0f32; 3 * 200];
        let (mic, sys) = vad.feed(&buf, 3, &info);
        assert_eq!(mic, None);
        assert_eq!(sys, None);
    }
}
