# meetrs

A terminal meeting recorder. Captures your microphone and your system audio into
one synchronized stream, and writes WAV chunks split on natural pauses so a crash
costs at most one chunk.

Transcription runs locally as recording proceeds — the first chunk starts
transcribing while the second is still being recorded. Nothing leaves the machine.

**Status: proof of concept.** macOS and Linux (PipeWire). It does not delete or
groom anything, so it will happily fill your disk.

## Install

### macOS

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
without telling you why.

TCC also keys consent to the *code-signing identity*, and ad-hoc signing has no
certificate to key to — the grant gets pinned to that exact build instead, so
every rebuild silently revokes it (`Failed to match existing code requirement` in
tccd's log). One self-signed identity, created once, fixes that permanently:

```sh
just signing-cert   # stays on this machine; then re-run: just install
```

Skipping it costs nothing but convenience: capture still works, you just
re-approve the prompts after every rebuild.

On first run, macOS will ask for microphone and system-audio-recording consent.
Grant both, then **fully quit and relaunch** — a screen/audio-recording grant does
not apply to an already-running process.

### Linux

Tested on Fedora (Bluefin) with PipeWire 1.6 and pipewire-pulse running, and an
AMD GPU using the RADV Vulkan driver. Other setups have not been tested.

Build dependencies (Fedora package names):

```sh
sudo dnf install clang clang-devel cmake gcc gcc-c++ pkgconf-pkg-config \
  openssl-devel pipewire-devel
```

- `clang`, `clang-devel`: `bindgen` generates the PipeWire and whisper.cpp bindings.
- `cmake`, `gcc`, `gcc-c++`: whisper.cpp is compiled from source.
- `pkgconf-pkg-config`, `pipewire-devel`: PipeWire is found through pkg-config.
- `openssl-devel`: TLS (for the model download) goes through OpenSSL.
GPU backends are optional. Without one, whisper.cpp runs on the CPU and nothing
GPU-related is needed to build.

- Vulkan (any Vulkan GPU, tested on AMD): `sudo dnf install vulkan-headers
  vulkan-loader-devel glslc`.
- CUDA (NVIDIA): the CUDA toolkit is not in Fedora's repositories. Add NVIDIA's
  Fedora repository and install the compiler and libraries:

  ```sh
  sudo dnf config-manager addrepo --from-repofile=https://developer.download.nvidia.com/compute/cuda/repos/fedora44/x86_64/cuda-fedora44.repo
  sudo dnf install cuda-nvcc-13-5 cuda-cudart-devel-13-5 libcublas-devel-13-5
  ```

On an immutable image such as Bluefin the host filesystem is read-only, so do the
build inside a distrobox or toolbox.

Runtime dependencies:

- PipeWire. Startup fails with an error if no default sink or source appears
  within 5 seconds.
- For Vulkan: a Vulkan loader (`vulkan-loader`) and a driver, such as
  `mesa-vulkan-drivers` for AMD. If Vulkan cannot start, transcription runs on
  the CPU instead (see below for the measured cost).
- For CUDA: the NVIDIA driver (tested with 615.78.08).

Install and run:

Pick the GPU backend at build time. A binary built without a backend always runs
on the CPU:

```sh
just run                          # CPU only
MEETRS_FEATURES=vulkan just run   # Vulkan GPU (AMD, Intel, NVIDIA)
MEETRS_FEATURES=cuda just run     # NVIDIA GPU via CUDA
```

Changing `MEETRS_FEATURES` rebuilds the binary, since the build records which
features it was compiled with.

Without `just`, the same builds are plain cargo feature flags:

```sh
cargo build --release                      # CPU only
cargo build --release --features cuda      # or --features vulkan
./target/release/meetrs
```

The binary is just `target/release/meetrs`. There is no `.app` bundle and no TCC
or consent step on Linux. `just build`, `just run` and `just install` work too:
`just run` injects the R2 credentials from SOPS as on macOS, and `just install`
copies the binary to `~/.local/bin` (set `MEETRS_BINDIR` to change that).

Finished sessions are compressed to FLAC in-process, so no external encoder is
needed.

#### Linux limitations

- **Weaker clock sync than macOS.** macOS runs both legs from one Core Audio
  IOProc, so the device keeps them sample-accurate. Here the mic and system legs
  are separate PipeWire streams, aligned only by the PipeWire graph clock. Pairing
  is by arrival order, so the legs can sit a few milliseconds apart, and that
  offset can vary.
- **Stalls and drift show up as silence.** A leg that runs more than 50 ms ahead
  of the other is buffered. Past that, the lagging leg is zero-filled for the whole
  backlog, so a stall or steady drift becomes a gap of up to about 50 ms on that
  leg. Samples are not lost or reordered.
- **A stream that dies after start is silent, with no notice.** Stream errors are
  only reported during startup. After that, that leg simply stops producing audio.
- **Default-device changes are not followed.** The streams stay on the sink and
  source that were default when meetrs started. Restart meetrs to pick up a new
  default.
- **Fixed format.** Both streams are requested as 48 kHz F32LE, and PipeWire's
  adapter converts from the device's own format.
- **Speakers leak into the mic.** The system leg is the default sink's monitor and
  the mic is the default source. Without headphones, the mic also picks up the
  playback, so the same speech can appear on both legs and be transcribed twice.
  Use headphones.

#### Transcription backend on Linux

Measured on one machine (Ryzen 7 6800U, Radeon 680M, RADV), with the `small.en-tdrz`
model on a 76 s clip: median of 9 passes, 15 threads as the worker uses.

| Backend | Wall time | RTF | CPU-seconds per pass |
|---|---|---|---|
| CPU | 31.1 s | 0.41 | 442 |
| Vulkan (RADV) | 26.2 s | 0.34 | 13 |

Vulkan is faster in wall time and uses about 34 times less CPU per pass. That
matters because transcription shares cores with capture and the TUI. Transcripts
differ by a few words between backends (the CPU run renders "Priya" as "Preer", the
Vulkan run drops the name), and each backend gives the same output run to run.

If no Vulkan driver is available (`VK_ICD_FILENAMES=/nonexistent` simulates this),
whisper.cpp logs `no GPU found` and transcribes on the CPU. In that run the RTF was
0.44, and the run still succeeded.

## Transcription model

First run downloads the transcription model (`ggml-small.en-tdrz.bin`, 465 MiB)
to `~/.meetrs/models/`. It is the tinydiarize fine-tune of `small.en` — the only
published Whisper model that marks speaker turns, which is why it is unquantized
and eight times the size of a plain `base.en`. A speaker turn shows up in a
transcript as a `T` number after the leg, e.g. `system/T3`: tinydiarize flags
where the speaker changed between two segments, so segments sharing a number
are believed to be the same speaker and a higher number is a different speaker
from the one before it (more on exactly what that does and doesn't claim below,
under `transcript.md`). Set `MEETRS_MODEL` to use a model you already have;
transcripts then simply carry no `T` numbers.

Upgrading from an earlier version leaves the superseded `ggml-base.en-q5_1.bin`
in that directory. meetrs grooms nothing, so delete it yourself if you want the
57 MiB back.

## Use

```sh
just run                          # build (and bundle, on macOS), launch the TUI
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

A recording left running doesn't pile everything into one log. A session ends at
local midnight, or after 30 minutes with no speech, and the next session starts
the moment speech is detected again. It gets its own directory named for that
moment (`2026-08-04T08-12-40`), its own `transcript.md`, and chunk numbers and
timestamps starting from zero; the full-text index treats it as a separate
session. While it waits, the header reads `idle, waiting for speech`. A session
that captured nothing (you started the recorder long before the meeting) is
discarded rather than left as an empty directory. Midnight with nobody talking
therefore leaves nothing for the new day until someone does, and a Mac that slept
through the gap starts the next session when audio resumes.

State and a full-text index live in a SQLite database at `~/.meetrs/meetrs.db`.

`transcript.md` looks like this:

```
**[00:00:00] system/T1:** The quick brown fox jumps over the lazy dog.

**[00:00:09] system/T2:** Meeting notes for the third of August about audio capture.
```

The mic and system legs are transcribed separately, but a leg is an *acoustic
path*, not a person — in an in-person meeting there is no system audio at all
and everyone arrives on the mic leg, so "leg" is never a stand-in for "speaker".
Turn numbers only ever go up within a leg, never repeat or alternate: tinydiarize
can tell you turn *N* differs from turn *N+1*, not whether turn 3 returns to
turn 1's speaker, so `T1, T2, T3…` is the strongest claim the signal supports.

T numbers also restart at `T1` in every chunk (chunks split on silence, roughly
every few minutes) and are scoped to one leg, so `system/T1` in one chunk is
not the same person as `system/T1` in the next, or as `mic/T1` in the same
chunk. They identify a turn, never a specific person.

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
the VAD fails to initialize. It has no effect on normal operation. The same
setting can live in the config file below as `silence_rms`.

## Configuration

meetrs reads `~/.meetrs/config.yaml`. The first run creates it with every key
commented out, so the built-in defaults apply until you uncomment one. meetrs
never rewrites the file after that, so your edits survive upgrades. An unknown
key or a malformed value stops startup and names the file and the problem.

| Key | Default | Effect |
|---|---|---|
| `silence_rms` | `0.02` | RMS threshold for the fallback path only (see above). Overridden by `MEETRS_SILENCE_RMS`. |
| `max_chunk_secs` | `150` | Seconds of continuous speech before a chunk is closed. Minimum 10. |
| `max_chunk_bytes` | `99000000` | Largest a chunk's WAV may grow, and the largest file the uploader sends. Minimum 10000000. |

The two chunk limits are written to the file on startup with their defaults, if
the file does not already set them, so they show up as options.

### Uploading to Cloudflare R2

Session files can be uploaded to an R2 bucket. Every `interval_secs` (default
300) whatever changed is sent, so a crash loses at most that much. A final pass
runs when recording stops, after transcription and compression. Uploads run on
their own thread and never delay recording. It is off by default. To turn it on, set `upload.r2` in the
config file:

```yaml
upload:
  r2:
    enabled: true
    account_id: <your Cloudflare account id>
    bucket: hatcher-meetrs
    prefix: meetings/        # optional; keys become meetings/<session>/<file>
    interval_secs: 300       # optional; how often changed files are sent (min 30)
```

The access key stays out of the file. It goes in the environment, and `just run`
injects it from a SOPS-encrypted env file, `~/secrets/meetrs.env` by default
(override with `MEETRS_SECRETS`):

```sh
sops ~/secrets/meetrs.env   # add MEETRS_R2_ACCESS_KEY_ID and MEETRS_R2_SECRET_ACCESS_KEY
just run
```

The variables live only in meetrs' process. `MEETRS_R2_SESSION_TOKEN` is also
read, for temporary credentials. Run `meetrs` directly and the same variables
must be set in the environment yourself.

If `enabled` is true and a credential is missing, meetrs prints a warning and
records without uploading. A bad value in the config file still stops startup. A failed upload is reported and retried on the next pass. The files stay
on disk, and uploads overwrite the same keys, so retrying is safe.

Compressed sessions leave their original WAV objects in the bucket next to the
FLAC files, because the final pass does not delete anything.

## How chunking works

A chunk ends after `SILENCE_TO_CUT` (2s) of continuous audio below the RMS
threshold, or after `max_chunk_secs` (2.5 min) of continuous speech, or before its
WAV would pass `max_chunk_bytes` (99 MB). The size cut keeps every chunk, and the FLAC made from
it, under the 99 MB limit of the planned upload backend — a monologue still bounds crash
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
| `src/capture/macos.rs` | Core Audio process tap + mic in one aggregate device, one IOProc |
| `src/capture/linux.rs` | PipeWire: mic and default-sink monitor as two streams |
| `src/chunk.rs` | Silence state machine, WAV writing, `meta.json` |
| `src/ui.rs` | Ratatui level meters and chunk list |
| `src/db.rs` | SQLite state + FTS5 index (derived, rebuildable) |

On macOS, capture builds a single aggregate device containing both a global process tap and
the default input device, with drift compensation on both legs, and installs one
IOProc on it. One IOProc means Core Audio does the clock sync — the alternative,
two independent streams reconciled by timestamp, means owning drift correction
yourself.

## Known limitations

- **macOS was not recompiled in the Linux port.** The macOS code is gated by
  `cfg`, and the Linux changes were built and tested on Linux only. Linux
  limitations are listed under [Linux](#linux-limitations).
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
