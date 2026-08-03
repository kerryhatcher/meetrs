# Audio recording & processing — prior research

Index of existing research on macOS audio capture, transcription, and TTS. All of it
lives in the `deskwork` workspace; paths below are relative to `~/projects/deskwork/`.
This file is a map, not a copy — read the source docs before implementing.

Two efforts are directly load-bearing for meetrs: **macos-audio-capture** (getting the
bytes) and **audio-transcription-pipeline** (turning them into text). Both are marked
complete. They were written for a proposed project called `robo-notes`, which is
essentially what meetrs is.

## 1. Capture — `research/macos-audio-capture/`

Question: capture mic + system audio simultaneously without taking exclusive ownership
of a device (the OBS problem). Complete 2026-06-29.

| Path | Min macOS | Notes |
|---|---|---|
| ScreenCaptureKit | 15.0 for mic, 13.0 system-only | One API for both. What OBS actually uses. |
| Core Audio process taps | 14.2+, 14.4 safer | System audio only — bring your own mic capture and mixer. |
| BlackHole + Aggregate device | any | Fallback. You own the clock-drift problem. |

Recommendation: ScreenCaptureKit if you can require macOS 15; Core Audio tap + a native
mixer if you need Sonoma.

Findings that will bite you if ignored:

- **Python cannot drive Core Audio taps.** PyObjC's own CoreAudio docs say the
  maintainer is not convinced the API works from Python. Preferred shape is a native
  Swift/Rust helper binary emitting PCM on stdout, subprocessed from Python (reference
  implementation: AudioTee). — `python-paths.md`
- **"Zero samples" bug.** A tap can look healthy — correct timestamps, correct cadence —
  and deliver pure `0.0f`. Unfixed into macOS 26 betas. Correlates with long uptime,
  sample-rate renegotiation, Bluetooth state changes, and specifically Teams/WebRTC
  routing. — `coreaudio-tap-stability.md`
- **`isExclusive = YES` is inverted** from what you'd guess and is the most common cause
  of a self-inflicted silent tap. — `README.md`
- **Mixing/sync:** put the tap *and* the mic in one aggregate device with drift
  compensation; Core Audio then clock-syncs both into a single IOProc callback and you
  never resample by hand. `AVAudioEngine` cannot be retargeted onto a tap-backed
  aggregate — it returns `noErr` and silently keeps reading the default input. —
  `mixing-and-sync.md`
- **Permissions:** ScreenCaptureKit needs *no entitlement at all*, only TCC consent
  (plus `NSScreenCaptureUsageDescription`), and the app must fully quit and relaunch
  after the grant. Mic needs an entitlement *and* TCC *and* a usage string. TCC is keyed
  to code-signing identity, so unsigned builds are never even prompted — ad-hoc sign
  (`codesign -s -`) for local dev. — `signing-entitlements-distribution.md`
- BlackHole path details, and the Multi-Output vs Aggregate device distinction (they are
  opposites and constantly confused). — `virtual-device-fallback.md`
- OBS reference study: ScreenCaptureKit for system/per-app audio, classic Core Audio HAL
  for mic, and explicitly *not* `AudioHardwareCreateProcessTap`. — `how-obs-does-it.md`
- Bibliography with source-quality ratings. — `sources.md`

## 2. Transcription — `research/audio-transcription-pipeline/`

Question: turn the recorded 4-channel CAF into speaker-labelled text, automatically, on
a schedule. Assumed input contract: `audio.caf` (4ch, 48kHz, float32 — ch0/1 = system,
ch2/3 = mic) plus a `meta.json` sidecar.

- **Engine choice:** `parakeet-mlx` + `parakeet-tdt-0.6b-v3` as default — roughly 50-65x
  realtime, ~6.3% WER (beats Whisper large-v3 at 7.44%). `mlx-whisper` +
  `large-v3-turbo` as the multilingual fallback. **Avoid `faster-whisper` on Mac** — it's
  CTranslate2, CPU-only, no Metal backend, ~7x slower. — `engines.md`
- **Speaker separation is free.** The fixed channel map gives "me vs them" with no
  diarization model: downmix ch0/1 and ch2/3 to mono, transcribe each independently,
  merge segments by timestamp (shared clock, no offset). Diarization is a v2 concern and
  only on the system channel; if you do it, pyannote 4.x must run on CPU — MPS returns
  wrong timestamps (pyannote issue #1337). Watch for far-end echo bleeding into the mic
  channel. — `channels-and-diarization.md`
- **Chunking:** transcription needs no pre-chunking (Whisper-family runtimes do sliding
  long-form internally, and naive cuts hurt WER at boundaries). The *recorder* does need
  to segment if it's always-on — VAD/silence-aligned with a hard-cap force-cut, gated on
  actual speech. — `automation.md`
- **Scheduling:** launchd LaunchAgent, `StartInterval` 60-300s, scan-and-process. The
  transcript file is its own done-marker — no database. Atomic writes via `tempfile` +
  `os.replace()`. `QueueDirectories` was evaluated and rejected. — `automation.md`
- **Output:** JSON is the durable source of truth stored next to the audio (keeps
  `segments`, `words`, `avg_logprob`, `no_speech_prob`); SRT/VTT/TSV/TXT are all lossy
  derivations. Render Markdown for humans. Build-vs-buy verdict: thin wrapper, don't
  adopt a GUI app (MacWhisper has the best watched-folder story and still leaves you
  polling its output). — `tooling-and-output.md`

## 3. Adjacent research

- **`research/local-tts-macos/`** — offline speech *output*. `pyttsx3` for zero effort;
  **Kokoro-82M via MLX** (~170MB) for quality; RealtimeTTS for low-latency streaming.
  License trap: espeak-ng is GPL-2.0 and hides as a dependency inside Kokoro, StyleTTS2,
  Piper, and Coqui.
- **`research/interstate-recording-consent/`** — legal, and the answer is **no**:
  standing in a one-party-consent state does not immunize you from an all-party state's
  law. Two of the three choice-of-law rules courts use favor the person being recorded;
  *Kearney v. Salomon Smith Barney*, 39 Cal.4th 95 (2006) applied California's all-party
  law to a Georgia recorder. Federal 18 U.S.C. § 2511 is a floor, not a ceiling. 13
  all-party states enumerated. Relevant the moment meetrs records anyone but you. Not
  legal advice.
- **`research/ec2-mac-mlx/`** — if inference ever moves off the laptop: only
  `mac2.metal` runs MLX (M1, 16GB, $0.65/hr, On-Demand only, no Spot/Reserved). ~13GB
  usable after OS overhead, so INT4 7-12B models. A T4 is cheaper and ~4x the throughput
  for anything not latency-bound.

## 4. Related projects in deskwork

- `projects/robo-notes/` — proposed; the intended consumer of §1 and §2. Closest prior
  art to meetrs.
- `projects/voicetest/` — active. `NSSpeechSynthesizer` via PyObjC, Haiku summarizes then
  speaks. Has acronym pronunciation fixes worth stealing (reads "CLI" as "C-L-I").
- `projects/hey-kerry/` — proposed TTS alerting daemon.
- `standups/` and `todo.yaml` — existing Teams-transcript ingest via Graph API, plus two
  open items on missing/lagging standup transcriptions.

Nothing audio-related is tracked in `research/research_status.yaml` — that's a separate
sweep.
