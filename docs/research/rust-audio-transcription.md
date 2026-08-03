# Speech-to-text / transcription from Rust (2026)

Prior research (`docs/research/audio-recording-and-processing.md` → `deskwork/research/audio-transcription-pipeline/`)
picked `parakeet-mlx` + `parakeet-tdt-0.6b-v3` for a Python stack, because MLX gives
Metal acceleration with near-zero engineering cost. **MLX has no Rust bindings and no
plan to get any** — it's an Apple/Python-first framework. This doc works out the
nearest equivalent reachable from pure Rust, plus the honest fallbacks.

## Recommendation

- **macOS (primary target):** `whisper-rs` (whisper.cpp bindings) with the Metal backend,
  running a GGUF Whisper model (`large-v3-turbo` or `medium` quantized `q5_1`/`q8_0`).
  whisper.cpp's Metal path is mature, shipped, and the crate is actively maintained
  (v0.16.0, 2026-03-12). This is the pragmatic v1 choice — it is not Parakeet-class
  accuracy, but it is a real, buildable Rust dependency today.
- **If Parakeet-level accuracy/speed is a hard requirement on macOS:** `sherpa-onnx`
  (Rust bindings via the `sherpa-onnx` or `sherpa-rs` crate) loading sherpa-onnx's own
  native `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` export (documented on k2-fsa's own
  pretrained-models page) — this is a better first choice than routing through the
  community `istupakov/parakeet-tdt-0.6b-v3-onnx` export (built for a different project,
  `onnx-asr`), though that export also works and is CC-BY-4.0 licensed if needed as a
  fallback. This is the closest thing to a "Parakeet from Rust" path that doesn't
  require MLX or Python. Verify CoreML/ANE execution provider actually engages for
  either export before committing — `[unverified]` whether it runs well on CoreML EP vs
  CPU. A second, much more experimental option is `parakeet-rs` (gpu-cli), a pure-Rust
  Candle+Metal reimplementation of Parakeet-TDT-0.6B-v3 — confirmed to exist (1 star,
  9 commits), but its own README shows it running at ~2x NVIDIA's published WER for the
  same model (3.83% self-reported vs. NVIDIA's stated 1.93%); do not depend on it
  without vendoring or forking and expect materially worse accuracy than the model it
  targets.
- **Linux:** whisper.cpp/whisper-rs with CUDA (if an NVIDIA GPU is present) or CPU;
  sherpa-onnx with the CUDA execution provider for Parakeet/Zipformer/Moonshine models.
  No Metal/ANE equivalent exists on Linux — CPU is the honest floor, and it's slow for
  large Whisper models without a GPU.
- **Diarization:** skip it for v1. meetrs already gets "me vs. them" for free from the
  channel split (per prior research) — that removes the single hardest ML problem from
  the critical path. If per-system-channel speaker diarization is wanted later,
  sherpa-onnx ships pyannote-segmentation-3.0 + 3D-Speaker/NeMo embedding ONNX exports
  natively, which is more buildable from Rust than trying to FFI into pyannote-audio.
- **Escape hatch:** a subprocessed Python (parakeet-mlx) or Swift (WhisperKit/FluidAudio)
  helper emitting JSON over stdout remains the fastest path to today's best macOS
  accuracy/speed, at the cost of a second runtime and a packaging story. Reasonable for
  a v1 that ships fast; revisit once/if a pure-Rust path matures.
- **Cloud (fallback tier only):** Deepgram has an official-ish Rust SDK; treat any
  cloud API as opt-in given meetrs' local-first, privacy-sensitive design goal.

## Comparison table

| Engine | macOS accel | Linux accel | License | Maturity signal |
|---|---|---|---|---|
| whisper-rs (whisper.cpp) | Metal, Core ML (ANE) | CUDA, Vulkan, CPU | Unlicense (crate); MIT (whisper.cpp) | Active — whisper-rs 0.16.0 (2026-03-12); whisper.cpp v1.9.1 (2026-06-19), large community |
| candle (Whisper example) | Metal, Accelerate | CUDA, CPU | MIT/Apache-2.0 | Active — candle-core/transformers 0.11.0 (2026-06-26), HF-maintained; Whisper example works, Parakeet/Conformer not in candle-transformers proper |
| ort (ONNX Runtime bindings) | CoreML EP | CUDA, TensorRT, ROCm, OpenVINO EP | MIT/Apache-2.0 | Active — 2.0.0-rc.13 (2026-07-28), pykeio-maintained, still pre-1.0 |
| sherpa-onnx (Rust API) | CoreML (via ORT), CPU | CUDA (via ORT), CPU | Apache-2.0 | Active — `sherpa-onnx` crate 1.13.4 (2026-07-08); alt `sherpa-rs` 0.6.8 (2025-10-05, slower-moving) |
| parakeet-rs (gpu-cli, Candle) | Metal | `[unverified]` — Candle CUDA untested by this project | MIT (code); model weights CC-BY-4.0 | Experimental — 1 star, 9 commits, no version/release tagged |
| transcribe-rs | Whisper: whisper.cpp under the hood; Parakeet/Moonshine: ONNX via ort | Same, via ort | MIT | Active-ish — 0.3.11 (2026-04-07), small single-maintainer crate |
| pyannote-rs | CPU (ORT default) | CPU | MIT | Slow-moving — 0.3.4 (2025-09-07) |
| Deepgram Rust SDK | N/A (cloud) | N/A (cloud) | MIT | Active — 0.10.0 (2026-05-12), community-labeled ("Community Rust SDK") |

## whisper-rs / whisper.cpp

- **whisper-rs**: crates.io latest **0.16.0**, published 2026-03-12, license **Unlicense**.
  90+ dependent crates, ~105k downloads/month — a real, widely-used binding, not a toy.
  Project moved from GitHub to Codeberg (`codeberg.org/tazz4843/whisper-rs`); the GitHub
  mirror (`tazz4843/whisper-rs`) still shows up in search. `whisper-rs-sys` (the raw FFI
  layer) is at 0.15.0, same license, same publish date — versions are coupled.
- Cargo features (confirmed directly from `Cargo.toml` on the `tazz4843/whisper-rs`
  default branch): `metal`, `coreml`, `cuda`, `hipblas`, `vulkan`, `openblas`,
  `intel-sycl`, `openmp`, plus `log_backend`/`tracing_backend` and a `raw-api` escape
  hatch. So whisper-rs exposes **both** the Metal path and the Core ML/ANE path as
  first-class Cargo features, not just Metal — correcting an earlier draft of this doc
  that only mentioned Metal for macOS. Each feature compiles whisper.cpp from source
  with the matching backend enabled; whisper-rs itself doesn't add logic beyond the
  FFI/build-script layer.
  Building requires a C/C++ toolchain (whisper.cpp is vendored and compiled from source
  via `bindgen`/`cc`), so expect the usual FFI build friction (Xcode CLT on macOS,
  CUDA toolkit on Linux for the CUDA feature) — this is meaningfully more build
  complexity than a pure-Rust crate.
- Alternative bindings surfaced in search but with much smaller footprints: `mutter`
  (sigaloid), `whisper-cpp-plus-rs` (adds real-time PCM streaming + VAD on top of
  whisper.cpp) — worth a look if streaming/live transcription becomes a requirement,
  but unverified maturity `[unverified]`.
- **whisper.cpp itself**: latest tagged release **v1.9.1**, 2026-06-19 (ggml-org/whisper.cpp
  on GitHub), MIT license, very active (ggml-org organization, large contributor base).
  - **Metal**: full GPU inference path on Apple Silicon.
  - **Core ML / ANE**: the *encoder* can run on the Apple Neural Engine via a separate
    Core ML model conversion step. Confirmed verbatim from the upstream README: "This
    can result in significant speed-up — more than x3 faster compared with CPU-only
    execution." No specific chip/model-size pairing is given for that multiplier in the
    README itself, so treat "3x" as the project's own general claim, not a benchmark
    tied to a specific device — but the quote and the feature are real, not a rumor.
  - **Accelerate**: CPU path uses Apple's Accelerate framework (BLAS) when Metal/CoreML
    aren't used.
  - **GGUF quantization tradeoffs**: whisper.cpp uses GGML/GGUF quantized weights.
    Community guidance cited in search: Q5_1 loses under ~1% WER vs. full precision;
    Q4_0 loses roughly 2-4% WER — **`[unverified]`, no primary source page fetched for
    these exact numbers; treat as a rough community rule of thumb, not a benchmark
    citation**. Get real numbers from whisper.cpp's own `models/README.md` benchmark
    tables before writing this into any product doc.
  - **Long-form handling**: whisper.cpp does internal sliding-window long-form decoding
    like upstream Whisper; no separate chunking needed, consistent with prior research's
    finding for Whisper-family runtimes generally.
  - **Word timestamps**: supported (`--max-len 1 -ml` / token-level timestamp flags in
    the CLI; the same functionality is exposed through whisper-rs's segment API).
  - **`--tinydiarize` / `-tdrz`**: a real feature — loads a tinydiarize-finetuned model
    and inserts speaker-turn markers (`[SPEAKER_TURN]`) in the output. This is *turn
    detection*, not full diarization with identity/labels — it tells you when the
    speaker changed, not who is who. Given meetrs already has a channel split, this is
    more useful as a fallback for same-channel multi-speaker segments than as the
    primary diarization strategy.

## candle (Hugging Face Rust ML framework)

- `candle-core` / `candle-transformers`: crates.io latest **0.11.0**, published
  2026-06-26, MIT/Apache-2.0 dual license, maintained directly by Hugging Face.
- Ships a working Whisper example (downloads openai/whisper-tiny-class models and
  transcribes an audio file) with pluggable `Device::{Cpu, Cuda, Metal}` backends and
  Accelerate on Apple Silicon.
- **Parakeet / Conformer / TDT**: not part of `candle-transformers`'s built-in model zoo
  as far as this research found — no first-party Parakeet or Conformer example ships
  with candle. The `parakeet-rs` (gpu-cli) project above is a separate, third-party
  Candle-based reimplementation, not something upstream candle provides.
- **Real-world viability**: candle's Metal backend is described (project docs) as having
  hand-written Metal MSL kernels, i.e., it's not just a CPU fallback — but candle's
  ecosystem is thinner than PyTorch/MLX's; expect to hand-port anything beyond the
  handful of models in the official examples repo. Reasonable for Whisper specifically,
  risky as a foundation for Parakeet.

## ort (ONNX Runtime Rust bindings)

- crates.io latest **2.0.0-rc.13**, published 2026-07-28, MIT/Apache-2.0, maintained by
  pykeio (`github.com/pykeio/ort`) — the de facto successor to the abandoned
  `onnxruntime-rs`. Still pre-1.0 after multiple years of RCs — treat API stability as
  "usually fine, watch changelogs on upgrade."
- Execution providers gated behind cargo features: `coreml`, `cuda`, `tensorrt`,
  `directml`, `rocm`, `openvino`, `onednn`, `xnnpack`, `qnn`, `cann`, `nnapi`. CoreML EP
  targets macOS/iOS; on Linux, `cuda`/`tensorrt`/`rocm`/`openvino` cover
  NVIDIA/AMD/Intel respectively.
- `ort` itself doesn't ship models — it's the runtime `sherpa-onnx` and `transcribe-rs`
  build on top of. Whisper ONNX exports exist and run under `ort` (Whisper is a common
  ONNX export target); Parakeet ONNX exports also exist (see below) and are reported to
  run through `ort`/sherpa-onnx, though CoreML-EP-specific behavior for Parakeet's
  encoder/decoder/joiner triplet is `[unverified]` by this research — Parakeet's TDT
  architecture is unusual enough (three separate ONNX graphs: encoder, decoder, joiner)
  that EP compatibility should be tested directly, not assumed.

## sherpa-onnx

- Two competing Rust crate names exist — check both before picking one:
  - **`sherpa-onnx`** (crates.io): "Safe Rust wrapper for sherpa-onnx speech recognition
    toolkit," latest **1.13.4**, published 2026-07-08, Apache-2.0, ~136k total downloads.
    Confirmed this is the genuine official k2-fsa binding, not a third-party stub: the
    crate's sole listed owner is `csukuangfj` (Fangjun Kuang), a core k2-fsa/sherpa-onnx
    maintainer, and its `repository` field points directly at `github.com/k2-fsa/sherpa-onnx`.
    Tracks upstream closely (upstream is on Apache-2.0 too).
  - **`sherpa-rs`** (crates.io, BenLocal/Limit-LAB forks on GitHub): "Rust bindings to
    k2-fsa/sherpa-onnx," latest **0.6.8**, published 2025-10-05, MIT — slower release
    cadence, worth checking which one has an actively maintained fork before adopting.
- Upstream `k2-fsa/sherpa-onnx`: Apache-2.0, very active (2,000+ commits), and the
  single most broadly-modeled toolkit found in this research:
  - **ASR models**: Whisper (multilingual), Moonshine (English), Zipformer (multi-lang
    incl. Chinese/English/Korean/French/Japanese/Thai/Russian), Paraformer, and **NeMo
    transducer models — including Parakeet** (`k2-fsa.github.io/sherpa/onnx/pretrained_models/offline-transducer/nemo-transducer-models.html`
    documents this explicitly, listing `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` and
    `sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming` by name, alongside
    Parakeet-v2 and GigaAM/Russian NeMo exports). **Correction to an earlier draft of
    this doc**: sherpa-onnx ships its **own first-party** `int8` Parakeet-v3 export under
    the k2-fsa org — this is a materially better Rust path than routing through
    `istupakov/parakeet-tdt-0.6b-v3-onnx` (see the Parakeet section below), since it's
    packaged and named specifically for sherpa-onnx rather than being a community export
    for a different project (`onnx-asr`) that happens to also work with sherpa-onnx.
  - **Diarization**: first-party support — ships `sherpa-onnx-pyannote-segmentation-3-0`
    plus 3D-Speaker/NeMo speaker-embedding ONNX models for a full segmentation +
    embedding + clustering pipeline, no PyTorch runtime required.
  - **VAD**: Silero-VAD is a documented example model.
  - **Platform acceleration**: strong on NPU/edge targets (Rockchip RKNN, Qualcomm QNN,
    Ascend, Axera) — this research did not find an explicit statement of macOS
    CoreML/Metal or Linux CUDA support in the pages fetched; sherpa-onnx builds its own
    ONNX Runtime distribution, so CoreML/CUDA support most likely tracks whatever ORT
    execution providers that build was compiled with — **`[unverified]`, confirm by
    building sherpa-onnx from source with the coreml/cuda feature flags before relying
    on it.**
  - **No-HF-token advantage**: sherpa-onnx models are distributed as plain files on
    GitHub releases / Hugging Face without gated/token-required repos — this avoids the
    auth friction some pyannote and NeMo checkpoints impose, which matters for an
    offline product that shouldn't need a login at build or run time. **Confirmed** for
    diarization specifically: the `speaker-segmentation-models` GitHub release
    (`k2-fsa/sherpa-onnx`, published 2024-09-29) hosts
    `sherpa-onnx-pyannote-segmentation-3-0.tar.bz2` and the reverb-diarization models as
    plain, unauthenticated release assets — no Hugging Face login required, even though
    the upstream pyannote-segmentation-3.0 model card on HF is itself gated.

## NVIDIA Parakeet (`parakeet-tdt-0.6b-v3`) from Rust — honest answer

There is **no first-party Rust path from NVIDIA itself**. NeMo (NVIDIA's
training/export framework) is Python-only. The Rust-reachable options are all
third-party, but one of them (sherpa-onnx's own export) is closer to "supported" than
an earlier draft of this doc gave it credit for:

1. **sherpa-onnx's own native Parakeet-v3 export**: `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8`,
   documented on k2-fsa's own pretrained-models page and distributed via
   k2-fsa's own release channel, is packaged specifically for sherpa-onnx — this is the
   most first-party-feeling Rust-reachable Parakeet path available (still not
   NVIDIA-shipped, but maintained by the same org that maintains the crate you'd use to
   load it).
2. **ONNX export + sherpa-onnx / ort (community, `onnx-asr`-flavored)**:
   `istupakov/parakeet-tdt-0.6b-v3-onnx` on Hugging Face — confirmed to exist, license
   **CC-BY-4.0** (inherited from NVIDIA's model card), tagged for the `onnx-asr` Python
   project rather than for sherpa-onnx specifically. **Correction**: the file layout is
   `encoder-model.onnx` + `decoder_joint-model.onnx` (plus `.int8` quantized variants
   and a `nemo128.onnx` feature-extractor graph) — **two** ONNX graphs with decoder and
   joiner fused into one file, not three separate encoder/decoder/joiner graphs as an
   earlier draft of this doc stated. It is a viable input to sherpa-onnx/ort, but item 1
   above is the more natural choice since it's built for that runtime already.
3. **`parakeet-rs` (gpu-cli)**: pure Rust, built on Candle, with a `--features metal`
   Apple Silicon path and a NeMo-checkpoint-to-SafeTensors conversion script. Repo and
   numbers **confirmed to exist** (`github.com/gpu-cli/parakeet-rs`, created
   2026-03-20, 1 star, 9 commits per the GitHub API, no tagged release). The README
   reports **3.83% WER on LibriSpeech test-clean (500 samples)** and **RTF 0.131 (7.6x
   realtime), measured on Apple Silicon with `--features metal`** — the hardware
   qualifier is stated in the README itself, so it's not purely undocumented. Critically,
   **the same README states NVIDIA's own published WER for this model is 1.93%** — i.e.
   this Candle reimplementation is running at roughly **2x the error rate** of the
   original NeMo/PyTorch model it's reimplementing, on the author's own numbers. That
   comparison did not appear in an earlier draft of this doc and materially changes how
   "promising" the numbers should read: fast and self-reported, but a meaningfully
   worse model than the thing it's approximating. The README states an MIT license, but
   no `LICENSE` file exists in the repo (404 at the expected path) — minor inconsistency,
   worth a heads-up if this project is ever vendored. Still not something to depend on
   without vendoring/forking and running your own eval — small sample size (500
   utterances), no CI badge observed, single-contributor, four-month-old repo.
4. Report honestly: **no mature, widely-adopted, first-party-from-NVIDIA Rust crate for
   Parakeet exists as of this research (Aug 2026).** But the more accurate framing is:
   sherpa-onnx (k2-fsa's own org, same org that ships the Rust crate) already documents
   and distributes a native Parakeet-v3 `int8` export — that's a safe default today.
   The 9-commit experimental Candle port is a separate, much riskier bet with a
   confirmed ~2x-worse WER than the model it targets.

## Moonshine, Kyutai STT, FluidAudio, WhisperKit

- **Moonshine** (usefulsensors/moonshine-ai): claims better accuracy than Whisper
  large-v3 at ~250M params for its newest streaming English model.
  **Resolved, was `[unverified]`**: this checks out independently. Moonshine's Medium
  Streaming model beats Whisper large-v3's word-error rate on the Hugging Face Open ASR
  Leaderboard despite using roughly 6x fewer parameters (250M vs. large-v3's ~1.5B) —
  consistent with the project's own README claim, on an independent leaderboard rather
  than only the project's self-reported numbers. Ships a C++ core over
  ONNX Runtime with "native interfaces for high-level languages" — reachable from Rust
  via `ort` directly, or via sherpa-onnx (which lists Moonshine as a supported ASR
  model), or via the `transcribe-rs` crate which explicitly lists Moonshine support.
  There's also a standalone `voice-stt` crate (0.1.0, 2026-03-20, MIT) — but it's
  **MLX-backed**, not ONNX/ort-backed, so it inherits the exact "not available outside
  macOS+Python-adjacent-tooling" constraint this whole research exercise is trying to
  get away from. Don't reach for `voice-stt` for a pure-Rust cross-platform build.
- **Kyutai STT**: **Correction to an earlier draft of this doc**, which claimed "no Rust
  crate or ONNX export surfaced." That was wrong — Kyutai's dedicated STT/TTS repo,
  `kyutai-labs/delayed-streams-modeling`, ships a genuine Rust path for STT specifically:
  a standalone `stt-rs` example (`cd stt-rs && cargo run --features cuda -r -- audio.mp3`,
  with `--timestamps` and `--vad` flags) plus a `moshi-server` Rust crate
  (`cargo install --features cuda moshi-server`) that runs STT streaming as a server.
  This is separate from the `kyutai-labs/moshi` repo (the full-duplex dialogue model),
  whose Rust code is a Mimi audio-codec implementation plus a dialogue backend, not a
  standalone STT path — the two repos are easy to conflate, and the original search
  likely landed on `moshi` and stopped there. If Kyutai's model becomes a contender,
  `delayed-streams-modeling` is the repo to evaluate, and it already has a Rust story
  — this changes it from a "no Rust path" entry to a candidate worth benchmarking
  alongside whisper-rs/sherpa-onnx.
- **FluidAudio**: Swift, Apple-platform-only (uses CoreML/ANE directly). Not reachable
  from Rust except via a subprocessed Swift helper binary — same FFI/subprocess pattern
  already recommended for macOS audio capture in the prior research doc. No Linux story
  at all.
- **WhisperKit** (argmaxinc): also Swift/CoreML, macOS+iOS only, no Rust bindings found.
  Same subprocess-helper-or-nothing situation as FluidAudio.

Both FluidAudio and WhisperKit are exactly the kind of native-Swift-helper-over-stdout
pattern the audio-capture research already validated for Core Audio taps (AudioTee
reference). If meetrs ever needs best-in-class on-device macOS accuracy and is willing
to ship a second, Swift-compiled binary, either is a stronger accuracy/speed bet than
anything currently reachable in pure Rust — at the cost of a second toolchain
(Xcode/swiftc) in the build, and zero portability to Linux.

## Diarization from Rust — and whether v1 needs it

- **pyannote ONNX exports**: the *segmentation* model exports to ONNX cleanly via
  standard PyTorch export. The *embedding* model (wespeaker-based) has known export
  friction because its internal fbank feature extraction uses torchaudio ops that don't
  trace to ONNX — community forks (e.g. `samson6460/pyannote-onnx-extended`) work around
  this by re-implementing feature extraction outside the traced graph.
- **`pyannote-rs`** crate: 0.3.4, published 2025-09-07, MIT — slower-moving than the
  sherpa-onnx ecosystem; runs segmentation via `ort` with a sliding 10s window, and
  embeddings via `knf-rs` for fbank extraction (sidestepping the torchaudio-export
  problem by not using torchaudio in Rust at all). CPU-only as far as this research
  confirmed.
- **sherpa-onnx speaker segmentation**: first-party, ships pyannote-segmentation-3.0 +
  3D-Speaker/NeMo embeddings together, more actively maintained than standalone
  `pyannote-rs`. If diarization is ever needed, this is the more defensible choice over
  hand-rolling `pyannote-rs` + `knf-rs`.
- **3D-Speaker (CAM++) embeddings**: ONNX-portable, used by sherpa-onnx's diarization
  pipeline; no separate Rust crate needed since sherpa-onnx wraps it.
- **Does the channel-split trick make diarization unnecessary for v1?** Per the prior
  research doc, meetrs' fixed 4-channel capture (ch0/1 = system, ch2/3 = mic) already
  gives "me vs. them" without any diarization model, by downmixing and transcribing each
  channel independently and merging on the shared clock. That fully covers 1:1 calls.
  It does **not** cover multi-party system audio (a Teams/Zoom call where "them" is
  three different people on one mixed system-audio channel) — for that case only,
  per-system-channel diarization (via sherpa-onnx) would add value. Recommendation
  stands: **skip diarization for v1**, revisit specifically for the multi-party-system-
  channel case in v2, using sherpa-onnx's built-in pipeline rather than `pyannote-rs`.

## Subprocess/FFI escape hatches

- **Cost of a Python helper** (e.g. `parakeet-mlx` subprocessed, emitting JSON over
  stdout): fastest path to today's best macOS accuracy/speed numbers (per prior
  research: ~50-65x realtime; the ~6.3% WER figure for parakeet-tdt-0.6b-v3 **is now
  independently confirmed by this doc's own research** — it's the model's average WER
  on the Hugging Face Open ASR Leaderboard's 8-dataset average (AMI, Earnings-22,
  GigaSpeech, LibriSpeech test-clean/test-other, SPGI Speech, TEDLIUM-v3, VoxPopuli):
  6.32-6.34% for parakeet-tdt-0.6b-v3 vs. 7.44% for Whisper large-v3 on the same
  leaderboard. This is a multi-dataset average, not a LibriSpeech-only number — don't
  confuse it with the `parakeet-rs` Candle port's separate 3.83% LibriSpeech-test-clean-only
  self-report above, which is a different benchmark on a different (worse) model).
  Cost: bundling a Python runtime + MLX + model
  weights inside a distributable Rust app, cross-compilation/signing complexity, and an
  IPC boundary (stdout framing, error handling, process lifecycle) that has to be built
  and tested regardless of which side does the ML.
- **Cost of a Swift helper** (WhisperKit or FluidAudio): same IPC cost, but a *much*
  lighter runtime dependency (no Python/MLX bundling — Swift + CoreML are part of the
  OS on macOS). Better fit for a macOS-only product; buys nothing on Linux, so meetrs
  would need a second, real Rust-native engine for Linux regardless — meaning the Swift
  helper doesn't reduce total engineering surface, it just improves the macOS ceiling.
- **Cost of pure Rust** (whisper-rs or sherpa-onnx): worse accuracy/speed ceiling today
  than parakeet-mlx, but one engine, one build, one binary, both platforms. No IPC
  boundary, no second runtime to package, no process-lifecycle bugs. This is the
  actual argument for whisper-rs/sherpa-onnx as the v1 pick even though it's not
  state-of-the-art: it collapses macOS+Linux to one code path.
- **Recommendation on this axis**: start pure-Rust (whisper-rs, Metal on macOS / CUDA-or-
  CPU on Linux). Only add a subprocess helper if a concrete accuracy/speed complaint
  shows up against real meeting audio — don't pre-build the IPC boundary speculatively.

## Cloud APIs (fallback tier only — privacy caveat applies)

meetrs is local-first; any cloud transcription tier should be explicitly opt-in and
disclosed, given the legal research already on file about recording consent. Briefly:

- **Deepgram**: crates.io `deepgram` crate, latest **0.10.0**, published 2026-05-12, MIT,
  ~502k total downloads, described on crates.io as a "Community Rust SDK."
  **Resolved, was `[unverified]`**: the repository lives at `github.com/deepgram/deepgram-rust-sdk`
  — under Deepgram's own GitHub org, not a third party — but Deepgram itself still
  labels it "Community" in both the crate description and the repo. Read that as
  "hosted by Deepgram, not held to the same support bar as their official
  Python/JS/Go SDKs" rather than either "fully official" or "random community fork."
- **AssemblyAI**: no dedicated Rust crate found in this research. Would require calling
  their REST/WebSocket API directly with `reqwest`/`tokio-tungstenite` — not a blocker,
  just no SDK convenience layer. `[unverified — gap, not confirmed absence]`.
- **OpenAI Whisper API**: no dedicated Rust crate found beyond generic OpenAI API client
  crates (several exist on crates.io, unverified maturity); same "call the REST API
  directly" situation as AssemblyAI.
- None of these were evaluated for accuracy/latency here — this section is scoped to
  "is there a Rust client," not "should meetrs use a cloud API," which the local-first
  design goal already answers by default (no).

## Open questions

1. Does CoreML EP actually accelerate Parakeet's three-graph ONNX export
   (encoder/decoder/joiner) under `ort`/sherpa-onnx on Apple Silicon, or does it silently
   fall back to CPU for some of the graphs? Needs a hands-on benchmark, not a search.
2. Does sherpa-onnx's own prebuilt binaries/crate actually ship CoreML and CUDA
   execution providers, or only the CPU-only default ORT build? The upstream docs
   fetched here didn't state this explicitly — check `sherpa-onnx`'s build scripts /
   Cargo features directly.
3. What is whisper.cpp's actual GGUF quantization WER/speed table (Q4_0 vs Q5_1 vs Q8_0
   vs f16) from its own benchmark docs — the numbers cited above are secondhand/
   `[unverified]` and should be replaced with the primary table before this doc is used
   to make a model-size decision.
4. Is there a maintained fork of `sherpa-rs` (BenLocal vs Limit-LAB) that's healthier
   than the crates.io-published 0.6.8, or should meetrs standardize on the `sherpa-onnx`
   crate name instead? Not resolved here.
5. ~~Kyutai STT: genuinely unresearched~~ — **resolved**: `kyutai-labs/delayed-streams-modeling`
   has a working Rust STT path (`stt-rs` example, `moshi-server` crate). Open question
   now is accuracy/RTF benchmarking against whisper-rs/sherpa-onnx on real audio — not
   whether a Rust path exists at all.
6. Real WER/RTF numbers for whisper-rs + Metal on an actual meetrs-shaped input (multi-
   speaker, far-field mic, echo-bleed from system audio) don't exist yet anywhere in this
   research — every number above is either a vendor claim or a different project's
   benchmark on different audio. Needs an in-house eval before shipping.

## Sources

- https://crates.io/crates/whisper-rs — version 0.16.0, license, publish date, download/dependent counts
- https://crates.io/crates/whisper-rs-sys — sys-layer version/license pairing with whisper-rs
- https://github.com/tazz4843/whisper-rs (via search) — GitHub mirror, project moved to Codeberg
- https://github.com/sigaloid/mutter (via search) — alternative whisper.cpp binding, unverified maturity
- https://github.com/operator-kit/whisper-cpp-plus-rs (via search) — streaming+VAD variant, unverified maturity
- https://github.com/ggml-org/whisper.cpp — project home; used for release lookup
- https://api.github.com/repos/ggml-org/whisper.cpp/releases/latest — confirmed v1.9.1, published 2026-06-19
- whisper.cpp README/docs (via search summary) — Metal, Core ML/ANE ~3x claim, `-tdrz`/tinydiarize flag, GGUF quantization rule-of-thumb (all flagged unverified above)
- https://github.com/huggingface/candle — project home, Whisper example, Metal/Accelerate backend claims
- https://crates.io/crates/candle-core and /candle-transformers — version 0.11.0, license, publish date 2026-06-26
- https://docs.rs/ort/1.13.0/ort/ and https://crates.io/crates/ort — version 2.0.0-rc.13, license, publish date 2026-07-28, execution-provider feature list
- https://github.com/pykeio/ort — maintainer, successor-to-onnxruntime-rs framing
- https://github.com/k2-fsa/sherpa-onnx — model coverage (Whisper/Moonshine/Zipformer/Paraformer/NeMo-Parakeet), diarization, VAD, NPU acceleration list, Apache-2.0, ~2000 commits
- https://crates.io/crates/sherpa-onnx — version 1.13.4, license, publish date 2026-07-08
- https://crates.io/crates/sherpa-rs — version 0.6.8, license MIT, publish date 2025-10-05 (slower cadence than sherpa-onnx crate)
- https://k2-fsa.github.io/sherpa/onnx/pretrained_models/offline-transducer/nemo-transducer-models.html — confirms sherpa-onnx documents NeMo/Parakeet transducer model support
- https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx — community ONNX export of parakeet-tdt-0.6b-v3
- https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3 — model card, NeMo export path via `.export()`
- https://github.com/gpu-cli/parakeet-rs — pure-Rust Candle+Metal Parakeet port; WER 3.83% (LibriSpeech test-clean, 500 samples) and RTF 0.131 claims, MIT license, low commit/star count (immaturity signal)
- https://crates.io/crates/transcribe-rs — version 0.3.11, license, publish date, model coverage claim (Parakeet/Canary/Moonshine/SenseVoice/GigaAM/Whisper/Whisperfile/OpenAI)
- https://github.com/moonshine-ai/moonshine — project description, ONNX Runtime core, accuracy-vs-Whisper-large-v3 claim (unverified independently)
- https://crates.io/crates/voice-stt — version 0.1.0, MLX-backed (not ONNX), license MIT, publish date 2026-03-20
- https://github.com/RustedBytes/pyannote-rs and https://crates.io/crates/pyannote-rs — version 0.3.4, license MIT, publish date 2025-09-07, ORT-based segmentation + knf-rs fbank embeddings
- https://k2-fsa.github.io/sherpa/onnx/speaker-diarization/index.html — sherpa-onnx diarization pipeline (pyannote-segmentation-3.0 + 3D-Speaker/NeMo embeddings)
- https://github.com/pyannote/pyannote-audio/discussions/1929 — embedding-model ONNX export friction (torchaudio fbank ops)
- https://github.com/samson6460/pyannote-onnx-extended — community pure-ONNX pyannote 3.1 pipeline, workaround for the above
- https://deepgram.com/learn/deepgram-vs-assemblyai-vs-whisper (via search) — Deepgram SDK language coverage claim including Rust
- https://crates.io/crates/deepgram — version 0.10.0, license MIT, publish date 2026-05-12, "Community Rust SDK" framing

## Fact-check log (2026-08-03)

**CONFIRMED (exact match, no change needed):**
- All crates.io version/date/license claims: `whisper-rs` 0.16.0 (2026-03-12, Unlicense), `whisper-rs-sys` 0.15.0 (2026-03-12, Unlicense), `sherpa-onnx` 1.13.4 (2026-07-08, Apache-2.0), `sherpa-rs` 0.6.8 (2025-10-05, MIT), `candle-core`/`candle-transformers` 0.11.0 (2026-06-26, MIT/Apache-2.0), `ort` 2.0.0-rc.13 (2026-07-28, MIT/Apache-2.0), `deepgram` 0.10.0 (2026-05-12, MIT), `transcribe-rs` 0.3.11 (2026-04-07, MIT), `pyannote-rs` 0.3.4 (2025-09-07, MIT), `voice-stt` 0.1.0 (2026-03-20, MIT) — every single one checked against the crates.io API and matched exactly.
- `sherpa-onnx` crate is genuinely the official k2-fsa binding, not a third-party stub — owner is `csukuangfj` (Fangjun Kuang), a core k2-fsa maintainer, repository field points at `k2-fsa/sherpa-onnx`.
- `gpu-cli/parakeet-rs` exists, 1 star, 9 commits (confirmed via GitHub API pagination), created 2026-03-20 — the "1-star, 9-commit, early repo" framing was accurate.
- `parakeet-rs`'s self-reported numbers (3.83% WER LibriSpeech test-clean 500 samples, RTF 0.131, 7.6x realtime) are real numbers from the README, correctly attributed as self-reported/unverified by any third party.
- whisper.cpp Core ML "more than x3 faster" claim — verbatim match to the upstream README.
- `candle-transformers` has no Parakeet/Conformer/TDT/NeMo model in its official model zoo — confirmed by listing the models directory.
- sherpa-onnx documents NeMo transducer models including Parakeet, and diarization (pyannote-segmentation-3.0 + 3D-Speaker/NeMo) — confirmed via the k2-fsa docs pages.
- sherpa-onnx's pyannote-segmentation-3.0 export is distributed as an ungated plain GitHub release file, sidestepping HF's own gating on that model — confirmed via the `speaker-segmentation-models` release tag.
- `istupakov/parakeet-tdt-0.6b-v3-onnx` exists on Hugging Face and is publicly accessible (not gated).

**CORRECTED (said → true, with source):**
- Kyutai STT Rust path: said *"no Rust crate or ONNX export surfaced... `[unverified — gap]`"* → **true: Kyutai's `delayed-streams-modeling` repo has a working Rust STT path** (`stt-rs` standalone example, `moshi-server` crate) — [github.com/kyutai-labs/delayed-streams-modeling](https://github.com/kyutai-labs/delayed-streams-modeling). The original research likely stopped at `kyutai-labs/moshi` (which is Rust-implemented but for the Mimi codec/dialogue backend, not standalone STT) and didn't find the dedicated STT repo.
- istupakov's ONNX export shape: said *"encoder/decoder/joiner, matching NeMo's own `.export()` output shape"* (implying 3 graphs) → **true: 2 graphs** — `encoder-model.onnx` + `decoder_joint-model.onnx` (decoder and joiner fused into one file), plus int8 quantized variants and a separate `nemo128.onnx` feature extractor.
- istupakov export license: unstated → **CC-BY-4.0** (inherited from NVIDIA's model card), confirmed via the HF API.
- Framing of "the" Rust Parakeet path: said sherpa-onnx would need to load the *community* `istupakov` export → **sherpa-onnx ships its own native `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` export**, documented on k2-fsa's own pretrained-models page — a more first-party option than the doc credited. Recommendation section updated accordingly.
- Moonshine vs. Whisper large-v3 accuracy claim: said `[unverified — no independent benchmark fetched]` → **confirmed independently**: Moonshine Medium Streaming beats Whisper large-v3's WER on the HF Open ASR Leaderboard at ~250M vs. ~1.5B params.
- Parakeet-tdt-0.6b-v3 "~6.3% WER vs. Whisper large-v3 7.44%" (carried forward uncited from prior doc): now sourced and precise — **HF Open ASR Leaderboard, 8-dataset average (AMI, Earnings-22, GigaSpeech, LibriSpeech test-clean/test-other, SPGI Speech, TEDLIUM-v3, VoxPopuli): 6.32-6.34% for Parakeet-TDT-0.6B-v3 vs. 7.44% for Whisper large-v3.** This is a multi-dataset average, not a LibriSpeech-only number — worth distinguishing from `parakeet-rs`'s separate LibriSpeech-only self-report.
- whisper-rs Cargo features: doc implied Metal was compile-flag-driven but didn't mention it (or Core ML) as an explicit, documented feature — **confirmed both `metal` and `coreml` are first-class Cargo features** in `Cargo.toml`, alongside `cuda`, `hipblas`, `vulkan`, `openblas`, `intel-sycl`, `openmp`.
- Deepgram crate maintenance: `[unverified]` whether Deepgram maintains it directly → **resolved: yes, hosted under Deepgram's own GitHub org, but still self-labeled "Community Rust SDK,"** not their flagship-tier SDK.
- Added a new, materially important number that was missing entirely: `parakeet-rs`'s own README states **NVIDIA's published WER for parakeet-tdt-0.6b-v3 is 1.93%**, meaning the Candle reimplementation's self-reported 3.83% is roughly **2x worse** than the model it targets — this context was absent from the original doc and changes how "promising" that project should read.

**STILL UNVERIFIED (correctly flagged, not resolved by this pass):**
- Whether CoreML EP actually accelerates Parakeet's ONNX graphs under `ort`/sherpa-onnx on Apple Silicon vs. silently falling back to CPU.
- Whether sherpa-onnx's own prebuilt binaries/crate ship CoreML and CUDA execution providers by default, or only CPU-only ORT.
- whisper.cpp's exact GGUF quantization WER/speed table (Q4_0/Q5_1/Q8_0/f16) — the doc's community rule-of-thumb numbers were not re-verified against whisper.cpp's own benchmark docs in this pass either; still needs the primary table.
- Whether a healthier fork of `sherpa-rs` (BenLocal vs. Limit-LAB) exists beyond the crates.io-published 0.6.8.
- Real WER/RTF numbers for whisper-rs + Metal on meetrs-shaped audio (multi-speaker, far-field, echo-bleed) — no in-house eval exists yet; every number in this doc remains a vendor claim or a different project's benchmark on different audio.

## Did the Recommendation change?

**Yes, on one specific point.** The "Parakeet-level accuracy" fallback recommendation now
points first at sherpa-onnx's **own native** `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8`
export rather than the community `istupakov` ONNX conversion — this is a stronger, more
maintained path than the original doc credited, though still not as good as
`parakeet-mlx` on accuracy. The `parakeet-rs` (gpu-cli) experimental option got
meaningfully *more* cautionary framing, not less: its own README shows ~2x NVIDIA's
published WER for the model it's reimplementing, which wasn't visible in the original
doc. The primary macOS recommendation (`whisper-rs` + Metal as the pragmatic v1 choice)
and the "skip diarization for v1" call both survived fact-checking unchanged — every
claim underneath them held up. (`https://crates.io/api/v1/crates/<name>`) — used directly for whisper-rs, sherpa-rs, sherpa-onnx, ort, candle-core, candle-transformers, transcribe-rs, pyannote-rs, deepgram, voice-stt, whisper-rs-sys, kalosm, whisper-cpp/whisper_cpp version/license/date ground truth (re-verified 2026-08-03, exact match on every version/date/license claim)
- https://raw.githubusercontent.com/tazz4843/whisper-rs/master/Cargo.toml — confirmed Cargo feature list: metal, coreml, cuda, hipblas, vulkan, openblas, intel-sycl, openmp
- https://github.com/ggml-org/whisper.cpp README — verbatim Core ML "more than x3 faster" quote
- https://crates.io/api/v1/crates/sherpa-onnx/owners — confirmed sole owner csukuangfj (Fangjun Kuang), core k2-fsa maintainer
- https://api.github.com/repos/gpu-cli/parakeet-rs — confirmed repo exists, 1 star, created 2026-03-20, 9 commits (via commits pagination)
- https://raw.githubusercontent.com/gpu-cli/parakeet-rs/main/README.md — confirmed WER 3.83%, RTF 0.131, 7.6x realtime, Apple Silicon/Metal hardware context, and NVIDIA's own published 1.93% WER comparison; MIT license claimed in README but no LICENSE file present in repo (404)
- https://huggingface.co/api/models/istupakov/parakeet-tdt-0.6b-v3-onnx — confirmed public (not gated), license cc-by-4.0, tags include onnx-asr
- istupakov/parakeet-tdt-0.6b-v3-onnx file listing (via HF tree view) — confirmed encoder-model.onnx + decoder_joint-model.onnx (2 graphs, decoder+joiner fused), plus int8 variants and nemo128.onnx feature extractor
- https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3 model card — confirmed 6.32-6.34% average WER on HF Open ASR Leaderboard 8-dataset average
- HF Open ASR Leaderboard (via search) — confirmed parakeet-tdt-0.6b-v3 6.32% vs Whisper large-v3 7.44%; confirmed Moonshine Medium Streaming beats Whisper large-v3 WER at ~250M vs ~1.5B params
- https://api.github.com/repos/k2-fsa/sherpa-onnx/releases/tags/speaker-segmentation-models — confirmed sherpa-onnx-pyannote-segmentation-3-0.tar.bz2 is a plain, ungated GitHub release asset
- https://github.com/kyutai-labs/delayed-streams-modeling — confirmed genuine Rust STT path: `stt-rs` standalone example and `moshi-server` crate, correcting the earlier "no Rust path" claim (which conflated this repo with `kyutai-labs/moshi`)
- https://api.github.com/repos/deepgram/deepgram-rust-sdk — confirmed hosted under Deepgram's own GitHub org, self-labeled "Community Rust SDK"
