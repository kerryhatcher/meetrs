# meetrs

A terminal meeting recorder. Captures your microphone and your system audio into
one synchronized stream, and writes WAV chunks split on natural pauses so a crash
costs at most one chunk.

Transcription runs locally as recording proceeds — the first chunk starts
transcribing while the second is still being recorded. Nothing leaves the machine.

**Status: proof of concept.** macOS only. It does not delete or groom anything,
so it will happily fill your disk.

## Install

macOS 14.4+ (that floor is a Core Audio process-tap requirement, not arbitrary —
see `docs/research/rust-audio-macos.md`, kept in git history rather than the
working tree: `git show 3212d62:docs/research/rust-audio-macos.md`).

```sh
cargo build --release
./scripts/bundle.sh release
./target/release/meetrs.app/Contents/MacOS/meetrs
```

The `.app` wrapper is not optional. macOS gates audio capture behind TCC consent,
consent requires the `NSAudioCaptureUsageDescription` and
`NSMicrophoneUsageDescription` keys in an `Info.plist`, and a bare binary has no
`Info.plist` — so an unbundled build is never even prompted, and capture fails
without telling you why. TCC also keys consent to the code-signing identity, so
`bundle.sh` ad-hoc signs the result.

On first run, macOS will ask for microphone and system-audio-recording consent.
Grant both, then **fully quit and relaunch** — a screen/audio-recording grant does
not apply to an already-running process.

## Use

```sh
just run                          # build, bundle if needed, launch the TUI
just check                        # headless: channel layout and per-leg levels
just search "budget OR headcount" # full-text search every transcript
just --list                       # everything else
```

```
q / Esc / Ctrl-C   stop recording and flush
```

Only one instance can run at a time. The second one exits immediately with
`meetrs is already running`, enforced by an `flock` on `~/.meetrs/meetrs.lock`
held by the process itself — so it holds however you launch it, not just through
`just`. The kernel drops the lock when the process dies, including on `SIGKILL`,
so there is no stale lock to clean up.

Recordings land in `~/.meetrs/recordings/<timestamp>/`:

```
~/.meetrs/recordings/2026-08-03T16-04-22/
  chunk-000.wav      48kHz 32-bit float, mic + system interleaved
  chunk-000.json     segments, session-relative times, per-leg labels
  chunk-001.wav
  chunk-001.json
  meta.json          channel map, detector, per-chunk offsets
  transcript.md      speaker-labelled, human-readable
```

State and a full-text index live in a SQLite database at `~/.meetrs/meetrs.db`.

`transcript.md` looks like this — the mic and system legs are transcribed
separately, which gives speaker attribution with no diarization model:

```
**[00:00:00] system:** The quick brown fox jumps over the lazy dog.

**[00:00:09] system:** Meeting notes for the third of August about audio capture.
```

Each chunk is closed and `fsync`ed before the next opens, so the only audio at
risk from a crash is the chunk in flight.

## Voice activity detection

Chunk boundaries come from a VAD (`earshot`, a small pure-Rust neural model with
embedded weights — no ONNX runtime, no model download). The mic and system legs
are evaluated independently and a chunk stays open while *either* leg has speech.

This replaced an RMS threshold, which needed per-machine calibration and was the
POC's worst edge. For the record of why: ambient mic noise on the author's machine
measured 0.0105 RMS, which silently defeated the original 0.004 threshold — every
chunk stayed open forever because the empty room read as sound.

`--check` (or `just check`) still earns its keep as a diagnostic:

```
channels=3 rate=48000 mic=(0, 0) system=(1, 2)
frames=237056 dropped=0
mic:    rms=0.009804 peak=0.090197
system: rms=0.044603 peak=0.578206
```

Use it to confirm both legs carry signal, and to catch the process-tap
zero-samples bug — if the system leg reports bit-exact zero while audio is audibly
playing, that's the bug, not a quiet room.

`MEETRS_SILENCE_RMS` still exists but only feeds the RMS fallback path, used if
the VAD fails to initialize. It has no effect on normal operation.

## How chunking works

A chunk ends after `SILENCE_TO_CUT` (2s) of continuous audio below the RMS
threshold, or at `MAX_CHUNK` (5min) regardless — a monologue still bounds crash
loss. Chunks under `MIN_CHUNK` (750ms) are discarded rather than written, so a
stray cough between two pauses doesn't leave a file behind.

A short pre-roll buffer keeps ~200ms *before* speech onset so the first syllable
isn't clipped, and trailing silence is trimmed to ~300ms rather than written in
full. Those two buffers are the only place audio is held back from disk. All the
thresholds live in [`src/types.rs`](src/types.rs).

## Layout

| File | Role |
|---|---|
| `src/types.rs` | Shared contracts and tuning constants |
| `src/capture.rs` | Core Audio process tap + mic in one aggregate device, one IOProc |
| `src/chunk.rs` | Silence state machine, WAV writing, `meta.json` |
| `src/ui.rs` | Ratatui level meters and chunk list |
| `src/db.rs` | SQLite state + FTS5 index (derived, rebuildable) |

Capture builds a single aggregate device containing both a global process tap and
the default input device, with drift compensation on both legs, and installs one
IOProc on it. One IOProc means Core Audio does the clock sync — the alternative,
two independent streams reconciled by timestamp, means owning drift correction
yourself.

## Known limitations

- **macOS only.** The PipeWire plan is in `docs/research/rust-audio-linux.md`,
  in git history (`git show 3212d62:docs/research/rust-audio-linux.md`).
- **No grooming.** Nothing is ever deleted. This will fill your disk.
- **Continuous system audio suppresses chunk splits.** A chunk stays open while
  either leg has speech, so background music or a playing video holds one chunk
  open until the 5-minute hard cap. Observed directly: two runs of the same test
  produced 2 chunks and 1 chunk depending on whether background audio happened to
  be playing. The hard cap still bounds crash loss, so this is a defensible
  design rather than a defect — but if you want tighter chunks during a
  screen-share, the cut rule needs to weight the mic leg over the system leg.
- **Transcript polish is deterministic, not a language model.**
  [`crustytts-sentence`](https://github.com/kerryhatcher/crustytts) restores
  capitalization and terminal punctuation in `transcript.md`, which helps the
  short fragments whisper emits without punctuation. It does not attempt to fix
  transcription errors. `chunk-NNN.json` keeps whisper's raw output either way.
- **Observed `base.en` error modes**, from real output: dropped word boundaries
  (`QuickBrown`), substituted onsets (`Fox` -> `Thox`), and dropped initial
  consonants (`brand` -> `rand`). Several of those are non-words, so a spell
  checker could plausibly help — worth testing rather than assuming either way.
  A larger model is the more direct fix.
- **Don't evaluate accuracy with synthesized speech.** macOS `say`'s default
  voice mispronounces some words (it renders "brown" close to "brand"), so a
  wrong transcript can be the test harness rather than the recognizer. Pin a
  named voice (`say -v Alex`) when generating test audio, or use real speech.
- **VAD frames are decimated 48k to 16k with a 3-sample box filter**, not a
  proper anti-alias filter. Cheap and adequate here; `rubato` is the upgrade if
  aliasing ever shows up as false speech.
- **The zero-samples bug is not fully ruled out.** Core Audio process taps have a
  documented failure mode where the tap looks healthy and delivers pure silence.
  If a recording comes back quiet, that's the first suspect — details and the
  three known root causes are in `docs/research/rust-audio-macos.md`, in git
  history (`git show 3212d62:docs/research/rust-audio-macos.md`).

## License

MIT
