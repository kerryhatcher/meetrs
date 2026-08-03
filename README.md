# meetrs

A terminal meeting recorder. Captures your microphone and your system audio into
one synchronized stream, and writes WAV chunks split on natural pauses so a crash
costs at most one chunk.

**Recording only.** Transcription is deliberately out of scope — see
[`docs/research/`](docs/research/) for the research backing what comes next.

**Status: proof of concept.** macOS only. It does not delete or groom anything,
so it will happily fill your disk.

## Install

macOS 14.4+ (that floor is a Core Audio process-tap requirement, not arbitrary —
see [`docs/research/rust-audio-macos.md`](docs/research/rust-audio-macos.md)).

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

```
q / Esc / Ctrl-C   stop recording and flush
```

Recordings land in `~/.meetrs/recordings/<timestamp>/`:

```
~/.meetrs/recordings/2026-08-03T16-04-22/
  chunk-000.wav      48kHz 32-bit float, mic + system interleaved
  chunk-001.wav
  meta.json          channel map, threshold, per-chunk offsets
```

Each chunk is closed and `fsync`ed before the next opens, so the only audio at
risk from a crash is the chunk in flight.

## Calibrate the silence threshold first

Chunking cuts on RMS falling below a threshold, and that threshold has to sit
above your machine's noise floor and below speech. That gap is hardware- and
environment-specific, so **measure it before trusting the chunking**:

```sh
# with nothing playing and the room quiet
./target/debug/meetrs.app/Contents/MacOS/meetrs --check
```

```
channels=3 rate=48000 mic=(0, 0) system=(1, 2)
frames=237056 dropped=0
mic:    rms=0.009804 peak=0.090197
system: rms=0.044603 peak=0.578206
```

Pick a threshold above both idle RMS figures but below your speech level, and set
it:

```sh
MEETRS_SILENCE_RMS=0.06 ./target/debug/meetrs.app/Contents/MacOS/meetrs
```

If the threshold is too low, nothing ever reads as silence and you get one
enormous chunk — which defeats the entire point of chunking. On the author's
machine the built-in default of 0.004 did exactly that, because ambient mic noise
alone measured 0.0105. The default is now 0.02, which is still too low if anything
is playing audio in the background.

`--check` is also the way to detect the process-tap zero-samples bug: if the
system leg reports bit-exact zero while audio is audibly playing, that's the bug,
not quiet audio.

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

Capture builds a single aggregate device containing both a global process tap and
the default input device, with drift compensation on both legs, and installs one
IOProc on it. One IOProc means Core Audio does the clock sync — the alternative,
two independent streams reconciled by timestamp, means owning drift correction
yourself.

## Known limitations

- **macOS only.** `docs/research/rust-audio-linux.md` has the PipeWire plan.
- **No grooming.** Nothing is ever deleted. This will fill your disk.
- **Silence detection is bare RMS**, not a real VAD. It cuts on quiet, not on
  absence-of-speech, so a noisy room or any background audio defeats it and you
  get one huge chunk. This is the weakest part of the POC and the reason
  `--check` exists. `docs/research/rust-audio-processing.md` covers replacing it
  with Silero VAD, which would remove the calibration step entirely.
- **The threshold is global, not per-leg.** Background system audio holds chunks
  open even when nobody is speaking, because the cut needs *both* legs quiet.
  Per-leg thresholds would be a better model.
- **The zero-samples bug is not fully ruled out.** Core Audio process taps have a
  documented failure mode where the tap looks healthy and delivers pure silence.
  If a recording comes back quiet, that's the first suspect — details and the
  three known root causes are in `docs/research/rust-audio-macos.md`.

## License

MIT
