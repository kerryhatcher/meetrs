# quill — prior art teardown

`quill` is a working, shipped implementation of roughly what meetrs wants to be, in
1841 lines of Swift for macOS only. Source: `github.com/digimata/quill`, read at commit
`855869e` (2026-08-03). A local copy is at `~/projects/quill/`.

This is a mechanism teardown, not an endorsement — the point is to steal the decisions
that were paid for in debugging and to know which ones don't survive the port to Rust.
Everything below was read from source, not observed at runtime; nothing here was
benchmarked or reproduced locally.

Third-party provenance note: quill came from an untrusted source and was security-audited
on 2026-08-03. It behaves as documented — no network code in its own sources, no
telemetry, capture only on explicit click. The one sharp edge is the `on_stop` config
hook (see §7).

## 1. Shape

Single Swift binary, no `.app` bundle, menu-bar tray via `NSApplication` with
`.accessory` activation policy. Three subcommands (`run`, `doctor`, `install`) on
`swift-argument-parser`. `run` is the default and is the daemon. Two dependencies total:
argument-parser and FluidAudio.

One click starts a session; one click stops it. A session is a timestamped directory:

```
~/Recordings/2026.08.03-1104/
  mic.caf          AAC mono, your side
  system.caf       AAC, everything the Mac played
  meta.json        timestamps, duration, per-track start offsets
  transcript.json  canonical — engine provenance + speaker-tagged timed segments
  transcript.md    same thing rendered for humans
  transcribe.log   per-session progress/errors
```

Directory-name collisions get a `-2`, `-3` suffix rather than overwriting
(`RecordingSession.swift:26-30`). Worth copying — two meetings in one minute is not
hypothetical.

## 2. Two independent capture graphs, not one aggregate

**This is the most important divergence from our prior research** (see
`audio-recording-and-processing.md` §1, which recommends putting the tap *and* the mic in
a single aggregate device with drift compensation so Core Audio clock-syncs both into one
IOProc callback).

quill does not do that. It runs two entirely separate capture paths:

- **System audio** (`SystemAudioRecorder.swift`) — `AudioHardwareCreateProcessTap` with
  `CATapDescription(stereoGlobalTapButExcludeProcesses: [])`, wrapped in a private
  aggregate device whose sub-device list is *empty* and whose tap list holds only that
  one tap. Buffers arrive via `AudioDeviceCreateIOProcIDWithBlock` on a dedicated
  dispatch queue and are written straight to file.
- **Mic** (`MicRecorder.swift`) — a plain `AVAudioEngine` input-node tap, entirely
  separate object graph, separate file.

The two tracks therefore run on **two clocks** and do not start on the same buffer. quill
reconciles this after the fact rather than in the audio layer: each recorder records the
wall-clock `Date` of its own first buffer (`firstBufferAt`), and on stop `meta.json` gets
a `start_offset_ms` map holding how far each track lagged the earliest one
(`RecordingSession.swift:55-69`). The transcriber then shifts each track's segment
timestamps by its offset before merging (`TranscriptionCoordinator.swift:121-129`).

Tradeoff, stated plainly:

- **What it buys:** total independence. The mic path can be torn down and rebuilt
  mid-session (which it does — see §4) without touching the system tap. No aggregate
  device has to accept both a tap and a physical input. It sidesteps the
  "`AVAudioEngine` cannot be retargeted onto a tap-backed aggregate" problem by never
  trying.
- **What it costs:** no hardware drift compensation between mic and system. A
  one-time wall-clock offset corrects *start* skew, not accumulating drift over a
  90-minute meeting. quill's own comment concedes the offsets are usually "tens of
  milliseconds," which is fine for reading a transcript and would not be fine for
  sample-accurate mixing.

For meetrs this is a real fork in the road: **single aggregate (sample-accurate, one
clock, harder to build, mic and tap fail together) vs. two graphs (loose sync via
timestamps, trivially recoverable, per-track failure isolation).** quill's design implies
that if your output is *text*, loose sync is enough. Note quill's architecture is
*consistent with* the prior finding about retargeting `AVAudioEngine`, but does not
prove it — quill never attempts it.

## 3. Tap configuration details worth copying verbatim

From `SystemAudioRecorder.swift:48-51` and `98-112`:

- `description.isPrivate = true` — the tap doesn't show up as a system-visible device.
- `description.muteBehavior = .unmuted` — capture without muting what the user hears.
  Getting this wrong silences the meeting for the user.
- Aggregate device: `kAudioAggregateDeviceIsPrivateKey: true`,
  `IsStacked: false`, **`TapAutoStart: true`**, empty `SubDeviceList`, and
  `kAudioSubTapDriftCompensationKey: true` on the single sub-tap.
- The tap's stream format is *read back* from the tap
  (`kAudioTapPropertyFormat` via `AudioObjectGetPropertyData`) rather than assumed, then
  used to construct the output file. Do not hardcode 48kHz/stereo.

Teardown order on stop matters and is explicit (`stop()` → `cleanup()`,
`SystemAudioRecorder.swift:72-79,159-173`): `AudioDeviceStop`, then
`AudioDeviceDestroyIOProcID`, then `AudioHardwareDestroyAggregateDevice`, then
`AudioHardwareDestroyProcessTap`. Every handle is reset to `kAudioObjectUnknown` so a
second `stop()` is a no-op. A fresh tap and aggregate are built on the next start —
nothing is kept warm.

**Version floor discrepancy:** `Package.swift` declares `.macOS(.v15)` and the README says
macOS 15+, but the API actually used (`CATapDescription`,
`AudioHardwareCreateProcessTap`) is macOS 14.2+, and the source comment says as much
(`SystemAudioRecorder.swift:6`). The 15 floor looks conservative rather than required.
If meetrs wants Sonoma support, the process-tap path is probably available to it.

## 4. Echo cancellation: the expensive lesson

quill's `.issues/rca-001-voice-processing-silent-mic.md` is a 120-line root-cause analysis
of a silent mic track, and it is the single highest-value document in the repo. Summary:

**Apple's voice processing (AEC) is not an input effect. It is a duplex I/O
configuration.** Calling `setVoiceProcessingEnabled(true)` on the input node silently
reconfigures both I/O nodes. If you then accept the input node's inherited format and
never build the output render path, the unit delivers *correctly timed digital silence* —
a file with the right duration and length at roughly -91 dB. On the affected machine the
inherited format came back as **9 channels at 48 kHz**, and writing those buffers to AAC
failed on every write, leaving a 4 KB zero-frame file.

The working graph (`MicRecorder.swift:119-131`, matching Apple's `UsingVoiceProcessing`
sample) needs four things in order:

1. Enable voice processing while the engine is stopped.
2. Choose **one explicit mono float32 client format** at the Voice I/O boundary — do not
   inherit the route's multichannel format and downmix afterward. A downstream
   `AVAudioConverter` is *below* the unit and cannot repair its I/O configuration.
3. `engine.connect(mainMixerNode, to: outputNode, format: monoFormat)` — the mixer has
   no sources and nothing is monitored or played; the connection exists purely to give
   the duplex unit a formatted output path.
4. Install the input tap with **that same** format.

Two further mitigations quill ships:

- **A first-second liveness check** (`MicRecorder.swift:152-181`). It tracks signal peak
  over the first `sampleRate` frames; if the peak is still exactly `0` when that many
  frames have arrived, it tears the engine down, deletes the partial file, resets
  `firstBufferAt`, and restarts capture raw with voice processing off
  (`fallBackToRaw()`, `:212-232`). This is a direct, empirical answer to the
  **"zero samples" tap bug** from our prior research (§1: a tap can look healthy —
  correct timestamps, correct cadence — and deliver pure `0.0f`). Steal the pattern
  regardless of platform: *never trust that a capture path producing callbacks is
  producing audio.* Verify signal, and have a fallback route.
- **Ducking suppression** (`MicRecorder.swift:78-79`). A live voice-processing unit makes
  macOS treat the session as a phone call and ducks all other audio — meaning the meeting
  you are recording gets quieter the moment you hit record. quill sets
  `voiceProcessingOtherAudioDuckingConfiguration(enableAdvancedDucking: false, duckingLevel: .min)`.
  Its own comment notes this minimizes but cannot zero the ducking.

**And then it defaults the whole feature off** (`Config.swift:56-58`, commit `8ab6ebb`).
Reasoning: the ducking can't be fully eliminated, and on headphones there is no acoustic
echo to cancel anyway. AEC is opt-in for the speakers case only.

The fallback for unsupported routes is proposed as *transcript-level* echo suppression
rather than more DSP: because quill already holds a clean far-end track and aligned
timestamps, mark a mic segment as echo when it overlaps a system segment in time *and*
has high fuzzy token similarity, while preserving segments with substantial unique mic
words so interruptions and double-talk survive. Not implemented; a good idea for meetrs.

## 5. Container and encoding choices

- **CAF, not m4a.** Stated reason, and it's a good one: CAF needs no finalization pass,
  so if the process dies mid-meeting everything already written is still readable. m4a
  needs its moov atom written at close and a killed process yields an unplayable file.
- **AAC inside CAF** (`AVFormatIDKey: kAudioFormatMPEG4AAC`) for both tracks.
- **Mic is forced to mono** — one channel, at the device's native sample rate. Speech
  models want one channel anyway.
- **Buffers stream straight to disk on every callback.** Nothing is buffered in memory
  beyond the encoder, so session length is unbounded and memory is flat. No ring buffer,
  no pre-roll, no "keep the last N seconds."
- The raw (non-AEC) mic path taps at the device's native format and runs one
  `AVAudioConverter` to mono, same sample rate on both sides so it's a one-shot convert
  (`MicRecorder.swift:185-207`).

## 6. Transcription pipeline

Engine: **Parakeet TDT 0.6B v2 (English)** via FluidAudio's Core ML port —
`AsrModels.downloadAndLoad(version: .v2)` then `AsrManager.transcribe(audio:decoderState:)`.
Claimed ~20 seconds per hour of audio on Apple Silicon. Models (~600 MB) download once
into FluidAudio's cache from `huggingface.co`; that endpoint belongs to FluidAudio's
`ModelRegistry` default, not to quill.

Note the version skew vs. our prior research, which recommends
`parakeet-tdt-0.6b-**v3**` (multilingual, ~6.3% WER). quill is on **v2** (English-only).
No stated reason.

Architecture worth copying wholesale:

- **The filesystem is the queue.** `TranscriptionCoordinator.resumePending(root:)` scans
  the recordings root at launch for directories where `meta.json` exists but
  `transcript.json` does not, sorts them by name (the `yyyy.MM.dd-HHmm` format sorts
  chronologically for free), and drains them oldest-first. No database, no state file.
  This is exactly the "transcript file is its own done-marker" conclusion from our prior
  research, arrived at independently.
- **Atomic writes** — `transcript.json` and `transcript.md` are both written with
  `options: .atomic` (temp file + rename), so a partially written transcript never exists
  on disk and the done-marker is never a lie.
- **Lazy model lifecycle.** The `TranscriptionEngine` protocol is
  `prepare()` / `transcribe()` / `release()`. The coordinator instantiates and prepares
  the engine only when the queue has work, and calls `release()` when it drains, so an
  idle daemon isn't holding gigabytes of weights resident. For a daemon that lives in
  the menu bar all day, this matters more than it looks.
- **Failure isolation at three levels.** A missing track is logged and skipped; a track
  that throws during transcription is logged and skipped so the *other* track's
  transcript still gets written; a whole failed session logs to its own `transcribe.log`
  and never blocks later queue entries.
- **Empty-file guard as crash protection.** `ParakeetEngine.transcribe` probes with
  `AVAudioFile(forReading:)` and requires `length > 0` before handing the file to the
  model, because a zero-frame track makes AVFoundation raise an **ObjC exception deep in
  the resampler that is uncatchable from Swift and takes the whole daemon down**
  (`ParakeetEngine.swift:40-51`). A Rust port using a C/ObjC audio framework should
  assume the same class of unwind hazard exists and pre-validate identically.
- **There is a race-closing re-drain**: after the drain loop exits and the engine is
  released, `drainIfIdle()` is called once more, because an enqueue that landed between
  loop exit and release completion would otherwise sit idle until the next session
  (`TranscriptionCoordinator.swift:95-97`).

Segmentation is punctuation-driven, not model-driven (`ParakeetEngine.swift:74-102`):
Parakeet v2 emits punctuation, so quill groups word timings into segments, breaking on
`.`/`?`/`!`, on a silence gap greater than **1.0s**, or at a hard cap of **60 words** so a
run-on speaker still wraps. If no word timings come back at all, it falls back to one
segment spanning the whole file.

**Diarization is free and there is no diarization model.** `mic.caf` → speaker `"me"`,
`system.caf` → speaker `"them"`, assigned by filename in `SessionMeta.read`
(`TranscriptionCoordinator.swift:222-227`). Two-party attribution for zero inference
cost. Matches our prior research §2 exactly. The limitation is inherent: everyone on the
far end is a single speaker called `them` until you add real diarization on the system
track.

Transcript schema — flat, and the right shape:

```json
{ "engine": "parakeet",
  "model": "parakeet-tdt-0.6b-v2-coreml",
  "created_at": "<iso8601>",
  "segments": [ { "speaker": "me", "start_ms": 0, "end_ms": 1400, "text": "..." } ] }
```

Engine and model provenance are recorded in the artifact. Do this — when you re-transcribe
a year of meetings with a better model you will want to know what produced what. Note
quill's JSON keeps *less* than our prior research recommended (no `words`, no
`avg_logprob`, no `no_speech_prob`); if meetrs wants confidence-based re-processing later,
keep the per-word data.

## 7. Config, and the one sharp edge

`~/.config/quill/config.json`, optional. Malformed JSON warns on stderr and is ignored
rather than failing silently — the stated reasoning is that recordings landing somewhere
unexpected is worse than a warning.

| key | default | effect |
|---|---|---|
| `recordings_dir` | `~/Recordings` | session root; `--out` flag overrides it |
| `transcription.enabled` | `true` | gate on auto-transcribe |
| `transcription.engine` | `"parakeet"` | dead knob — anything else warns and still gets Parakeet |
| `mic_voice_processing` | `false` | Apple AEC on the mic (see §4) |
| `on_stop` | unset | **shell command**, run with the session dir as its argument |

`on_stop` runs `/bin/sh -c "<cmd> \"$0\"" <session-dir>` via `Process()`
(`TranscriptionCoordinator.swift:160-170`), after the transcript is written — or
immediately after recording if transcription is disabled. It is documented as the
extension point for summarization, filing, and indexing.

It is also the only mechanism in quill by which data can leave the machine, which means
the "nothing ever leaves the machine" guarantee is really a guarantee about
`~/.config/quill/config.json`. Anything on the box that can write that file turns the
recorder into an exfiltration tool with a one-line edit. **If meetrs ships a post-session
hook, that's the design question to answer up front** — an allowlist of named actions, or
a hook binary in a fixed directory, is a materially different security posture than an
arbitrary shell string, for the same ergonomics.

Note the deliberate ordering detail: with transcription disabled the hook still fires, it
just receives an untranscribed folder. The hook contract is "a session finished," not "a
transcript exists."

## 8. Daemon, permissions, packaging

- `quill install` writes exactly one file — `~/Library/LaunchAgents/com.digimata.quill.plist`
  — then `launchctl bootout` + `bootstrap` into `gui/<uid>`. `RunAtLoad: true`,
  `KeepAlive: {SuccessfulExit: false}`, `ProcessType: Interactive`, stdout/stderr to
  `/tmp/quill.{out,err}.log`. `--uninstall` reverses it completely. User-level agent, no
  daemon, no root.
- **TCC without an app bundle.** This is the packaging trick meetrs will need. A bare
  executable has no `Info.plist`, so TCC can't attribute the mic/system-audio permission
  to it. quill embeds the plist into the Mach-O `__TEXT,__info_plist` section at link
  time (`Package.swift:19-29`):
  `-sectcreate __TEXT __info_plist Sources/quill/Info.plist`. Legitimate documented Apple
  technique. The plist itself declares only `CFBundleIdentifier`, `CFBundleName`,
  `NSMicrophoneUsageDescription`, `NSAudioCaptureUsageDescription`. No entitlements file
  exists in the repo at all.
  - Corollary from our prior research that still applies: **TCC is keyed to code-signing
    identity, so unsigned builds are never even prompted.** Ad-hoc sign (`codesign -s -`)
    for local dev, and expect the permission grant to reset whenever the identity changes.
- **Notifications via `osascript`** — `Process()` launching
  `/usr/bin/osascript -e 'display notification ...'` (`Notify.swift`), specifically to
  avoid needing the `UserNotifications` entitlement. Cheap and effective; it also means
  no notification actions or grouping.
- `quill doctor` checks mic TCC status, recordings-folder writability, and **whether the
  ~600 MB models are already cached** — the stated point being that you never discover a
  model download is pending right before an important meeting. Cheap, high-value. Note it
  cannot query system-audio TCC state without side effects, so it prints a static warning
  for that one.
- Startup checks are fatal (`ExitCode(1)`) but permissions themselves prompt lazily on
  first recording, so a fresh install doesn't need to be run interactively first.
- SIGINT is trapped so `^C` stops a live session cleanly — files finalized, `meta.json`
  written — before terminating.

## 9. What does not survive the port to Rust

| quill mechanism | Rust / cross-platform reality |
|---|---|
| Core Audio process tap (`AudioHardwareCreateProcessTap`) | macOS 14.2+ only. Reachable from Rust via `coreaudio-sys`/`objc2` bindings, but it's raw FFI — no crate wraps `CATapDescription` yet as far as this teardown checked (unverified). Windows needs WASAPI loopback; Linux needs a PulseAudio/PipeWire monitor source. Three separate implementations behind one trait. |
| `AVAudioEngine` mic tap | `cpal` covers mic capture on all three platforms. Straightforward. |
| Apple voice processing (AEC) | **No portable equivalent.** This is a macOS framework feature. Rust options are `webrtc-audio-processing` (AEC3, the real answer, C++ bindings) or `speexdsp`. Note that a portable AEC needs the far-end reference signal explicitly fed to it — which quill's two-track design already gives you for free. Arguably *easier* in meetrs than in quill. |
| FluidAudio / Core ML Parakeet | Swift-only, Apple-only. Rust paths: `sherpa-onnx` (has Parakeet TDT ONNX exports and Rust bindings), `ort` (ONNX Runtime) directly, or `whisper-rs` if you accept Whisper's worse WER. Cross-platform ASR is the single biggest port risk — budget real time here. |
| AAC-in-CAF via `AVAudioFile` | CAF is an Apple container. Portable equivalent with the same crash-safety property: **WAV is not it** (header holds sizes), but raw/streaming Ogg+Opus or a headerless PCM file plus sidecar metadata is. Opus is the better call anyway for speech at these bitrates. Keep the "readable if the process is killed" requirement — it's the reason for the choice, not the container itself. |
| `osascript` notifications | `notify-rust` covers Linux/Windows; macOS support is limited. Tray + notification both point at `tray-icon` / `muda` or `tauri` if you want a real UI later. |
| launchd LaunchAgent | Three implementations: launchd plist, systemd user unit, Windows Task Scheduler / registry Run key. |
| `__TEXT,__info_plist` linker trick | Still needed for an unbundled Rust binary on macOS; pass the same `-sectcreate` flags through `build.rs` / `RUSTFLAGS`. Irrelevant elsewhere. |

## 10. Decisions to carry over

Ranked by how much debugging they represent:

1. **Verify signal, don't trust callbacks.** The first-second liveness check with a
   fallback route. Both our prior research and quill's RCA independently hit silent-but-
   healthy capture paths.
2. **One explicit mono format at the AEC boundary**, and an AEC that is a duplex graph
   with a rendered output path, not an input filter. Whatever library provides AEC, expect
   this shape.
3. **Two tracks, and diarization for free.** `me` / `them` by filename beats a
   diarization model for two-party calls.
4. **Filesystem as queue; the output artifact is the done-marker; atomic writes.** No
   database. Resume by rescanning.
5. **A container that survives `kill -9` mid-write**, and buffers that stream to disk so
   session length is unbounded.
6. **Lazy model load and explicit release** when the queue drains — a menu-bar daemon
   should idle cheap.
7. **Per-track and per-session failure isolation.** One bad track never costs you the
   other; one bad session never blocks the queue.
8. **Record engine and model provenance in the transcript.**
9. **A `doctor` that checks whether the model cache is warm**, not just permissions.
10. **Pre-validate audio files before handing them to the model** — a zero-frame file
    crashed the daemon from inside a framework, uncatchable.

Open questions this teardown does not answer: whether loose timestamp sync holds up over
a 90-minute meeting (quill assumes it does and never measures drift), whether Parakeet v3
should replace v2, and what the per-platform system-audio story costs to build once the
macOS path is done.
