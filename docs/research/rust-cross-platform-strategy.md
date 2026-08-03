# Cross-platform architecture strategy for meetrs

Scope: the seam between platform-specific capture and everything else. Not a macOS
capture guide (see `audio-recording-and-processing.md`, `quill-prior-art.md`) and not a
Linux/PipeWire deep-dive or transcription-engine survey — those are sibling documents.
This one asks: how do you structure *one* Rust codebase so platform code stays thin,
swappable, and testable, and what parts of the problem refuse to be abstracted no matter
how the trait is drawn.

## Recommendation

Ship a single `capture` crate with one small trait (`CaptureBackend`, four methods:
`start_mic`, `start_system`, `stop`, `devices`) implemented by three thin platform
modules gated with `#[cfg(target_os = ...)]`, not `dyn` trait objects behind a
runtime-dispatched factory — there is exactly one backend per build target, chosen at
*compile* time, so an enum or `cfg`-selected type alias is enough; a trait exists only so
the rest of the app (session manager, disk writer, UI) can be written once against a
`Box<dyn CaptureBackend>` in tests and against the concrete type in production. Each
platform module's only job is to turn its native push model (Core Audio IOProc callback,
PipeWire main-loop callback, WASAPI polling thread) into the same output shape: an
`mpsc`/`crossbeam` channel of raw interleaved `f32` frames plus a `StreamFormat` read
back from the OS, never assumed. Do **not** try to unify mic+system into one clock
model in the trait — carry quill's two-independent-tracks-plus-timestamp-reconciliation
design (see `quill-prior-art.md` §2) into the cross-platform version too, because
PipeWire and WASAPI don't offer an aggregate-device equivalent either; a single common
"synchronized capture" abstraction would be modeling a macOS-only mechanism (Core Audio
aggregate devices) as if it were portable, and it isn't. Put permissions, device
enumeration UI, and per-app audio identification *outside* the trait entirely as
platform-specific, optional, capability-queried side interfaces — most callers don't
need them and Linux/Windows can't satisfy the macOS shape anyway. Transcription and LLM
summarization are fully platform-agnostic except for compute backend (Metal / CUDA /
CPU), which is a Cargo feature flag on the inference crate, not a `cfg(target_os)` split
in application code.

## The capability matrix

The core deliverable. "Possible" means shipped in a real OS API today, not a hack;
minimum OS version is for the *primitive*, not for whatever wrapper crate exists.

| Capability | macOS | Linux (PipeWire) | Windows |
|---|---|---|---|
| **Mic capture** | Core Audio HAL / `AVAudioEngine` input node. Any macOS version `cpal` supports. | PipeWire client capturing from a source/mic node, or ALSA directly. Any modern distro with PipeWire (≥0.3, i.e. any 2021+ desktop distro) or plain ALSA as fallback. | WASAPI capture client. Any Win10+. |
| **System/output capture** | ScreenCaptureKit (mic+system unified, macOS 13.0 system-only / 15.0 for mic-via-SCK) *or* `AudioHardwareCreateProcessTap` (macOS 14.2+ introduces the API; the companion `rust-audio-macos.md` treats **14.4** as the practical recommended floor, avoiding a rough edge in 14.2/14.3 — pick 14.4 as the real target, not 14.2). See `audio-recording-and-processing.md` §1. | PipeWire monitor source on the default sink (`*.monitor`), or capture directly from the sink's PipeWire node as an input stream — no special permission, no kernel module, works today on any PipeWire desktop. This is *categorically easier* than macOS: PipeWire was designed with this as a first-class use case. | WASAPI loopback capture on the render endpoint (`AUDCLNT_STREAMFLAGS_LOOPBACK`). Win10+. `cpal` itself *has* implemented whole-system WASAPI loopback since [RustAudio/cpal#339](https://github.com/RustAudio/cpal/pull/339) (merged 2019, closing the original request [#251](https://github.com/tomaka/cpal/issues/251)) — the claim that cpal "doesn't do this at all" is false. The reasons to still prefer the dedicated `wasapi` crate (HEnquist) are narrower and confirmed below: a documented history of the flag getting dropped during refactors ([#476](https://github.com/RustAudio/cpal/issues/476), closed 2020) and, more importantly, `wasapi` supports **per-process** loopback (`AudioClient::new_application_loopback_client(pid, include_tree)`), which cpal has no equivalent for at all. |
| **Per-application audio capture** | ScreenCaptureKit can filter to specific running apps/processes (`SCContentFilter` with app exclusion/inclusion) — macOS 13+. Process taps can also target a specific PID via `CATapDescription(processes:)`. | PipeWire is node-per-stream by design: every app's output is its own node with metadata (`application.name`, `application.process.id`), and a client attaches to a specific node via `target.object`. This is the *native* PipeWire model, not a workaround — see the "per-app" section below. | **Confirmed, not unverified.** Windows 10 2004+ (build 19041, activation constant confirms build 20348+ per Microsoft's own sample) exposes per-process loopback via `ActivateAudioInterfaceAsync` + `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`, and the `wasapi` crate (HEnquist) **already wraps it**: `AudioClient::new_application_loopback_client(pid: u32, include_tree: bool)` is a real, present method in the crate today — verified by reading its docs.rs page. The earlier `[unverified]` marker here was wrong; no custom FFI is needed to reach this API from Rust. |
| **Synchronized mic + system (one clock)** | Core Audio aggregate device holding both the tap and the physical input, `kAudioSubTapDriftCompensationKey: true` → one IOProc, one clock, hardware-level drift compensation. macOS-only mechanism, no analog elsewhere. | No equivalent primitive. PipeWire has a shared graph clock across nodes it manages *if you build an explicit link/loopback module for it*, but there is no "put a mic and a monitor source in one aggregate" one-liner; day-to-day this ends up being two independent PipeWire streams like quill's two-graph model, reconciled by timestamp. | No equivalent primitive either. Two independent WASAPI streams (capture + loopback), reconciled by timestamp, same as Linux. |
| **Per-app metadata (which app is making sound)** | `SCRunningApplication` from ScreenCaptureKit, or the process list from a process tap's `CATapDescription`. Requires screen-recording-class TCC consent. | Free and always-on: every PipeWire node carries `application.name`/`application.process.id`/`application.icon` in its properties, queryable with no special permission — this is just how the graph is shaped, not a capture feature you opt into. | Session/process enumeration via WASAPI's `IAudioSessionManager2`/`IAudioSessionControl2`, no extra permission needed (unlike macOS). |

Reading the table: **macOS is the platform where each capability requires a distinct,
gated, versioned API and a permission prompt; Linux/PipeWire is the platform where all
of this — including per-app identification — is just what the audio graph looks like,
free, with no consent dialog because there is no OS-level audio privacy model on
Linux at all.** That asymmetry is not a Rust problem to abstract away; see "what must
not be abstracted" below.

## Abstraction design in Rust

### Why not a bigger trait, and why not enum dispatch either

Two tempting shapes to reject up front:

- **A trait mirroring the union of all platform features** (aggregate devices, per-app
  filters, TCC state, PipeWire node metadata as one interface) forces every platform to
  either fake support for features it doesn't have or return `NotSupported` from most
  methods most of the time. That's an abstraction modeling the union of three OSes'
  audio stacks, which is a bigger and buggier surface than any one OS's real API.
- **Enum dispatch** (`enum Backend { Mac(MacCapture), Linux(PwCapture), Win(WasapiCapture) }`)
  buys nothing over a plain `#[cfg(target_os = ...)] pub type Backend = ...;` type alias,
  because exactly one variant ever exists in a given compiled binary — the enum's other
  arms are dead code paid for in match-exhaustiveness boilerplate. Reach for an enum only
  if you truly need multiple backends live in one process (e.g. a debug build that can
  also run the null/fixture backend for testing) — which, notably, meetrs does want, so
  a *small* enum (`Real(PlatformBackend) | Null(NullBackend)`) selected by a feature flag
  or CLI arg is reasonable; a three-OS enum is not.

### The actual trait

Keep it to the shape every platform can satisfy identically, and push everything
push-based into a channel so the trait doesn't need to know whether the OS calls back
on a system thread (macOS/Windows) or drives its own event loop that must be pumped
(PipeWire's `pipewire::main_loop::MainLoop` blocks the thread that owns it):

```rust
// crates/capture/src/lib.rs

pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,           // read back from the OS, never hardcoded
    pub sample_format: SampleFormat, // f32 | i16 | ...
}

pub enum AudioFrame {
    Data { pts: std::time::Instant, samples: Vec<f32> }, // always downmixed/interleaved f32
    StreamError(CaptureError),
    Started { format: StreamFormat },
    Stopped,
}

/// One capture session on one logical source (mic OR system, never both).
/// Implementors own a background thread/callback and forward everything
/// through `tx`; `stop()` must be safe to call from any thread and must
/// block until teardown is complete (mirrors quill's explicit
/// stop→cleanup ordering — no half-torn-down state).
pub trait CaptureBackend: Send {
    fn start_mic(&mut self, tx: Sender<AudioFrame>) -> Result<(), CaptureError>;
    fn start_system(&mut self, tx: Sender<AudioFrame>) -> Result<(), CaptureError>;
    fn stop(&mut self);
    fn devices(&self) -> Vec<DeviceInfo>; // best-effort; Linux/macOS can both answer this
}

#[cfg(target_os = "macos")]
pub use macos::MacBackend as PlatformBackend;
#[cfg(target_os = "linux")]
pub use linux::PipewireBackend as PlatformBackend;
#[cfg(target_os = "windows")]
pub use windows::WasapiBackend as PlatformBackend;
```

Module layout:

```
crates/capture/
  src/
    lib.rs            # trait + shared types (AudioFrame, StreamFormat, CaptureError)
    null.rs           # NullBackend — silence generator, for tests/CI, all platforms
    fixture.rs         # FixtureBackend — replays a recorded PCM file as if it were live capture
    macos/
      mod.rs           # MacBackend: owns the Core Audio tap + AVAudioEngine mic path,
                        # bridges IOProc/AVAudioEngine callbacks into `tx.send()`
      tap.rs, mic.rs, aec.rs   # internals mirroring quill's file split
    linux/
      mod.rs           # PipewireBackend: owns a MainLoop on a dedicated thread,
                        # bridges stream `process` callbacks into `tx.send()`
      pipewire_stream.rs
    windows/
      mod.rs           # WasapiBackend (feature-gated, not built by default yet)
```

**Where the seam actually goes**: not at the trait method boundary alone, but at the
*thread* boundary. Every platform's native capture mechanism is push-based from code
Rust doesn't control the timing of — a Core Audio IOProc runs on a system-managed
real-time thread, PipeWire's `Stream::add_local_listener().process()` callback runs on
whatever thread is pumping that backend's `MainLoop`, and a WASAPI event-driven capture
loop runs on a thread you spawned and event-wait on. The trait's job is just to say "give
me a channel, and by the time `start_*` returns, someone is feeding it" — the callback
adaptation is 100% internal to each `mod.rs` and never leaks into the channel item type
or the caller. This keeps platform code "thin" in the sense that matters: adapter code
around a callback, not business logic. Segmentation, VAD, disk writing, and the mixer
all live above the trait and are written once.

### Where platform code stays thin vs. where it's inherently thick

- **Thin and mechanical** (belongs behind the trait, one clear job): pulling PCM out of
  whatever native callback exists and putting it on a channel with a format tag.
- **Thick and platform-specific, correctly so** (does *not* belong behind the trait,
  gets its own module with its own API, callers opt in explicitly):
  - macOS AEC (`voiceProcessingEnabled`, duplex graph — see `quill-prior-art.md` §4) —
    exposed as `macos::aec::configure(...)`, called only from `macos/mod.rs`.
  - PipeWire per-app target selection (`target.object` property, node enumeration) —
    exposed as `linux::app_targets() -> Vec<AppAudioSource>`, with no macOS/Windows
    equivalent function at all, not a stub returning `vec![]`.
  - TCC status/prompting on macOS — exposed as `macos::permissions::mic_status()`;
    simply doesn't exist as a symbol on other platforms, rather than a cross-platform
    `permissions::status()` that always returns `Granted` on Linux. A caller checking
    permissions has to know it's on macOS; that's honest, not extra ceremony.

## What must not be abstracted

A narrower common interface is the goal, not an accident of laziness. Specific things
that should be surfaced as platform-specific APIs, never unified:

- **Permissions.** macOS TCC is a stateful, per-code-signing-identity, sometimes
  reset-on-rebuild consent system with distinct prompts for mic vs. screen/system-audio
  (see `quill-prior-art.md` §8). Linux has *no* OS-level audio permission model — any
  process can open any PipeWire node. Windows has its own privacy-settings surface
  (Settings → Privacy → Microphone) that's OS-managed, not app-queryable the way TCC is.
  A `PermissionState` enum shared across all three would either lie on Linux (claiming a
  "granted" state nothing ever checked) or force Linux to import macOS's mental model.
  Model it as `macos::permissions`, full stop, called from an `if cfg!(target_os =
  "macos")` block in the one place the UI needs to show a permission prompt.
- **Device/graph models.** Core Audio's device graph (HAL devices, aggregate devices,
  taps, IOProcs) and PipeWire's graph (nodes, ports, links, session-manager-owned
  routing policy) are different enough in *kind*, not just API shape, that a shared
  `Device`/`Graph` type would have to either drop PipeWire's per-app node metadata or
  fabricate macOS concepts PipeWire doesn't have. Keep `DeviceInfo` in the trait
  deliberately dumb (id, name, is_default) — anything richer belongs in the platform
  module.
- **Channel layouts.** macOS process taps hand you the tap's *actual* negotiated
  format, read back, which can be mono, stereo, or (per the quill AEC RCA) an
  unexpected 9-channel duplex format. PipeWire streams likewise report their own
  negotiated format. There is no cross-platform guarantee of stereo, so the common
  `StreamFormat` in the trait must be read from the OS every session, never assumed, and
  downmix-to-target-channel-count is app logic above the trait, not baked into it.
- **Clocking.** Already covered above: no unified sync primitive exists, so don't
  pretend one does by giving the trait a `start_synchronized(mic, system)` method that
  only macOS can honor precisely. Two independent `start_mic`/`start_system` calls plus
  app-level timestamp reconciliation (quill's model) is the *correct* cross-platform
  answer, not a compromise forced by Rust's limitations.

## Cargo mechanics

Platform dependencies go under `[target.'cfg(target_os = "...")'.dependencies]`, which
Cargo resolves per build target — a `cargo build` on Linux never touches the macOS
table, so it never fetches, let alone compiles, `objc2`/`core-foundation`/
`coreaudio-sys`:

```toml
[dependencies]
# platform-agnostic: channel, error types, the trait itself
crossbeam-channel = "0.5"
cpal = "0.18"              # cross-platform; verified its own Cargo.toml gates
                            # objc2/coreaudio-* under Apple-only cfg and
                            # alsa/pipewire/pulseaudio under Linux/BSD cfg —
                            # a Linux build of cpal never touches objc2.

[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.6"
objc2-foundation = "0.3"
coreaudio-rs = "0.14"

[target.'cfg(target_os = "linux")'.dependencies]
pipewire = "0.10"          # gitlab.freedesktop.org/pipewire/pipewire-rs

[target.'cfg(target_os = "windows")'.dependencies]
wasapi = "0.23"            # HEnquist/wasapi-rs — cpal alone doesn't reliably do loopback
```

(Versions current as of this doc's writing; re-check crates.io before pinning — this is
a fast-moving corner of the ecosystem.)

Notes:

- `cpal` itself is cross-platform (RustAudio/cpal) and needs only one entry in the
  platform-agnostic table — confirmed by reading cpal's own `Cargo.toml` directly (not
  just its README) on 2026-08-03. One correction from an earlier pass of this doc: the
  Apple-only gate is `cfg(target_vendor = "apple")`, not `cfg(target_os = "macos")` —
  cpal shares one Apple dependency block (`coreaudio-rs`, `objc2`, `objc2-core-audio`,
  `objc2-audio-toolbox`, `objc2-core-audio-types`, `objc2-core-foundation`,
  `objc2-foundation`, `mach2`) across macOS/iOS/tvOS/visionOS, then layers a small
  macOS-specific `cfg(target_os = "macos")` block on top for the optional `jack`
  feature. On Linux/BSD, `alsa`/`alsa-sys`/`libc` are the only unconditional deps;
  `jack`/`pulseaudio`/`pipewire` are all `optional = true` Cargo features, not pulled in
  by default. **A Linux build of cpal does not transitively pull in objc2** — the
  "Linux users shouldn't compile objc2" goal holds without meetrs doing anything extra,
  as long as meetrs's own platform deps follow the same `cfg` split cpal already uses
  internally (matching on `target_vendor = "apple"` if meetrs wants exactly cpal's own
  shape, or `target_os = "macos"` if meetrs only ever targets desktop macOS and doesn't
  need the iOS/tvOS/visionOS reach).
- Feature flags layer on top for optional capability, not OS selection: e.g. a
  `system-audio-per-app` feature that's meaningful on Linux (real PipeWire node
  filtering) and macOS (ScreenCaptureKit app filter) but simply absent — not
  `#[cfg(not(...))] compile_error!` — on a hypothetical fourth target, so downstream
  crates that don't care about per-app filtering don't need to reason about it at all.
- **Conditional-compilation testing**: `cargo check --target x86_64-unknown-linux-gnu`
  and `cargo check --target aarch64-apple-darwin` from either machine catches most
  `cfg`-block typos and missing-import errors without needing the other OS's toolchain
  installed, *as long as the platform crates in the other table don't need to actually
  link* — which for `objc2`/`coreaudio-rs` they do at final link time, not at `cargo
  check` time, so `check` (not `build`) is the cross-target sanity check to run in CI on
  every PR, with real `build`+`test` reserved for native runners per OS.
- **CI**: use `macos-latest` and `ubuntu-latest` GitHub Actions runners directly, one
  job per OS, rather than cross-compiling — this is confirmed as the idiomatic pattern
  for real multi-platform Rust CI (native runner per matrix entry, `cross`/`zigbuild`
  reserved for niche Linux targets a runner doesn't cover, e.g. musl/ARM). **Cross-
  compiling a macOS binary from Linux CI is not realistic and this isn't a tooling gap to
  route around**, confirmed against current tooling docs, not folklore:
  - `cross-rs`'s own README states it does not ship pre-built images for Darwin/MSVC
    targets at all — that support lives in a separate, explicitly unofficial
    `cross-toolchains` repo, not the main supported matrix.
  - `cargo-zigbuild`'s README documents an unresolved upstream Zig linker issue when
    cross-compiling to Darwin targets that need Apple frameworks (e.g.
    `CoreFoundation`), and its Docker images only work at all because they bundle a
    macOS SDK obtained out-of-band — not something Apple's SDK license permits
    redistributing.
  - `osxcross` (the underlying trick both routes ultimately depend on) requires the
    user to separately accept Xcode's license and manually extract an SDK from a real
    Xcode install; it's explicitly not an Apple-sanctioned toolchain and is documented
    as fragile beyond simple binaries.
  - The CI implication: **budget for a real macOS runner** (GitHub-hosted
    `macos-latest`, or a self-hosted Mac) for every build/test/release job touching the
    macOS capture module. There is no crate or CI trick that removes this requirement.
- `cross`/`cargo-zigbuild` are still worth using — for the *Linux* leg: producing a
  `x86_64-unknown-linux-musl` static binary from any host, or eventually a
  Windows `.exe` from Linux CI via `cargo-zigbuild`'s MinGW support, once Windows
  support lands. They just don't solve the macOS leg.

## Windows preview

Brief, because a sibling doc may cover this properly and the point here is just
confirming the abstraction survives Windows arriving later:

- Mic capture: `cpal` already works on Windows (WASAPI shared-mode capture), no new
  trait shape needed.
- System/loopback capture: cpal's WASAPI loopback has a rockier history than a clean
  "just works" story, but "unresolved, not just missing" overstates it after checking
  the actual issue thread. The original request is
  [RustAudio/cpal#251](https://github.com/tomaka/cpal/issues/251) (2018); a loopback
  implementation was merged in
  [RustAudio/cpal#339](https://github.com/RustAudio/cpal/pull/339) (2019-11-17, closing
  #251). [RustAudio/cpal#476](https://github.com/RustAudio/cpal/issues/476) reports the
  loopback flag got dropped during a later refactor (code moved from `stream.rs` to
  `device.rs`) and needed to be re-applied alongside `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`
  — but that issue was **closed 2020-10-28, by the reporter, who supplied the fix
  patch themselves**. No confirmed open regression was found for cpal's current (2026)
  releases in this pass — the "still unreliable" framing in an earlier draft of this doc
  was not supported by evidence and has been walked back. The stronger, confirmed reason
  to reach for the dedicated `wasapi` crate (HEnquist, v0.23, documented loopback
  support) instead of cpal for `windows::WasapiBackend::start_system` is capability, not
  reliability: **cpal has no per-process loopback at all**, and `wasapi` does (see next
  bullet) — that alone is enough to prefer it if per-app Windows capture is ever wanted,
  independent of whether cpal's whole-system loopback is currently solid.
- Per-app: **confirmed available today, not a future custom-FFI project.**
  Process-specific loopback exists at the Windows API level (`ActivateAudioInterfaceAsync`
  + `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`, Windows 10 2004+ / build 20348+) and the
  `wasapi` crate already wraps it as `AudioClient::new_application_loopback_client(pid,
  include_tree)` — verified directly against the crate's docs.rs page. This is a real
  correction to an earlier `[unverified]` marker in this doc: when Windows support is
  actually built, `windows::app_targets()`-style per-app capture can be built on top of
  an existing safe wrapper, the same "thin adapter over a real crate API" shape the
  macOS and Linux modules already have — not custom FFI written from scratch.

## Transcription and LLM boundary

Both are fully platform-agnostic *as application logic* — they consume `AudioFrame`s (or
written PCM/Opus files) and produce text, then text and a prompt and produce a summary.
Nothing about "transcribe this buffer" or "summarize this transcript" needs a
`cfg(target_os)` anywhere in `crates/transcribe` or `crates/summarize`. The only
per-platform variable is *inference compute backend*, and that's a Cargo feature on the
inference crate (`whisper-rs`, `sherpa-onnx`, `ort`, whichever the sibling transcription
doc lands on), not a fork in application code:

```toml
[features]
default = []
metal = ["whisper-rs/metal"]     # or the equivalent for whichever ASR crate is chosen
cuda   = ["whisper-rs/cuda"]
# cpu is the no-feature fallback on every platform
```

A build picks `--features metal` on macOS, `--features cuda` on a Linux box with an
Nvidia GPU, or ships with no GPU feature at all (CPU) on Linux boxes without one — this
is a build-time choice a packaging script makes per target, not a runtime `if
target_os` branch, and it's the same pattern the LLM-call layer should use if/when local
LLM inference (rather than an API call) enters the picture.

## Testing strategy

CI has no audio device, no PipeWire session, no TCC consent, and (per the Cargo
mechanics above) frequently no macOS runner attached to every PR. Structure tests around
that constraint rather than fighting it:

- **`NullBackend`** (all platforms, no `cfg` at all): implements `CaptureBackend` by
  spawning a thread that pushes silence (or a synthetic sine wave for a non-trivial
  signal) at the configured sample rate on a timer. Exercises the session
  manager, disk writer, VAD/segmentation, and channel-teardown logic without touching
  any OS audio API. This is what unit and integration tests default to.
- **`FixtureBackend`**: reads a checked-in short PCM/WAV fixture (a few seconds, real
  recorded speech) and replays it through the same channel at real-time-equivalent
  pacing, so segmentation and transcription-pipeline-glue tests run against real signal
  characteristics (actual silence gaps, actual amplitude) without a live device.
  Multiple fixtures (clean speech, cross-talk, silence-only) cover the segmentation edge
  cases quill's RCA warned about (zero-sample tracks, near-silence).
- **Platform capture modules get their own narrow, mockable seam**: the *adapter*
  function that turns a Core Audio/PipeWire/WASAPI callback into an `AudioFrame` is
  small enough to unit-test with a hand-constructed fake buffer, independent of whether
  the real OS API is reachable — test the conversion logic, not the OS integration.
- **What stays manual**: actually opening a live mic/system tap and verifying real audio
  comes out correctly leveled, actually clicking through a fresh macOS TCC prompt flow,
  actually verifying PipeWire per-app filtering against a real running app. These need a
  human with a real device and, on macOS, a real consent dialog — no CI runner can grant
  TCC non-interactively for a capture app (this is deliberate on Apple's part). Keep a
  short manual test checklist (`docs/manual-testing.md` or similar) run before each
  release rather than pretending a CI green check covers device I/O.
- **CI matrix in concrete terms**: `ubuntu-latest` runs the full Linux capture module
  build/test including any PipeWire integration tests that use `NullBackend`/
  `FixtureBackend` (no real PipeWire session needed for those); `macos-latest` runs the
  equivalent for the macOS module. Neither runner needs a microphone or a granted
  permission because nothing in the automated suite calls the real OS capture API.

## Prior art: cross-platform Rust audio/transcription apps

Honest survey — platforms *actually* wired up (checked against source and CI config,
not just README claims), license, and maturity. `[unverified]` marks anything not
directly confirmed by opening the repo; star counts and exact version numbers here came
from a research pass on live repos rather than from this document's author browsing
GitHub directly, so treat exact figures as approximate and re-check before citing them
externally.

| Project | Repo | License | Claimed platforms | Actual capture platforms | Capture design | System/loopback audio | Local transcription |
|---|---|---|---|---|---|---|---|
| **Handy** | [cjpais/handy](https://github.com/cjpais/handy) | MIT (confirmed) | macOS, Windows, Linux | **Confirmed, not `[unverified]`.** Read the actual release workflow: it builds 7 matrix configs — macOS (Apple Silicon + Intel), Ubuntu 22.04/24.04 (x86_64 + ARM64), Windows (x86_64 + ARM64). All three OSes are genuinely built and CI-tested, exactly as claimed. 28.6K stars, pushed 2026-08-03 (actively maintained). | Tauri (Rust backend); `cpal` for mic capture, resampled to WAV | Mic only — confirmed no loopback/system capture in the docs or feature list | Yes — local Whisper, Parakeet, Moonshine |
| **Vibe** | [thewh1teagle/vibe](https://github.com/thewh1teagle/vibe) | **MIT (confirmed — the `[unverified]` marker was avoidable)** | macOS, Windows, Linux | All three, Tauri-based, 6,992 stars, pushed 2026-07-26. **Live mic capture is real on all three** (`🎤 Transcribe from microphone` ships). **System audio does not yet work — confirmed, not `[unverified]`.** The project's own author opened [RustAudio/cpal#875](https://github.com/RustAudio/cpal/issues/875) ("Record system audio on multiple platforms") and [#876](https://github.com/RustAudio/cpal/issues/876) ("Support ScreenCaptureKit loopback"), and there's an open [PR #894](https://github.com/RustAudio/cpal/pull/894) attempting to add it — i.e. Vibe is *waiting on upstream cpal* for system-audio capture, not shipping it. The README's `⏳ Transcribe system audio` line (hourglass, distinct from the plain 🎤/📂 markers elsewhere) is accurately read as "not yet," matching this. | Tauri + `whisper.cpp` bindings, with GPU acceleration paths (CUDA/ROCm/Metal via whisper.cpp) | **Not yet shipped**, pending upstream cpal support (see above) — this is stronger and more specific than the prior `[unverified]` framing, and it independently corroborates this doc's own finding that a portable system-audio capture layer doesn't exist off-the-shelf in cpal today | Yes — whisper.cpp locally; can also route to Parakeet TDT v3 / cloud LLM summarization |
| **screenpipe** | [screenpipe/screenpipe](https://github.com/screenpipe/screenpipe) | **Screenpipe Commercial License — source-available, not `[unverified]`.** GitHub's own license detection reports "Other (NOASSERTION)"; the repo's `LICENSE.md` states personal/non-commercial use is permitted, commercial use requires a paid license. This is a real, material correction: screenpipe is not simply open source. | macOS, Windows, Linux (Linux documented as build-from-source) | macOS confirmed: docs explicitly reference macOS 14.4+ and the "System Audio Recording Only" TCC permission category (matches the process-tap TCC category confirmed independently in `rust-audio-macos.md`). Windows WASAPI loopback is claimed but not independently verified against source in this pass. **The actual Linux audio-capture code path still could not be confirmed** — this specific gap survives re-verification; still the one to check before citing screenpipe as a working Linux example. 20.7K stars, pushed 2026-08-03 (very active). | Screen + audio capture via platform-specific APIs per OS, not a single shared crate | Claimed on all three, **verified only for macOS** | Local Whisper large-v3-turbo, or cloud (Deepgram) as an alternative |
| **Whisperi** | [xarthurx/whisperi](https://github.com/xarthurx/whisperi) | MIT (confirmed) | macOS, Windows, Linux (via Tauri) | **Confirmed Windows-only in practice** — the project's own description says "Built on Windows, for Windows," explicitly noting Tauri supports macOS/Linux but Whisperi doesn't target them. No CI/release artifacts for the other two OSes. Exactly the "claims cross-platform tooling, ships one OS" case flagged. 21 stars, pushed 2026-07-31, 0 open issues. | Tauri + hotkey-triggered capture | Mic only, plus a beta "live dictation" streaming mode | **No** — cloud-only (OpenAI, Groq, Mistral, Qwen, and OpenRouter APIs, one more provider than previously listed), the one project surveyed that isn't local-transcription at all |
| **owhisper** | [fastrepl/hyprnote](https://github.com/fastrepl/hyprnote) (OWhisper subproject; [docs](https://docs.hyprnote.com/owhisper/what-is-this/)) | `[unverified]` (not independently checked in this pass) | `[unverified]` — installable via `brew tap fastrepl/hyprnote && brew install owhisper`, implying at least macOS/Linuxbrew support; Windows support not confirmed | **This entry was simply wrong before — a real project exists, and the "does not appear to exist" verdict should not have been trusted.** OWhisper is described as "an Ollama-style STT engine that transcribes and summarizes conversations directly on-device," built by the Hyprnote (fastrepl) team as a companion to their local-first meeting-notes app. It is a **transcription-serving engine**, not itself a meeting *recorder* with capture code — different scope than Handy/Vibe/screenpipe/Whisperi, so it's a different kind of prior art (closer to a local model-serving layer meetrs could point at than a capture-architecture comparable). Re-verify platform/capture details directly before citing further; this correction only fixes the "doesn't exist" error. | Server/engine, not a capture app — N/A | `[unverified]` | Yes — on-device STT is its whole purpose |
| **"Whispering"** | [braden-w/whispering](https://github.com/braden-w/whispering) — **archived/moved to [EpicenterHQ/epicenter](https://github.com/EpicenterHQ/epicenter)** | `[unverified]` (not independently checked) | Historically cross-platform via Tauri/web; current status under Epicenter `[unverified]` | **This entry was also wrong before — a real, well-known project by this exact name exists.** "Press shortcut → speak → get text," originally a hosted web app + browser extension + Tauri desktop app, since folded into Epicenter's "local-first apps" ecosystem. **Important scope caveat for this survey: its application logic is TypeScript/Svelte, not Rust** — evidence found describes "a lightweight TypeScript library for error handling" as core to the app, and Epicenter's apps are TS/Svelte-on-Tauri, meaning any Rust in the stack is a thin Tauri shell, not the transcription/capture logic itself. It belongs in this table as a "real project, wrong language for a Rust-architecture comparison" entry, not as further evidence for a Rust capture pattern. | Historically browser/Tauri-based; transcription via cloud Whisper API (OpenAI) rather than local capture-heavy design | `[unverified]` | Cloud (OpenAI Whisper API) in the original app; current Epicenter-era status `[unverified]` |
| **libobs-rs / obs-rs** | [libobs-rs/libobs-rs](https://github.com/libobs-rs/libobs-rs) | `[unverified]` | Windows/Linux bindings exist (v5.0.1 found) | Bindings to libobs exist, but no production Rust *meeting-recorder* app using them was found — this is a bindings crate, not evidence of a shipped cross-platform recorder | Wraps OBS's own C++ capture graph rather than a native-Rust capture path | Whatever OBS itself supports per platform, inherited wholesale | N/A (not a transcription tool) |

Pattern across the survey: every project that ships **local** transcription (Handy,
Vibe, screenpipe) uses `cpal` for mic capture; system audio is either absent (Handy),
not yet shipped and explicitly blocked on upstream cpal work (Vibe — its own author is
one of the people filing cpal issues #875/#876 asking for exactly this), or built with
a separate hand-written platform-specific path outside cpal (screenpipe, confirmed for
macOS only). None of them ship a single shared "system audio" abstraction across all
three OSes — independent support for this doc's recommendation that a unifying
synchronized-capture trait is the wrong shape to build, and a live, current-generation
illustration (not just a historical one) of exactly the gap this doc's capability
matrix identifies. The one project with a real cross-platform-but-actually-single-
platform gap (Whisperi) is a Tauri app that never wired up the other two targets, not a
capture-API limitation — a reminder that "uses Tauri" says nothing about whether the
native capture code was actually written for a target, only that the *shell* could be.
Two names in an earlier pass of this survey (`owhisper`, `"Whispering"`) were reported
as not existing; both are real, named projects — see the corrected table rows above.
Neither changes the pattern-level conclusion (owhisper is a transcription-serving
engine, not a capture app; Whispering's logic is TypeScript, not Rust), but citing a
real project as nonexistent was a research failure worth flagging plainly, not
smoothing over.

## UI/packaging seam

Brief, since it's adjacent rather than core to the capture abstraction, but it does
interact with the permissions story:

- **Tauri**: a real windowed UI, WebView-based, packages as a proper `.app` bundle on
  macOS — which matters because TCC needs an `Info.plist`-bearing bundle (or the
  `__TEXT,__info_plist` linker trick quill uses for a bare binary, see
  `quill-prior-art.md` §8) to attribute a permission prompt to the app at all. Tauri
  gives you that bundle for free and is the natural choice if meetrs wants a real
  settings/review UI.
- **egui**: a native immediate-mode UI, no WebView, still needs the same macOS bundling
  step (an `.app` wrapper or the linker-section trick) for TCC to work — egui doesn't
  solve packaging, it just avoids a WebView dependency.
- **Headless daemon + CLI** (quill's actual shape): smallest footprint, no bundle by
  default, which is exactly why quill had to reach for the `__TEXT,__info_plist`
  linker-section trick to get TCC attribution at all on an unbundled binary. This is the
  cheapest option to build but carries the most packaging risk on macOS specifically —
  Linux has no equivalent bundling requirement since there's no permission system to
  satisfy, so a headless daemon is *strictly simpler* on Linux than on macOS, which is
  one more argument for keeping the permission-prompt/bundling logic entirely inside the
  macOS module rather than something the UI layer has to special-case per platform.

The seam consequence: whichever UI shape is chosen, the macOS packaging requirement
(bundle-of-some-form for TCC attribution) is orthogonal to which UI toolkit is used and
should be solved once, in build tooling, not duplicated per UI choice.

## Open questions

- Whether loose timestamp-based mic/system sync (the quill model, carried over
  cross-platform) holds up acoustically over long (90+ minute) sessions on Linux/Windows
  the same way it appears to on macOS — nobody has measured drift on any platform yet
  (noted as unmeasured in `quill-prior-art.md` too).
- ~~Whether any existing Rust crate wraps Windows per-process loopback capture~~ —
  **resolved in this fact-check pass**: yes, the `wasapi` crate's
  `AudioClient::new_application_loopback_client(pid, include_tree)` wraps it today.
- ~~Whether cpal 0.18.x's WASAPI loopback support... is currently working~~ — **partially
  resolved**: [#476](https://github.com/RustAudio/cpal/issues/476) was closed in 2020 by
  its own reporter with a fix patch, and no open regression was found for current (2026)
  cpal releases in this pass. Still recommend the dedicated `wasapi` crate, but now for
  a confirmed reason (per-process capability cpal lacks entirely) rather than an
  unverified reliability worry.
- Exact maintenance cadence of the `pipewire` Rust binding crate (hosted at
  `gitlab.freedesktop.org/pipewire/pipewire-rs`, currently 0.10.0 per crates.io) — worth
  a freshness check immediately before pinning a version, since this corner of the
  ecosystem moves fast.
- screenpipe's actual Linux audio-capture implementation — claimed in its docs but not
  confirmed against source in this pass; worth a direct look if screenpipe is going to
  be cited as a working Linux precedent rather than just a macOS one.
- Whether Vibe does live mic capture on all three platforms or is primarily an
  import/file-transcription tool with capture as a secondary feature — `[unverified]`,
  noted in the prior-art table.
- Whether ScreenCaptureKit's per-app audio filter and Core Audio process taps'
  per-process filter are redundant or meaningfully different in practice (both listed in
  the matrix) — worth resolving before building the macOS per-app feature.
- No decision recorded yet on whether meetrs commits to Windows support at all versus
  treating this document's Windows row purely as a design-doesn't-need-rework check.

## Sources

- `docs/research/audio-recording-and-processing.md` (this repo) — macOS capture and
  transcription prior research index.
- `docs/research/quill-prior-art.md` (this repo) — Swift/macOS-only reference
  implementation teardown; source of the two-graph/timestamp-reconciliation design and
  the TCC/bundling mechanics cited above.
- [RustAudio/cpal](https://github.com/RustAudio/cpal) — cross-platform Rust audio I/O
  crate; mic capture on all three target platforms.
- [RustAudio/cpal#251 (WASAPI loopback)](https://github.com/tomaka/cpal/issues/251) —
  confirms cpal does not implement Windows loopback capture.
- [HEnquist/wasapi-rs](https://github.com/HEnquist/wasapi-rs) — Windows WASAPI crate
  including loopback capture support cpal lacks.
- [pipewire crate on crates.io](https://crates.io/crates/pipewire) — Rust bindings to
  libpipewire.
- [PipeWire docs: Streams](https://docs.pipewire.org/page_streams.html) and
  [pipewire-props](https://docs.pipewire.org/page_man_pipewire-props_7.html) — per-node
  metadata (`application.name`, `target.object`) underlying the per-app capture row in
  the matrix.
- [PipeWire docs: Loopback module](https://docs.pipewire.org/page_module_loopback.html).
- [cpal Cargo.toml](https://github.com/RustAudio/cpal/blob/master/Cargo.toml) — confirms
  Apple-only crates (`objc2`, `objc2-core-audio`, `coreaudio-rs`, etc.) and Linux/BSD-only
  crates (`alsa`, `pipewire`, `pulseaudio`) are both gated behind `cfg(target_os = ...)`
  target tables, not pulled unconditionally.
- [RustAudio/cpal#339](https://github.com/RustAudio/cpal/pull/339) and
  [RustAudio/cpal#476](https://github.com/RustAudio/cpal/issues/476) — WASAPI loopback
  merged, then reported broken; current status unverified.
- [HEnquist/wasapi-rs](https://github.com/HEnquist/wasapi-rs) /
  [wasapi crate on crates.io](https://crates.io/crates/wasapi) — dedicated Windows
  WASAPI crate with documented loopback support.
- [pipewire crate on crates.io](https://crates.io/crates/pipewire) (0.10.0, hosted at
  `gitlab.freedesktop.org/pipewire/pipewire-rs`).
- [cross-rs README](https://github.com/cross-rs/cross) — states Darwin/MSVC targets are
  not part of its supported pre-built image matrix.
- [cargo-zigbuild README](https://github.com/rust-cross/cargo-zigbuild) — documents the
  unresolved Zig/Darwin-framework linking issue and reliance on an externally-supplied
  macOS SDK.
- [osxcross](https://github.com/tpoechtrager/osxcross) — the underlying unofficial
  toolchain both of the above ultimately depend on; requires manually extracting an SDK
  from a licensed Xcode install.
- Prior-art survey (Handy, Vibe, screenpipe, Whisperi, libobs-rs) — repo pages linked
  inline in the prior-art table above; gathered via a live research pass on
  2026-08-03, some detail marked `[unverified]` where the repo tree could not be fully
  inspected.
- [cjpais/handy GitHub Actions release workflow](https://github.com/cjpais/handy) —
  fetched directly, confirms the 7-way macOS/Ubuntu/Windows CI matrix.
- [RustAudio/cpal#875 "Record system audio on multiple platforms"](https://github.com/RustAudio/cpal/issues/875)
  and [#876 "Support ScreenCaptureKit loopback"](https://github.com/RustAudio/cpal/issues/876),
  plus [PR #894](https://github.com/RustAudio/cpal/pull/894) — filed by Vibe's own
  author, confirming Vibe's system-audio feature is not yet shipped.
- [screenpipe/screenpipe](https://github.com/screenpipe/screenpipe) `LICENSE.md` and
  README — confirms source-available commercial license (not plain open source) and
  the macOS 14.4+ / "System Audio Recording Only" TCC detail.
- [fastrepl/hyprnote](https://github.com/fastrepl/hyprnote) and
  [OWhisper docs](https://docs.hyprnote.com/owhisper/what-is-this/) — confirms `owhisper`
  is a real project, correcting this doc's earlier "does not appear to exist" claim.
- [braden-w/whispering](https://github.com/braden-w/whispering) (moved to
  [EpicenterHQ/epicenter](https://github.com/EpicenterHQ/epicenter)) — confirms
  "Whispering" is a real, named project, correcting this doc's earlier "no canonical
  project... was found" claim; also establishes it's TypeScript/Svelte-primary, not Rust.
- [RustAudio/cpal#476](https://github.com/RustAudio/cpal/issues/476) — fetched directly
  (not just linked): closed 2020-10-28 by its own reporter with a fix patch; corrects
  this doc's earlier "current status still unreliable" framing.
- [wasapi crate docs.rs](https://docs.rs/wasapi/latest/wasapi/struct.AudioClient.html) —
  confirms `AudioClient::new_application_loopback_client(pid, include_tree)` exists,
  correcting the earlier `[unverified]` marker on Windows per-process loopback crate
  support.
- [Microsoft Learn: ActivateAudioInterfaceAsync / Application loopback audio capture sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/) —
  confirms `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`, Windows 10 2004+/build 20348+.
- [cpal `Cargo.toml`, re-fetched](https://github.com/RustAudio/cpal/blob/master/Cargo.toml) —
  corrects the Apple-only gate from `cfg(target_os = "macos")` to
  `cfg(target_vendor = "apple")`, and confirms `pipewire`/`pulseaudio`/`jack` are all
  `optional = true` on Linux/BSD (only `alsa`/`alsa-sys`/`libc` are unconditional).
- [cross-rs/cross README](https://github.com/cross-rs/cross), re-fetched — re-confirms
  no prebuilt Darwin/MSVC images; that support lives in the separate `cross-toolchains`
  repo.
- [rust-cross/cargo-zigbuild README](https://github.com/rust-cross/cargo-zigbuild),
  re-fetched — re-confirms the Zig/Darwin-CoreFoundation linking issue and the
  externally-supplied-SDK (`SDKROOT`) requirement.

## Fact-check log (2026-08-03)

Adversarial pass against live sources (crates.io API, GitHub API, GitHub source/READMEs,
docs.rs, Microsoft Learn). Default posture was skepticism, especially on the prior-art
table — README claims about cross-platform support are the highest-risk content in this
doc and got the most scrutiny.

**CONFIRMED (no change needed):**
- Handy: MIT license, mic-only (no loopback), `cpal`-based. CI matrix claim upgraded from
  `[unverified detail-level]` to fully confirmed (7-way macOS/Ubuntu/Windows matrix read
  directly from the release workflow).
- Whisperi: MIT, Windows-only in practice despite Tauri's cross-platform reach, cloud-only
  transcription. Verdict was already correct; added the OpenRouter provider detail.
- cross-rs: no prebuilt Darwin/MSVC images (lives in separate `cross-toolchains` repo).
- cargo-zigbuild: documented Zig/Darwin-CoreFoundation linking issue, requires an
  externally-supplied macOS SDK.
- osxcross being the underlying unofficial dependency of both, requiring manual Xcode
  SDK extraction.
- PipeWire per-app capability (node-per-stream, `application.name`/`application.process.id`
  metadata, no permission model) — matches `rust-audio-linux.md`'s independent findings.
- ScreenCaptureKit macOS 13.0 floor for system-audio-only capture.
- Transcription feature-flag story (`whisper-rs/metal`, `whisper-rs/cuda`) is consistent
  with `rust-audio-transcription.md`'s recommendation of whisper-rs as the primary engine.

**CORRECTED (said → true, with source):**
- Vibe license: "`[unverified]`" → **MIT**, confirmed via GitHub repo metadata.
- Vibe system audio: "`[unverified]` — no evidence found... looks file/import-oriented" →
  **confirmed not yet shipped**, and confirmed *why*: Vibe's own author filed
  [cpal#875](https://github.com/RustAudio/cpal/issues/875) and
  [#876](https://github.com/RustAudio/cpal/issues/876) asking cpal for exactly this
  capability, with an open PR (#894) still in progress. Stronger evidence than the
  original `[unverified]`, and it still supports the doc's overall pattern-level point.
- screenpipe license: "`[unverified]`" → **Screenpipe Commercial License** (source-available,
  personal/non-commercial use free, commercial use requires a paid license) — a real,
  material correction; this is not a plain open-source project.
- `owhisper`: "does not appear to exist as a named project" → **wrong, it's real**:
  [fastrepl/hyprnote](https://github.com/fastrepl/hyprnote)'s OWhisper, an Ollama-style
  on-device STT engine. Different scope than the recorder apps in the table (a serving
  engine, not a capture app), but it should never have been reported as nonexistent.
- `"Whispering"`: "no canonical project by this exact name was found" → **wrong, it's real**:
  [braden-w/whispering](https://github.com/braden-w/whispering), now folded into
  [EpicenterHQ/epicenter](https://github.com/EpicenterHQ/epicenter). Its application logic
  is TypeScript/Svelte, not Rust, which is why it doesn't change this doc's Rust-specific
  recommendations — but claiming it didn't exist was a plain research failure, not a
  defensible `[unverified]`.
- cpal Windows WASAPI loopback: "cpal itself does not implement this" → **false** — merged
  in [PR #339](https://github.com/RustAudio/cpal/pull/339) (2019). The real, narrower,
  confirmed reason to prefer the `wasapi` crate is that cpal has **no per-process loopback**
  at all, not that cpal lacks loopback entirely.
- cpal issue #476 status: "reported broken... whether cpal 0.18.x fixes this is
  `[unverified]`... evidence points to it still being unreliable" → **overstated**. The
  issue was closed 2020-10-28 by its own reporter, who supplied the fix. No open
  regression was found for current (2026) cpal releases in this pass.
- Windows per-app/per-process loopback crate support: "`[unverified]` whether any Rust
  crate wraps it today" → **confirmed it does**: the `wasapi` crate's
  `AudioClient::new_application_loopback_client(pid, include_tree)` wraps
  `ActivateAudioInterfaceAsync`/`VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` directly. This was
  the single most consequential correction in the capability matrix — the doc had rated
  a real, already-wrapped capability as unverified/theoretical.
- cpal's Apple-only Cargo.toml gate: described as `cfg(target_os = "macos")` → actually
  `cfg(target_vendor = "apple")` for the shared Apple dependency block (covering
  macOS/iOS/tvOS/visionOS), with a separate small `cfg(target_os = "macos")` block only
  for the optional `jack` feature. The underlying claim (a Linux build never touches
  objc2) still holds; the exact predicate was wrong.
- macOS process-tap minimum version: the capability matrix stated "14.2+" without
  qualification; `rust-audio-macos.md` (sibling doc, read as part of this pass) treats
  14.4 as the real recommended floor, since 14.2/14.3 have a documented rough edge.
  Reconciled in the matrix — see Cross-document contradictions below.

**STILL UNVERIFIED (honestly, not just carried forward):**
- screenpipe's actual Linux audio-capture source path — claimed in docs, not confirmed
  against source in this or the prior pass.
- Whether cpal's current (2026) Windows WASAPI loopback path has any live regressions —
  no open issue found in this pass, but that is absence of evidence, not a benchmark or a
  changelog confirmation of correctness.
- owhisper's and "Whispering"'s exact platform support matrices (which OSes actually build
  and run each) — this pass fixed the "doesn't exist" error but did not do a full
  platform-support audit to the same depth as Handy/Vibe/screenpipe/Whisperi.
- Whether `cidre`'s system-audio-capture changelog entry (mentioned in `rust-audio-macos.md`)
  is relevant to any of the prior-art projects — not investigated here, out of scope for
  this doc's survey.
- Exact whisper.cpp GGUF quantization WER numbers cited elsewhere in the sibling docs —
  out of scope for this pass, already flagged `[unverified]` in `rust-audio-transcription.md`.

### Cross-document contradictions

- **macOS process-tap minimum version.** This doc's capability matrix said "macOS 14.2+"
  for `AudioHardwareCreateProcessTap` without further qualification. `rust-audio-macos.md`
  states 14.2 introduced the API but recommends **14.4** as the practical floor, citing a
  rough edge in 14.2/14.3 per multiple sample projects. **Fixed in this doc** — the matrix
  cell now says 14.2+ introduces the API, 14.4 is the recommended real target, and links to
  the sibling doc's fuller reasoning. Not fixed in `rust-cross-platform-strategy.md`'s
  peers (out of scope per this task's instructions to edit only this file), but flagged here
  so a reader of `rust-audio-macos.md` alone and a reader of this doc alone won't disagree
  about the target version.
- **cpal's native macOS loopback (0.17+) is omitted, not contradicted, here.**
  `rust-audio-processing.md` reports cpal 0.17.0 (2025-12-20) added native macOS
  system-audio loopback support, but recommends holding off given how new and bug-ridden
  the 0.17→0.18 changelog shows it to be (UID collisions, silent-tap failures — the same
  "zero samples" failure class `rust-audio-macos.md` documents for the hand-rolled tap
  path). This doc's own recommendation (hand-rolled `objc2`-based tap in `macos/mod.rs`,
  per `rust-audio-macos.md`) is **consistent** with that caution — both land on "don't use
  cpal's own loopback yet" — but this doc never mentions that cpal *has* a native loopback
  option at all, which reads as an omission next to the more complete picture in
  `rust-audio-processing.md`. Not a factual contradiction, but worth a reader knowing: if
  cpal's macOS loopback matures, this doc's Cargo-mechanics section and module layout would
  need revisiting, and that dependency isn't currently called out here.
- **No contradiction found** between this doc's transcription-boundary Cargo feature-flag
  sketch (`whisper-rs/metal`, `whisper-rs/cuda`) and `rust-audio-transcription.md`'s
  recommendation of whisper-rs as the primary engine — they agree.
- **No contradiction found** between this doc's Linux permissions claim ("no OS-level audio
  permission model on Linux at all") and `rust-audio-linux.md`'s dedicated Permissions
  section, which says the same thing in more detail (Flatpak/Snap sandboxing and SELinux
  are the only adjacent, non-runtime-consent gates).
