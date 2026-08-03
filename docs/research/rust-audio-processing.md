# Rust audio ecosystem for meetrs — capture, codecs, DSP, VAD (cross-platform layer)

Scope: the platform-agnostic crates layer only. macOS-specific capture APIs (ScreenCaptureKit,
Core Audio process taps) are covered in `docs/research/audio-recording-and-processing.md` →
`macos-audio-capture`; Linux-specific capture (PulseAudio/PipeWire monitor sources, ALSA) is a
sibling agent's territory. This doc covers the crates you'd use regardless of OS: device I/O
abstraction, decode/encode, resampling, DSP/metering, VAD, lock-free buffers, and the
callback→async bridge.

All version/download/license/activity numbers below were pulled live from the crates.io API and
GitHub API on 2026-08-03 (see Sources). Anything not directly confirmed from a fetched source is
marked `[unverified]`.

## Recommendation

- **Capture I/O:** `cpal` 0.18 — it's the only real cross-platform option and is actively
  maintained. Its new CoreAudio loopback support (added 0.17.0, hardened in 0.18.0) is confirmed
  real via the CHANGELOG and the originating PRs — it wraps the same Core Audio process-tap
  mechanism the macOS research hand-rolls, not a ScreenCaptureKit path (a separate
  `ScreenCaptureKit loopback` PR, #894, remains open/unmerged). Treat it as promising but young:
  keep the hand-rolled Core Audio tap path from the macOS research as the fallback until 0.18's
  loopback fixes (UID collisions, silent-tap bug) prove stable in the field.
- **Decode:** `symphonia` for WAV/CAF/FLAC/AAC/ALAC/Ogg-Vorbis/MP3 container+codec decode
  (pure Rust, no system deps, `caf` feature flag covers the macOS 4-channel CAF file directly).
- **Encode/write raw PCM out:** `hound` for WAV (you likely don't need an encoder at all if you
  keep the durable artifact as WAV or CAF-via-symphonia's writer path — check if you actually
  need lossless compression before adding FLAC).
- **Skip ffmpeg bindings** (`ffmpeg-next`, `rsmpeg`) unless a future requirement needs a container
  symphonia doesn't decode. Both drag in a system FFmpeg build whose license is **LGPL by default,
  GPL if built `--enable-gpl`, or outright non-redistributable if built `--enable-nonfree`** —
  real licensing exposure that depends entirely on build flags, and symphonia avoids the question
  altogether by not linking FFmpeg at all.
- **Resampling:** `rubato` — async sinc/polynomial resampler is real-time-safe (no allocation in
  the hot path) and handles the mic-vs-system-audio sample-rate mismatch case if the two capture
  paths ever disagree on rate.
- **DSP/metering:** hand-roll RMS/peak metering and channel downmixing (a dozen lines each); don't
  add `dasp` (abandoned since 2020) or `fundsp` (a synthesis/effects DSL, wrong shape for this).
  Add `rustfft`/`realfft` only if you need spectral analysis later.
- **VAD:** `voice_activity_detector` (Silero v5 via ONNX Runtime) for actual speech/silence
  segmentation quality; keep `earshot` (a small self-contained neural-net VAD, ~40KiB model, no
  ONNX runtime dependency — **not** a WebRTC-algorithm reimplementation, see correction below) as
  a lighter fallback if the ONNX Runtime download/binary story becomes a packaging problem. Do not
  use `webrtc-vad` — abandoned since 2019.
- **Ring buffer (callback → async bridge):** `rtrb` — purpose-built SPSC ring buffer for exactly
  this job, no allocation after construction, push/pop never block.
- **Skip `rodio`** — it's a playback/mixing library; meetrs has no playback requirement beyond
  maybe a UI beep, which doesn't justify the dependency.

## Comparison table

| Crate | Latest | Released | Downloads (all-time / recent) | License | Last push | Open issues | Verdict |
|---|---|---|---|---|---|---|---|
| cpal | 0.18.1 | 2026-06-07 | 17.1M / 4.26M | Apache-2.0 | 2026-08-01 | 137 | Production-ready, actively developed |
| symphonia | 0.6.0 | 2026-05-15 | 9.45M / 3.30M | MPL-2.0 | 2026-08-02 | 73 | Production-ready |
| hound | 3.5.1 | 2023-09-25 | 15.6M / 3.94M | Apache-2.0 | 2026-02-08 | 40 | Stable, feature-complete, low churn |
| claxon | 0.4.3 | 2020-08-09 | 3.67M / 0.66M | Apache-2.0 | 2025-12-03 | 9 | Stable, low-activity but not abandoned |
| audiopus | 0.2.0 | 2021-04-22 | 1.28M / 0.35M | ISC | 2023-05-09 | 5 | Stagnant — bindings, works, rarely touched |
| opus (opus-rs) | 0.3.1 | 2026-01-03 | 1.40M / 0.39M | MIT/Apache-2.0 | 2026-05-24 | 9 | Alive, small |
| ogg | 0.9.2 | 2025-01-12 | 8.73M / 1.68M | BSD-3-Clause | 2025-01-12 | 13 | Stable |
| rodio | 0.22.2 | 2026-03-05 | 9.66M / 2.03M | MIT/Apache-2.0 | 2026-08-01 | 160 | Active; not needed for meetrs |
| rubato | 4.0.0 | 2026-07-09 | 8.64M / 2.97M | MIT/Apache-2.0 | 2026-07-18 | 4 | Production-ready |
| dasp | 0.11.0 | 2020-05-29 | 4.44M / 0.94M | MIT/Apache-2.0 | 2020-05-29 (crate); repo pushed 2026-01-12 | 65 | **Abandoned** — crate + all `dasp_*` sub-crates unpublished since 2020 despite repo remaining nominally active |
| fundsp | 0.23.0 | 2026-01-07 | 0.18M / 33K | MIT/Apache-2.0 | 2026-03-03 | 12 | Active, but a synthesis DSL — wrong tool for capture-side DSP |
| realfft | 3.5.0 | 2025-06-12 | 14.2M / 3.81M | MIT | 2026-03-12 | 3 | Stable, low-churn, well-used |
| rustfft | 6.4.1 | 2025-09-18 | 23.8M / 6.01M | MIT/Apache-2.0 | 2025-09-18 | 23 | Production-ready, most-used FFT crate |
| ringbuf | 0.5.1 | — | 15.4M / 3.34M | MIT/Apache-2.0 | 2026-07-14 | 8 | Active, more feature-rich than rtrb (SPSC + growable variants) |
| rtrb | 0.3.4 | — | 9.51M / 3.02M | MIT/Apache-2.0 | 2026-07-12 | 13 | Active, purpose-built for audio-thread handoff |
| crossbeam | 0.8.4 | — | 124.5M / 25.8M | MIT/Apache-2.0 | 2024-01-08 | 36 | Ubiquitous; last publish 2024, low churn ≠ abandoned |
| voice_activity_detector | 0.2.1 | — | 110K / 55.8K | MIT (non-SPDX field on crates.io; MIT via LICENSE file) | 2025-08-04 | 6 | Small but real, Silero v5 quality |
| webrtc-vad | 0.4.0 | 2019-10-01 | 500K / 184K | MIT | 2020-07-16 | 5 | **Abandoned** since 2020 |
| earshot | 1.2.1 | — | 103K / 83.3K | MIT/Apache-2.0 | 2026-07-22 | 2 | Active, small NN-based VAD (not WebRTC-algorithm), no ONNX dependency |
| ffmpeg-next | 8.1.0 | — | 6.12M / 3.10M | WTFPL (wrapper); FFmpeg itself GPL/LGPL depending on build | 2026-07-21 | 83 | Active but heavy — system FFmpeg + licensing burden |
| rsmpeg | 0.18.0+ffmpeg.8.0 | — | 156K / 34.1K | MIT (wrapper); same FFmpeg licensing caveat | 2025-08-24 | 15 | Smaller community, same FFmpeg burden |
| samplerate (libsamplerate bindings) | 0.2.4 | — | 1.65M / 0.41M | BSD-2-Clause | 2023-09-15 | 6 | Stagnant; `rubato` is the pure-Rust, actively-maintained alternative |

Downloads are "all-time / recent" (crates.io `downloads` / `recent_downloads`, a rolling
~90-day figure per the crates.io API). "Last push" is the GitHub repo's `pushed_at`, which can
reflect non-code activity (docs, CI, README).

## Detail

### cpal — device I/O

Cross-platform audio I/O: CoreAudio (macOS/iOS default), ALSA (Linux/BSD default), WASAPI (Windows
default, optional ASIO/JACK), AAudio (Android), Web Audio API (Wasm). On Linux/BSD, **PipeWire and
PulseAudio are separate native `Host` implementations, not ALSA sub-backends** — each is its own
Cargo feature (`pipewire` → `dep:pipewire`, `pulseaudio` → `dep:pulseaudio`, `jack` →
`dep:jack`) with an independent `devices()`/stream implementation, confirmed by reading
`Cargo.toml`'s `[features]` table and `src/host/pulseaudio/mod.rs` directly. This is itself a
change: PipeWire got a dedicated native host in cpal 0.16 (PR #938/#1093) and PulseAudio got one
in 0.17.2 (PR #957) — before that, "PipeWire/PulseAudio support" in cpal meant going through
ALSA's PipeWire/Pulse-emulation PCM devices. When multiple Linux hosts are compiled in, priority
is PipeWire > PulseAudio > ALSA (CHANGELOG 0.18.0, "Linux/BSD: Default host priority"). `[README +
Cargo.toml + CHANGELOG.md, fetched]`

**System audio / loopback capability — this is the interesting update, and it checks out.**
Verified directly against `CHANGELOG.md` and the originating GitHub PRs (not just a description of
the feature, but the exact merged changes):

- **cpal 0.17.0 (2025-12-20)** added, under CoreAudio → Added: "Support for loopback recording
  (recording system audio output) on macOS > 14.6" — merged via
  [PR #1003](https://github.com/RustAudio/cpal/pull/1003) "Support loopback recording on macOS."
  This wraps a Core Audio process-tap mechanism (same family the macOS research hand-rolls), not
  ScreenCaptureKit — a separate ScreenCaptureKit-loopback PR
  ([#894](https://github.com/RustAudio/cpal/pull/894)) is still open/unmerged. Issue
  [#1030](https://github.com/RustAudio/cpal/issues/1030), "Loopback not working on macOS <= 14.6,"
  confirms the version floor is a real field-reported constraint, not a docs typo.
- **cpal 0.18.0 (2026-06-06)**'s CoreAudio → Fixed section reads exactly as the prior draft
  quoted: "Fix undefined behavior and silent failure in loopback device creation," "Fix loopback
  aggregate device UID collisions between concurrent instances and after crashes," "Fix loopback
  capture returning silence due to disabled tap auto-start." These three land in a single PR,
  [#1198](https://github.com/RustAudio/cpal/pull/1198) ("fix(coreaudio): aggregate-device uuid
  collision + auto start to true") plus an earlier related fix,
  [#1123](https://github.com/RustAudio/cpal/pull/1123) ("declare aggregate_device_id as mut and
  check AudioHardwareCreateProcessTap status"). That's the exact "zero samples" failure mode the
  macOS research flagged as unfixed and long-lived — cpal is now fighting the same class of bug
  from inside a general-purpose crate instead of a purpose-built helper. As of 0.18.1 there is no
  further loopback-specific fix in `[Unreleased]`, and a still-open PR
  ([#1257](https://github.com/RustAudio/cpal/pull/1257), "check macOS system audio permission")
  suggests the permission-prompt/auto-start edge cases aren't fully closed out yet either.

**Verdict on the "is the zero-samples bug actually fixed" question: partially, and only as of
0.18.0 (one release, 2026-06-06 to today 2026-08-03).** The specific "silent tap" bug the macOS
research names is addressed by a named, merged fix (#1198), but it's young — under two months in
the wild at doc time, with related permission-handling work (#1257) still open. Recommendation:
don't switch off the hand-rolled tap approach yet; re-evaluate once cpal's loopback path has a few
more patch releases behind it. `[CHANGELOG.md + GitHub PR search, fetched — upgraded from the
original draft's unverified paraphrase to citations of the actual merged PRs]`

On Linux, cpal does not need special "loopback" support the way macOS does — PulseAudio/PipeWire
already expose a `.monitor` source as an ordinary input device. **This is now verified, resolving
the sibling Linux doc's `[unverified]` flag on the same question**: reading
`src/host/pulseaudio/mod.rs`'s `devices()` method directly shows it calls `list_sources()` (the
PulseAudio protocol's full source list) with **no filtering by source type** — monitor sources
have no special status in that list, they're ordinary `SourceInfo` entries alongside microphones,
so cpal's PulseAudio host enumerates them as regular `Device::Source` input devices with no extra
code required. This only holds for cpal's **native `pulseaudio`/`pipewire` host features**
(0.16+/0.17.2+); the plain ALSA host has no monitor concept at all and won't see them. `[cpal
source, github.com/RustAudio/cpal/blob/master/src/host/pulseaudio/mod.rs, fetched]`

**Buffer/callback model:** cpal is callback-based — you register a closure invoked by the OS audio
thread with a `&[f32]` (or other sample type) buffer each cycle; there is no async/await surface
in the hot path. `realtime` and `realtime-dbus` Cargo features exist to raise the callback thread
to `SCHED_FIFO`/high-priority scheduling for lower latency, but the README notes this "only
succeed[s] when the process lacks the resource limits to acquire `SCHED_FIFO`" — i.e. it can
silently fail to promote priority. `[README, fetched]`

**Known issues (from README/CHANGELOG):** PipeWire/PulseAudio can hold exclusive ALSA device
access, producing `DeviceBusy` errors on the ALSA backend; default buffer sizes vary wildly (1024
to `u32::MAX` on misconfigured hardware) — never assume a buffer size, always query the actual
stream config. `[README, fetched]`

**Maintenance:** very healthy — 137 open issues, but pushed 2026-08-01 (yesterday relative to
today's date), 17.1M downloads all-time, 4.26M in the recent window. Not a toy.

### Decoding — symphonia

Pure-Rust, no system dependencies. Containers: AIFF, **CAF** (feature `caf` — directly relevant,
this is the container the macOS capture path already writes), ISO/MP4, MKV/WebM, Ogg, WAV.
Codecs: AAC-LC, ADPCM, ALAC, FLAC, MP1/2/3, PCM, Vorbis. By default only royalty-free open formats
are enabled; MP3/AAC/MP4/AIFF/CAF are behind explicit feature flags. The docs don't spell out
codec-by-codec patent/royalty status beyond that framing — audio patents on MP3/AAC have
substantially expired as of 2026, but this claim about symphonia's own docs is `[unverified]`
beyond the "royalty-free by default" framing quoted above. `[docs.rs/symphonia, fetched]`

MPL-2.0 license — file-level copyleft, weaker than GPL, generally fine to link into a proprietary
binary as long as modified symphonia source files themselves stay open; doesn't touch the rest of
the meetrs codebase. Very active: pushed 2026-08-02, 9.45M downloads.

### Other decode/encode crates

- **hound** (WAV read/write): stable, Apache-2.0, 15.6M downloads, no update since 2023-09 but the
  WAV spec doesn't move — feature-complete rather than abandoned. If meetrs needs to *write* WAV
  (e.g., segment exports), this is the standard choice; symphonia is read/decode-focused.
- **claxon** (FLAC decode, pure Rust): stable, low-churn (last push 2025-12-03), small (330 stars).
  Redundant with symphonia's built-in FLAC decoder unless you specifically want a lighter
  single-purpose dependency.
- **audiopus / opus (opus-rs)**: both are bindings to libopus (C), not pure-Rust encoders/decoders
  — a system `libopus` or vendored build is required either way. `audiopus` (ISC) is stagnant
  (last push 2023-05); `opus`/opus-rs (MIT/Apache-2.0) is smaller but more recently touched
  (2026-05-24). Only relevant if meetrs needs Opus for network transport/compression of the audio
  artifact — no clear need identified yet for local mic+system capture piped straight to a
  transcription engine.
- **ogg**: low-level Ogg container read/write, BSD-3-Clause, stable (RustAudio org), used
  internally by symphonia's Ogg demuxer and by audio-encoding pipelines that need raw Ogg framing.
- **ffmpeg-next / rsmpeg**: both bind to a full FFmpeg build. `ffmpeg-next`'s own crate license is
  WTFPL (confirmed via crates.io), `rsmpeg`'s is MIT — but neither says anything about the FFmpeg
  binary/library you link against. FFmpeg itself is dual-licensed **LGPL 2.1+ by default**; it
  only becomes **GPL 2+** if built with `--enable-gpl` (which pulls in GPL-licensed components,
  e.g. `libx264`/`libx265`/certain filters); and specific codecs/features additionally require
  `--enable-nonfree` (e.g. `libfdk-aac`), which makes the resulting build **not freely
  redistributable at all** regardless of GPL/LGPL framing. So the actual exposure is entirely a
  function of which `./configure` flags produce the FFmpeg binary meetrs links against, not a
  fixed property of `ffmpeg-next`/`rsmpeg` themselves — the crates' own READMEs don't state this,
  it doesn't touch the wrapper crate's license, and it needs to be checked against whatever build
  script/vendored-FFmpeg config meetrs would actually use. `[GitHub README + crates.io, fetched;
  the LGPL/GPL/nonfree distinction itself is general FFmpeg-licensing knowledge, not confirmed from
  a fetched FFmpeg source in this pass]`. Both crates also require a working FFmpeg toolchain at
  build time (pkg-config, headers, shared/static libs) — real CI and cross-compile pain on
  macOS+Linux for no capability symphonia doesn't already give you for WAV/CAF/FLAC. Skip unless a
  specific unsupported container shows up.

### Resampling — rubato

Three resampler families: `Async` (sinc or polynomial interpolation, variable ratio, handles
clock drift/ratio changes at runtime), `Fft` (fixed-ratio, high quality, no quality knobs needed),
and `Slip` (near-1.0 ratio "clutch," passes samples through with no filtering, minimal CPU). Sinc
interpolation gives the best quality with anti-aliasing at higher CPU cost; polynomial trades
quality for speed when "little aliasing is acceptable." Explicitly documented as allocation-free
during `process_into_buffer()`, i.e. safe to use in a steady-state real-time path (not necessarily
inside the audio *callback* itself — better run on the consumer side after the ring buffer hop).
`[docs.rs/rubato, fetched]` Actively maintained (pushed 2026-07-18, only 4 open issues) and the
clear default choice over the abandoned `samplerate` (libsamplerate C bindings, BSD-2, stagnant
since 2023) or `dasp`'s resampling module (dead).

### DSP/utility — dasp, fundsp, rustfft/realfft

**dasp is abandoned.** The meta-crate `dasp` 0.11.0 and every sub-crate checked
(`dasp_rms`, `dasp_signal`, `dasp_ring_buffer`, `dasp_sample`) last published **2020-05-29** —
five-plus years with zero releases, confirmed directly against the crates.io API per-crate, not
just the umbrella crate. The GitHub repo shows a `pushed_at` of 2026-01-12, but that's
non-release repo activity (issues/docs), not new crate versions; 65 open issues on the repo is
consistent with a project that gets bug reports nobody ships fixes for. Don't build on it for
anything meetrs will maintain long-term.

**fundsp** is a real, actively-maintained (pushed 2026-03-03, 12 open issues) audio DSP/synthesis
library, but it's shaped as a signal-processing *graph DSL* for building synths/effects chains —
overkill and the wrong abstraction for "downmix N channels to mono and compute RMS." Not
recommended for meetrs' actual need.

**rustfft** (general-purpose FFT, 23.8M downloads, most-used FFT crate in the ecosystem) and
**realfft** (real-input wrapper around rustfft avoiding the conjugate-symmetric half-spectrum
bookkeeping) are both production-ready and low-churn because the algorithms are settled, not
because they're neglected — rustfft pushed 2025-09-18, realfft 2025-06-12, both with single-digit
open issue counts. Only pull these in if meetrs actually needs spectral analysis (e.g., a
frequency-domain VAD feature or spectral gating); RMS/peak metering and channel downmixing don't
need an FFT at all — they're a running sum-of-squares and an averaging loop, respectively. Nothing
in the ecosystem packages "RMS metering" or "channel downmix" as a dedicated crate; this is
correctly a dozen lines of code, not a dependency.

### Playback — rodio

`rodio` (built on cpal) is a mixing/playback library — decoding + resampling + mixing multiple
sources down to an output stream. It's active (pushed 2026-08-01, 9.66M downloads) but solves a
problem meetrs doesn't have: meetrs captures and hands PCM to a transcription engine, it doesn't
play audio back except perhaps a UI notification sound, which is a rodio-sized hammer for a
one-line `afplay`/system-sound-API nail. Not recommended.

### VAD — voice activity detection

- **voice_activity_detector** (`nkeenan38`): wraps **Silero VAD v5** (fixed window: 256 samples at
  8kHz, 512 at 16kHz) via the `ort` crate (ONNX Runtime bindings). Default build downloads a
  prebuilt ONNX Runtime binary from Microsoft at build/run time — no manual system install needed
  for typical use, with a `load-dynamic`/`dlopen()` escape hatch if you need to control binary
  placement. Mono-only; multi-channel audio must be split before feeding it in. Supports both a
  synchronous iterator API and an `async` feature for streaming. crates.io reports its license
  field as "non-standard" because the crate uses a `LICENSE` file rather than an SPDX `license`
  key in Cargo.toml — the file itself is plain MIT, confirmed by fetching it directly.
  `[docs.rs + GitHub LICENSE, fetched]` Small (110K downloads, 64 GitHub stars) but real and
  current (pushed 2025-08-04, 6 open issues) — this is the highest-quality VAD option here and
  matches the prior research's requirement to segment on actual speech rather than a hard-cap
  timer.
- **earshot**: **correction — this is not a WebRTC VAD reimplementation.** The prior draft called
  it "a pure-Rust reimplementation of the WebRTC fixed-point VAD algorithm"; the crate's own
  README (fetched directly, `github.com/pykeio/earshot`) says nothing about WebRTC at all — it's a
  small self-contained **neural network** VAD (~40KiB model weights, ~95KiB total binary, ~8KiB
  runtime memory per `Detector` instance), operating on 256-sample (16ms @ 16kHz) mono `i16`/`f32`
  frames, with no ONNX Runtime or other ML-runtime dependency (the network is small enough to run
  without one). This has been true since the earliest available tagged source (v1.0.0,
  2026-02-22) — there's no evidence it was ever WebRTC-algorithm-based; the doc's original claim
  appears to be a mix-up with a different project. The crate's own README **claims** (self-
  reported benchmark, not independently verified here) to be both faster and *more* accurate than
  Silero VAD v6 and TEN VAD, which directly contradicts the prior draft's speculation that earshot
  would be *less* accurate than Silero — that speculation is now retracted, not confirmed either
  way. Actively maintained (pushed 2026-07-22, only 2 open issues, 184 stars, and notably
  newer/more downloaded in the recent window — 83.3K recent vs 102.9K all-time, i.e. most of its
  downloads are recent — than `voice_activity_detector`'s 55.8K/110.2K). Still worth using as the
  low-dependency fallback, or if the Silero ONNX-download story ever becomes a packaging/offline-
  build blocker; a real head-to-head benchmark against Silero on meetrs' own meeting-audio
  characteristics is still `[unverified]` and needed regardless of either project's own marketing
  claims. `[github.com/pykeio/earshot README, fetched directly — corrects the original,
  unverified characterization]`
- **webrtc-vad** (`kaegi`): **abandoned** — 4 versions ever published on crates.io, all in a
  seven-week window 2019-08-14 to 2019-10-01; GitHub repo last pushed 2020-07-16 (confirmed via
  crates.io per-version history and the GitHub API). No currently-checked crate is a direct
  drop-in replacement implementing the same WebRTC fixed-point algorithm — `earshot` (see
  correction above) is a different, NN-based approach, not a WebRTC-VAD reimplementation. Do not
  adopt `webrtc-vad`.

### Ring buffers / lock-free realtime patterns

The universal rule, confirmed by every source above that discusses real-time behavior: **never
allocate, lock, or block inside the audio callback.** No `Vec::push` that might reallocate, no
`Mutex`, no `println!`, no channel send that can block. The callback's only job is to move samples
into a fixed-capacity structure and return.

- **rtrb**: single-producer single-consumer, lock-free *and* wait-free, fixed capacity allocated
  once at construction, `push()`/`pop()` never allocate afterward, `write_chunk()`/`read_chunk()`
  for batched access without per-sample overhead. This is exactly the audio-callback-to-consumer-
  thread shape and it's the crate's stated purpose. `[docs.rs/rtrb, fetched]` Actively maintained
  (pushed 2026-07-12), 9.51M downloads.
- **ringbuf**: broader feature set (SPSC plus other variants, async wrappers), also active (pushed
  2026-07-14, actually more downloads than rtrb: 15.4M vs 9.51M all-time). Either is a defensible
  choice for the callback hop; `rtrb` is more tightly scoped to exactly this use case, `ringbuf` is
  more general if you later need something fancier than plain SPSC.
- **crossbeam**: the workhorse general concurrency crate (channels, epoch-based GC, etc.),
  124.5M downloads — for-scale ubiquitous, last published 2024-01-08 (low churn because the crate
  is mature, not neglected: it's a dependency of roughly a third of the ecosystem). Its MPMC
  channels are a fine choice *downstream* of the ring buffer (e.g. moving fully-formed chunks
  between worker threads) but crossbeam's channels themselves are not documented as callback-safe
  in the same sense as rtrb/ringbuf — don't call `crossbeam_channel::Sender::send` from inside the
  audio callback; use the SPSC ring buffer there instead. This distinction is an inference from
  the ring-buffer crates' explicit real-time claims versus crossbeam's general-purpose design, not
  a directly fetched statement — flagged `[unverified]` as a hard claim about crossbeam channel
  internals, but it matches the field's universal recommendation.

### Async runtime interaction — bridging the callback to tokio

No single crate solves this; it's a pattern, not a dependency, and no source above claims otherwise
(`[unverified as a benchmarked/blessed pattern — this is the standard shape described across the
cpal/rtrb ecosystem docs, not a specific fetched recommendation]`). The shape that's consistent
with every real-time constraint documented above:

1. Audio callback (cpal) writes samples into an `rtrb::Producer` — non-blocking, non-allocating,
   returns immediately even if the buffer is momentarily full (drop or count overruns, don't
   block).
2. A plain OS thread (not a tokio task — tokio tasks aren't guaranteed to be scheduled promptly
   under load) owns the `rtrb::Consumer`, polls it (either a tight loop with a short sleep, or
   `rtrb`'s blocking `read_chunk` variants), and on each chunk either calls back into application
   code directly or hands the chunk to tokio.
3. Handing off to tokio from that consumer thread: `tokio::sync::mpsc::channel`'s blocking
   `Sender::blocking_send` (called from the non-async consumer thread, not the audio callback) is
   the standard bridge — tokio explicitly supports this for exactly this producer-outside-tokio
   case. This step happens outside the real-time-constrained callback, so tokio's internal
   allocation/locking is fine here.

## Open questions

- Does meetrs actually need Opus/FLAC encoding at all, or does the durable artifact stay as raw
  WAV/CAF (in which case `audiopus`/`opus-rs`/`claxon` are all unnecessary dependencies)? Prior
  macOS research assumes a 4-channel float32 CAF as the recording contract — if that stands,
  hound/claxon/opus may not be needed at all beyond symphonia's read-side CAF support.
  `[unverified — not addressed in either doc]`
- cpal 0.17/0.18's native macOS loopback support is confirmed real (see PR citations above) but
  still new in the field — 0.18.0 shipped 2026-06-06, so under two months of patch-release history
  at doc time, with one related PR (#1257, permission handling) still open. Worth a hands-on spike
  to see whether it now matches or still trails a hand-rolled Core Audio tap before committing
  either way. `[unverified — the code paths are confirmed to exist; their field reliability is
  not]`
- No head-to-head accuracy/latency comparison between Silero-based `voice_activity_detector` and
  `earshot` (a small NN-based VAD, **not** a WebRTC-VAD reimplementation as the original draft
  claimed — see correction in the VAD section) was found; earshot's own README claims to beat
  Silero v6 on both speed and accuracy in its self-reported benchmark, but that is a vendor claim,
  not independent verification. A decision between them for meetrs' actual meeting-audio
  characteristics (background noise, overlapping speech, echo bleed noted in the
  transcription-pipeline research) needs empirical testing, not just crate-maturity signals or
  either project's own numbers. `[unverified]`
- Whether `ort`'s prebuilt ONNX Runtime binary download story is acceptable for meetrs' build/CI
  pipeline (offline builds, code-signing/notarization implications on macOS, static linking on
  Linux) wasn't investigated here — flag before committing to `voice_activity_detector` as primary.
  `[unverified]`
- Exact behavior of `tokio::sync::mpsc::Sender::blocking_send` under backpressure from a
  non-tokio OS thread (does it block indefinitely, and is that acceptable for meetrs' consumer
  thread) wasn't verified against tokio's own docs in this pass. `[unverified]`

## Sources

- https://crates.io/api/v1/crates/cpal (and per-crate/per-version endpoints for symphonia, hound,
  claxon, audiopus, opus, ogg, rodio, rubato, dasp, dasp_rms, dasp_signal, dasp_ring_buffer,
  dasp_sample, fundsp, realfft, rustfft, ringbuf, rtrb, crossbeam, voice_activity_detector,
  webrtc-vad, earshot, ffmpeg-next, rsmpeg, samplerate) — authoritative version numbers, download
  counts, license fields, publish dates for every crate in the comparison table.
- https://api.github.com/repos/{org}/{repo} for the same set of projects — `pushed_at`,
  `open_issues_count`, `archived`, `stargazers_count`, license SPDX id; basis for all "last push" /
  "abandoned" / "active" maintenance calls.
- https://docs.rs/symphonia/latest/symphonia/ — confirmed container/codec feature-flag table
  including CAF support and the "royalty-free by default" framing.
- https://github.com/RustAudio/cpal — README: host API list per platform, realtime/realtime-dbus
  features, known DeviceBusy/buffer-size issues.
- https://raw.githubusercontent.com/RustAudio/cpal/master/CHANGELOG.md — exact changelog text for
  the 0.17.0 loopback-recording addition and the 0.18.0 loopback bug fixes (UID collision, silent
  tap, silence-on-capture), fetched and read directly line-by-line (not just summarized) to confirm
  which fixes land in which version.
- https://github.com/RustAudio/cpal/pull/1003 ("Support loopback recording on macOS") — the PR
  that shipped the 0.17.0 loopback feature.
- https://github.com/RustAudio/cpal/pull/1198 ("fix(coreaudio): aggregate-device uuid collision +
  auto start to true") and https://github.com/RustAudio/cpal/pull/1123 ("declare
  aggregate_device_id as mut and check AudioHardwareCreateProcessTap status") — the PRs behind the
  0.18.0 loopback bug fixes.
- https://github.com/RustAudio/cpal/pull/894 (open) and
  https://github.com/RustAudio/cpal/issues/1030, /1257 — confirm the merged loopback path is a
  Core Audio process tap (not ScreenCaptureKit) and that related permission-handling work is still
  in flight.
- https://raw.githubusercontent.com/RustAudio/cpal/master/Cargo.toml — `[features]` table proving
  `pipewire`/`pulseaudio`/`jack` are independent Cargo features/hosts, not ALSA sub-backends.
- https://raw.githubusercontent.com/RustAudio/cpal/master/src/host/pulseaudio/mod.rs — read
  directly to confirm `Host::devices()` calls `list_sources()` with no filtering, i.e. PulseAudio
  `.monitor` sources are enumerated as ordinary input devices by cpal's native PulseAudio host.
  This resolves the `[unverified]` flag on the same question in `rust-audio-linux.md`.
- https://github.com/RustAudio/cpal/pull/938, /1093 (PipeWire native host) and /957 (PulseAudio
  native host) — establish when these became separate native hosts rather than ALSA-bridge PCMs.
- https://github.com/zmwangx/rust-ffmpeg — README: WTFPL crate license, FFmpeg version range
  supported (3.4–8.0), confirms the README does *not* itself address FFmpeg's own GPL/LGPL terms.
- https://docs.rs/rubato/latest/rubato/ — resampler type table (Async/Fft/Slip), quality/CPU
  tradeoffs, allocation-free `process_into_buffer()` claim.
- https://docs.rs/rtrb/latest/rtrb/ — SPSC, lock-free/wait-free claim, no-allocation-after-
  construction claim, chunk API description.
- https://docs.rs/voice_activity_detector/latest/voice_activity_detector/ — Silero v5 model,
  `ort`/ONNX Runtime dependency, prebuilt-binary default, mono-only constraint, sync+async API.
- https://raw.githubusercontent.com/nkeenan38/voice_activity_detector/main/LICENSE — confirmed
  plain MIT text behind crates.io's "non-standard" license field.
- https://raw.githubusercontent.com/pykeio/earshot/main/README.md (and the `1.0.0` tag's README) —
  read directly; corrects the prior draft's "WebRTC VAD reimplementation" claim. Earshot is a
  small neural-net VAD (~40KiB model), not a WebRTC-algorithm reimplementation, and has been since
  its earliest available tagged source.
- WebSearch "cpal rust loopback system audio capture monitor source pulseaudio pipewire 2026" —
  corroborated PipeWire/PulseAudio backend selection order and confirmed the macOS >14.6 loopback
  claim from a second angle (DeepWiki/GitHub release notes), used as a cross-check on the
  CHANGELOG.md read.

## Fact-check log (2026-08-03)

**Method:** every crate's version/downloads/license/publish-date was re-pulled live from
`https://crates.io/api/v1/crates/<name>` (with a `User-Agent` header — crates.io 403s anonymous
requests without one, which is why the first pass came back null) and cross-checked against
`https://api.github.com/repos/<org>/<repo>` for `pushed_at`/`open_issues_count`/`license`/stars.
cpal's CHANGELOG.md and the GitHub PR/issue search API were read directly rather than trusted from
a paraphrase.

**CONFIRMED (no change needed):**
- Every crate version, download count, and license in the comparison table matches the live
  crates.io API, with one caveat below (audiopus). `dasp` and all `dasp_*` sub-crates: confirmed
  independently, every one last published 2020-05-29 on crates.io, GitHub repo `pushed_at`
  2026-01-12 (non-release activity) — the "abandoned since 2020" verdict is accurate.
  `webrtc-vad`: confirmed 4 versions total on crates.io (2019-08-14 through 2019-10-01), GitHub
  `pushed_at` 2020-07-16 — "abandoned since 2020" is accurate by the doc's own stated methodology
  (using repo push date, not crate-publish date, as the cutoff).
- **The most important claim (cpal loopback):** confirmed exactly as stated, down to the version
  numbers and exact changelog wording. cpal 0.17.0 (2025-12-20) added CoreAudio loopback recording
  for macOS >14.6 (merged via PR #1003); cpal 0.18.0 (2026-06-06) shipped the three named bug fixes
  (undefined behavior/silent failure in device creation, UID collisions, silent tap due to
  disabled auto-start), merged via PR #1198 and an earlier related PR #1123. The original draft's
  characterization was accurate; this pass adds the PR numbers and confirms via the actual PRs
  that the fix is a Core Audio process tap, not ScreenCaptureKit (a separate ScreenCaptureKit PR,
  #894, remains open/unmerged).
- `voice_activity_detector`: Silero VAD v5, 256-sample window @ 8kHz / 512-sample @ 16kHz, `ort`/
  ONNX Runtime dependency with prebuilt-binary default, mono-only, sync+async API, MIT license via
  LICENSE file — all confirmed by fetching the crate's own README/docs directly.
- FFmpeg GPL/LGPL exposure: the underlying claim was directionally right but imprecise; tightened
  to state the actual mechanism (LGPL by default, GPL only with `--enable-gpl`, non-redistributable
  with `--enable-nonfree`) rather than a vague "GPL or LGPL depending on build configuration."

**CORRECTED:**
- **earshot's identity** — said: "pure-Rust reimplementation of the WebRTC fixed-point VAD
  algorithm." Actually: a small self-contained **neural-network** VAD (~40KiB model), unrelated to
  the WebRTC VAD algorithm, per the crate's own README fetched directly from
  `github.com/pykeio/earshot`. This also invalidates the doc's speculation that earshot would be
  *less* accurate than Silero on noisy audio — earshot's own (unverified, vendor) benchmark claims
  the opposite. This was the single largest factual error found.
- **cpal's Linux backend model** — said ALSA is "default, with optional JACK/PipeWire/PulseAudio
  backends behind Cargo features," implying they're ALSA sub-backends. Actually: PipeWire and
  PulseAudio are independent native `Host` implementations (their own Cargo features, own
  `devices()`/stream code), not ALSA feature flags — confirmed by reading `Cargo.toml` and
  `src/host/pulseaudio/mod.rs` directly. They only became native hosts in cpal 0.16 (PipeWire, PR
  #938/#1093) and 0.17.2 (PulseAudio, PR #957); before that, "PipeWire/PulseAudio support" meant
  going through ALSA's emulation PCMs.
- **realfft's "last push" table cell** — said 2025-06-12 (same as the crates.io publish date).
  GitHub `pushed_at` is actually 2026-03-12 — the doc's own stated methodology (GitHub `pushed_at`
  for "last push") wasn't applied to this one row.
- `audiopus`'s version: doc says 0.2.0. crates.io's plain `max_version` field actually returns
  `0.3.0-rc.0` (a prerelease); `max_stable_version` is 0.2.0, matching the doc. Not an error, but
  worth noting the doc used the stable-version field, which is the right choice here.

**RESOLVED (upgraded from `[unverified]`):**
- The sibling `rust-audio-linux.md`'s `[unverified]` question — "can `cpal` see PulseAudio/PipeWire
  monitor sources as input devices?" — is now confirmed **yes, for cpal's native `pulseaudio`
  Cargo-feature host** (0.17.2+): `src/host/pulseaudio/mod.rs`'s `Host::devices()` calls
  `list_sources()` with no filtering, so `.monitor` sources appear as ordinary `Device::Source`
  entries. This does **not** apply to the plain ALSA host, which has no monitor concept.

**STILL UNVERIFIED (and why):**
- Whether cpal's 0.18.0 loopback fixes are actually stable in the field — the fixes are real and
  merged, but 0.18.0 is under two months old at doc time and one related PR (#1257, permission
  handling) is still open. No amount of source-reading substitutes for field experience here; this
  needs a hands-on spike, as the doc already recommends.
- Head-to-head Silero vs. earshot accuracy on real meeting audio — both projects' own claims are
  self-reported and not cross-checked against each other or against meetrs' actual audio
  characteristics.
- `ort`'s ONNX Runtime binary/offline-build/notarization story, and `tokio::sync::mpsc`'s
  `blocking_send` backpressure semantics — neither was investigated in this pass either, same as
  the original draft.
- Whether cpal's native PipeWire host (as opposed to PulseAudio, which was checked directly)
  applies the same no-filtering behavior to PipeWire monitor ports — plausible by symmetry with
  the PulseAudio host and consistent with the sibling Linux doc's PipeWire-side description, but
  the PipeWire host's source code wasn't read line-by-line in this pass the way the PulseAudio
  host's was.

**Overall assessment:** the original draft was largely accurate on the numbers and unusually
careful about marking speculation as `[unverified]`. The one substantive error — earshot's
identity — was a real factual mistake, not a hedged guess, and it touched the Recommendation,
the VAD detail section, the webrtc-vad comparison, and an open question; all four are now
corrected. The cpal loopback claim, which the task treated as the highest-stakes item to verify,
held up under direct inspection of the CHANGELOG and the merged PRs.
