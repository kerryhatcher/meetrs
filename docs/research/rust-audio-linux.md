# Linux audio capture from Rust (2026) — mic + system audio, non-exclusive

Counterpart to `audio-recording-and-processing.md` (macOS). Question: capture microphone
AND system/application audio simultaneously, without exclusive device ownership, from Rust,
on Linux.

## Recommendation

Target **PipeWire directly**, via the official `pipewire` crate (v0.10.0, MIT). Open two
independent `pw::stream::Stream` capture streams on one `pw::main_loop::MainLoop`/`Core`:
one plain audio-capture stream for the mic node, one with `stream.capture.sink = true`
(or `target.object` pointing at a sink) for system audio via that sink's **monitor**
ports. This covers current Fedora/Ubuntu/Debian/Arch/openSUSE out of the box — no
PulseAudio dependency needed, and `pipewire-pulse` means a PulseAudio-API build works too
via the compat socket if you'd rather ship `libpulse-binding` instead.

Fallback / MVP path: subprocess `pw-record --target <node-id>` (or `parec -d
<sink>.monitor`) for system audio and a normal `cpal` ALSA/PipeWire-feature input stream
for the mic, muxing PCM on stdout into your pipeline. This gets you shipping before you've
hand-rolled the native stream API, at the cost of a spawned-process dependency and losing
`pw_time`-level clock alignment.

Do not target raw ALSA as the primary layer — it has no concept of a "monitor" and fights
the sound server for the device. Do not build a PipeWire portal (`org.freedesktop.portal.ScreenCast`)
path for the non-sandboxed build — as of writing there is **no audio parameter** on that
portal (confirmed against the current `ScreenCast.xml` spec, fact-checked below); only wire
it if meetrs ships as a Flatpak.

**Fact-check update, unchanged recommendation but one caveat added**: it turns out `cpal`
0.18.1, built with `--features pipewire`, *can* also cover the system-audio/monitor leg —
its device model exposes sink nodes as capturable and sets `STREAM_CAPTURE_SINK`
automatically when you open one for input (verified in cpal's own source, see the `cpal` on
Linux section). That's a legitimate simpler alternative to hand-rolling the native `pipewire`
stream API for both legs. The recommendation above still stands as primary because the raw
`pipewire` crate gives finer control (naming an exact sink via `target.object`, or capturing a
single app's pre-mix stream — neither of which fits cpal's plain device model), but treat
"cpal everywhere, one dependency, `--features pipewire`" as a valid lower-effort MVP variant
if the native-crate route proves too slow to ship.

## API layers and binding crates

| Layer | Purpose | Rust crate | Version (crates.io, checked 2026-08) | License | Downloads (all-time / recent) | Verdict |
|---|---|---|---|---|---|---|
| PipeWire (native) | Default sound server on current distros; per-app routing, monitor ports, single graph | `pipewire` (+ `pipewire-sys`) | 0.10.0 | MIT | 1.27M / 651K recent | **Primary** |
| PipeWire (subprocess) | No FFI, no build-time libpipewire dep | `pw-record`/`pw-cat` (system binary, not a crate) | ships with `pipewire-bin`/`pipewire-utils` | — | — | Fallback |
| PulseAudio (native) | Legacy API; still the API surface `pipewire-pulse` emulates | `libpulse-binding` + `libpulse-simple-binding` | 2.30.1 / 2.29.0 | MIT OR Apache-2.0 | 5.6M / 4.8M | Viable, not primary |
| PulseAudio (subprocess) | Simplicity | `parec`/`pactl` (system binary) | — | — | — | Fallback |
| JACK | Pro-audio graph routing | `jack` | 0.13.5 | MIT | 1.08M | Not relevant to meetrs |
| ALSA (raw) | Lowest layer; hardware/dmix only | `alsa` (+ `alsa-sys`) | 0.12.1 / 0.6.1 | Apache-2.0/MIT | 18.3M / 16.6M | Avoid as primary |
| Cross-platform | Portable input/output streams | `cpal` | 0.18.1 | Apache-2.0 | 17.1M | Mic only, see below |
| XDG portal | Sandboxed screencast (video); no confirmed audio param | `ashpd` | 0.13.13 | MIT | 11.9M | Only if Flatpak |

`alsa`/`cpal` download counts are inflated by being pulled in as transitive deps of the whole
Rust-audio ecosystem (e.g., `cpal` itself depends on `alsa-sys`/`libc` on Linux even when
you don't touch ALSA yourself) — treat them as ecosystem-ubiquity signals, not "everyone
calls monitor sources with this" signals.

## The 2026 landscape

- **PipeWire is the default** on Fedora 34+ (2021, confirmed — Fedora Magazine: PipeWire "has
  come to full fruition in Fedora Workstation 34, where it handles both audio and video"; the
  Fedora Change wiki page targeted this for Fedora 34 and it shipped on schedule, despite
  contemporaneous Phoronix coverage floating a possible slip to 35), Ubuntu 22.10+ (confirmed —
  Ubuntu 22.04 LTS shipped PipeWire only for video/Wayland compat with PulseAudio still owning
  audio; 22.10 "Kinetic Kudu" removed PulseAudio from the desktop image and made PipeWire+
  pipewire-pulse the audio default), Arch, openSUSE, Manjaro, Pop!_OS. **Debian is more mixed
  than "12+" implies**: per the Debian Wiki, PipeWire is the default sound server in Bookworm
  (12) *only for the GNOME desktop* — other Debian 12 desktop flavors may still default to
  PulseAudio. Debian 13 "Trixie" ships PipeWire 1.4.2 + WirePlumber 0.5.8 and is documented as
  the default across "most" desktop environments, not universally either. PipeWire replaced
  both PulseAudio (desktop audio) and JACK (pro-audio) as the single graph where it is the
  default.
- **PulseAudio install base**: still present on older LTS installs (Ubuntu 20.04-class),
  minimal/server distros that never adopted a desktop stack, and some embedded/appliance
  Linux images. A new desktop-facing app in 2026 should not assume it, but a server/headless
  meeting-recorder image might still meet it.
- **JACK**: pro-audio-only relevance (DAWs, low-latency routing). Not a target for meetrs.
- **Bare ALSA**: still the kernel-facing layer everything else sits on; a userspace app
  talking ALSA directly gets exclusive-ish access semantics (or has to fight `dmix`), no
  per-application routing, and no monitor concept. Every layer above pipewire/pulse still
  needs ALSA dev headers at build time (`libasound2-dev`/`alsa-lib-devel`) because they link
  against it transitively.
- **`pipewire-pulse` compat layer**: PipeWire ships a socket-compatible PulseAudio server
  reimplementation. `pactl`, `pavucontrol`, `paplay`, and any app linked against
  `libpulse`/`libpulse-simple` — including a Rust app using `libpulse-binding` — talk to it
  unmodified; it doesn't know or care it's PipeWire underneath. This means **shipping a
  `libpulse-binding`-based client is a legitimate, still-current strategy** on a PipeWire
  system: you get PipeWire's routing/mixing for free through the compat layer, at the cost
  of the older, more limited PulseAudio API (no direct access to PipeWire-only features like
  arbitrary node linking).

## PipeWire from Rust

`pipewire` (crates.io, MIT, v0.10.0 as of 2026-05-17) is the official Rust binding,
generated/maintained under the freedesktop.org PipeWire project (`pipewire-rs` repo), with
`pipewire-sys` as the raw FFI layer underneath. Docs: https://pipewire.pages.freedesktop.org/pipewire-rs/pipewire/

API shape, confirmed from the rustdoc:
- `main_loop` / `loop_`: event loop, timers, I/O, signals.
- `registry`: enumerate global objects (nodes, ports, devices, clients) via callbacks.
- `node`, `port`, `device`, `client`: introspection/proxy objects for the graph.
- `stream::Stream`: the high-level capture/playback API — this is what you use to get PCM
  bytes in a `process` callback.

Enumerating nodes (sketch, based on the registry pattern documented in pipewire-rs):

```rust
use pipewire as pw;

pw::init();
let mainloop = pw::main_loop::MainLoop::new(None)?;
let context = pw::context::Context::new(&mainloop)?;
let core = context.connect(None)?;
let registry = core.get_registry()?;

let _listener = registry
    .add_listener_local()
    .global(|obj| {
        if let Some(props) = &obj.props {
            println!("{:?} {:?}", obj.type_, props);
        }
    })
    .register();

mainloop.run();
```

Capturing a **monitor** stream (system audio) — the property that matters is
`stream.capture.sink` (a.k.a. `PW_KEY_STREAM_CAPTURE_SINK`): setting it true tells PipeWire
to route the capture stream to the target sink's **monitor ports** instead of treating the
target as a source. This is documented in the `pipewire-pulse`/protocol-simple module docs
(`docs.pipewire.org/page_module_protocol_simple.html`), which state plainly: setting
`stream.capture.sink = false` (its inverse) "make[s] the capture stream capture the monitor
ports" — i.e. capturing a sink's monitor is the standard, first-class way to get system
audio out of the graph, mirroring PulseAudio's own `<sink>.monitor` concept one layer down.
`target.object` (node name or id) selects *which* sink or app stream to attach to.

```rust
use pipewire::{properties::properties, stream::StreamFlags};

let props = properties! {
    *pw::keys::MEDIA_TYPE => "Audio",
    *pw::keys::MEDIA_CATEGORY => "Capture",
    *pw::keys::MEDIA_ROLE => "Music",
    // Attach to a specific sink's monitor rather than an arbitrary source:
    *pw::keys::TARGET_OBJECT => "alsa_output.pci-0000_00_1f.3.analog-stereo",
    *pw::keys::STREAM_CAPTURE_SINK => "true",
};

let stream = pw::stream::Stream::new(&core, "meetrs-system-audio", props)?;

let _listener = stream
    .add_local_listener_with_user_data(())
    .process(|stream, _| {
        // pull buffer, read PCM, note pw_time for alignment (see below)
    })
    .register()?;

stream.connect(
    pipewire::spa::utils::Direction::Input,
    None,
    StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
    &mut [],
)?;
mainloop.run();
```

**Resolved**: the constant names `pw::keys::STREAM_CAPTURE_SINK` and `pw::keys::TARGET_OBJECT`
are correct for `pipewire` 0.10.0. Confirmed indirectly but concretely: `cpal` v0.18.1's own
`Cargo.toml` pins `pipewire = { version = "0.10", features = ["v0_3_53"] }` and its
`src/host/pipewire/device.rs` uses exactly `*pw::keys::STREAM_CAPTURE_SINK` and
`*pw::keys::TARGET_OBJECT` against that dependency — a real, shipped, compiling consumer of
the 0.10.0 API, which is stronger evidence than a docs.rs page render. The property semantics
(`stream.capture.sink=true` routes to monitor ports, `target.object` selects the target node)
remain confirmed against PipeWire's C-level module docs; `node.target` → `target.object` was
indeed a historical rename, but the current crate uses the current name.

**Capturing a specific application's playback** vs the whole sink: same mechanism, just
target the application's own output *stream* node (by its `node.id`, discovered via the
registry) instead of the sink, and omit `stream.capture.sink` — you get exactly that app's
audio pre-mix, which PulseAudio's sink-monitor model cannot do (it only gives you post-mix
sink output). This is a real capability gap in meetrs' favor if per-app isolation is ever
wanted (e.g. capture only the video-call app, not system notification dings).

**Subprocess fallback**: `pw-record --target <node-id-or-name> out.wav` (alias for
`pw-cat --record`) and `pw-cat -p` for playback. Ships in `pipewire-bin`/`pipewire-utils` on
every distro that has PipeWire at all. Fine for a v1 or for the system-audio leg while you
build out the native mic path with `cpal`.

## PulseAudio from Rust

`libpulse-binding` (2.30.1, MIT OR Apache-2.0, 5.6M downloads) is the full binding;
`libpulse-simple-binding` (2.29.0, same license, 4.8M downloads) wraps `libpulse-simple` for
single-stream-per-connection use (no channel maps, no volume control, no multiple streams —
fine for "record system audio to one buffer," not fine if you need the mic and system
streams to share fine-grained control).

The monitor-source concept: every PulseAudio (and pipewire-pulse-emulated) sink
automatically exposes a matching source named `<sink-name>.monitor` that carries whatever
is being played to that sink. Recording from it is the textbook way to capture system
audio on the Pulse API, same idea as PipeWire's `stream.capture.sink`, one layer up:

```rust
use libpulse_simple_binding::Simple;
use libpulse_binding::stream::Direction;
use libpulse_binding::sample::{Spec, Format};

let spec = Spec { format: Format::S16le, channels: 2, rate: 48_000 };
let simple = Simple::new(
    None,                                   // default server
    "meetrs",
    Direction::Record,
    Some("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor"),
    "system-audio",
    &spec,
    None,
    None,
)?;
let mut buf = [0u8; 4096];
simple.read(&mut buf)?; // blocking read of PCM frames
```

Discover the monitor name at runtime with `pactl list sources short` — filter for names
ending `.monitor`, or query `libpulse-binding`'s introspection API
(`Context::introspect().get_source_info_list`) rather than hardcoding.

**Subprocess fallback**: `parec -d <sink>.monitor` piped into your process, exactly the
PulseAudio-era pattern predating PipeWire; still works unmodified under `pipewire-pulse`.

## ALSA from Rust — and why it's usually the wrong layer

`alsa` (0.12.1, Apache-2.0/MIT) + `alsa-sys` (0.6.1, MIT) give thin, safe wrappers over
`libasound`. Problems for meetrs specifically:

- **Exclusive-ish access**: ALSA's hardware devices (`hw:0`) don't multiplex; a second
  opener gets `EBUSY` unless routed through `dmix`/`dsnoop`, which you'd have to configure
  yourself (`~/.asoundrc` or `/etc/asound.conf`), and that's exactly the "OBS problem" from
  the macOS doc, transplanted.
- **No per-app routing, no monitor concept**: ALSA has no idea what "system audio" or "this
  application's output" means — those are sound-server-level abstractions PipeWire/Pulse
  build on top. Recording at the ALSA layer only sees whatever the sound server chooses to
  mix down to the hardware device, at best.
- **cpal and everything else still links it**: PipeWire, PulseAudio, and JACK's own ALSA
  plugin all sit on top of `libasound`, so `libasound2-dev`/`alsa-lib-devel` is a build-time
  dependency for meetrs regardless of which layer you target directly.

## `cpal` on Linux

`cpal` 0.18.1 (Apache-2.0, RustAudio) uses **ALSA as its default/always-on Linux backend**;
JACK, PipeWire, and PulseAudio are optional Cargo features (`--features jack`, `--features
pipewire`, `--features pulseaudio`) that add native integration, gated behind their own
`-dev`/`-devel` system packages. ALSA dev headers are required even when you enable one of
the others, per cpal's own README.

Critical question: **can `cpal` see PulseAudio/PipeWire monitor sources as input devices?**
**Resolved by reading the actual backend source (`RustAudio/cpal` tag `v0.18.1`, the version
this doc recommends — not just master) — the answer is yes, for both non-default backends:**

- Via cpal's plain ALSA host (the default, no feature flags): still no — a monitor source is a
  sound-server abstraction that doesn't exist at the ALSA device-enumeration level; ALSA only
  sees the sound server's ALSA bridge devices (`pipewire`/`pulse` as pcm names), not individual
  monitor sources. This part of the original finding holds.
- With `--features pulseaudio`: `PulseAudioHost::devices()`
  (`src/host/pulseaudio/mod.rs`) calls `self.client.list_sources()` and wraps **every** result
  as `Device::Source` with **no filtering** for monitor sources. PulseAudio's own
  `list_sources` protocol call returns `<sink>.monitor` sources mixed in with physical mic
  sources — cpal does nothing to exclude them, so they surface as ordinary `cpal::Device`s via
  `devices()`, indistinguishable from a real microphone except by name/description.
- With `--features pipewire`: `src/host/pipewire/device.rs` explicitly classifies discovered
  nodes by `MEDIA_CLASS` and treats `Audio/Sink` nodes as `DeviceDirection::Duplex` (not
  output-only) — the code comment states this directly: *"Discovered `Audio/Sink` nodes are
  exposed as `Duplex`, so they are treated as input-capable. When cpal later opens an input
  stream on such a device, it sets `STREAM_CAPTURE_SINK`, which makes that stream capture audio
  playing to the sink."* This is confirmed in `pw_properties()`: `if matches!(self.role,
  Role::Sink) && matches!(direction, DeviceDirection::Input) { properties.insert(
  *pw::keys::STREAM_CAPTURE_SINK, "true"); }`. So opening a sink-classed `cpal::Device` for
  input is exactly the sink-monitor-capture mechanism, done for you.
- Practical implication, revised: **cpal *can* cover the system-audio leg**, if built with
  `--features pipewire` (preferred, matches the native-PipeWire recommendation above) or
  `--features pulseaudio`, by enumerating `devices()`/iterating sinks and opening one for
  input. This weakens (but doesn't eliminate) the case for hand-rolling the native `pipewire`
  stream API — cpal-with-pipewire-feature is now a legitimate alternate path for both legs of
  meetrs' capture, at the cost of losing fine control over `target.object`/per-app-stream
  targeting that the raw `pipewire` crate gives you (cpal's device model has no notion of
  "capture this one app's pre-mix stream," only sinks/sources).

Build-time note: cpal's PipeWire feature needs `libpipewire-0.3-dev`
(Debian/Ubuntu)/`pipewire-devel` (Fedora); its plain build needs `libasound2-dev`/`alsa-lib-devel`
regardless.

## Simultaneous mic + system capture: sync and drift

Two independent streams (mic via cpal/ALSA, system via a PipeWire/Pulse monitor capture)
means two independent clocks and buffering paths — the same "you own the drift" problem the
macOS doc flags for the BlackHole+aggregate-device fallback. Two ways to avoid hand-rolled
resampling:

1. **One PipeWire graph, two streams sharing the graph clock.** PipeWire's driver runs the
   whole graph — every node (including your two capture streams) is clocked against the same
   quantum/rate by the graph driver, so as long as both streams are pulled from the *same*
   PipeWire instance (not one via PipeWire and one via a separate PulseAudio daemon), their
   buffers correspond to the same wall-clock ticks by construction; you're aligning by
   `pw_time`, not resampling by ear.
2. **`libpipewire-module-combine-stream`**: a PipeWire module (see
   `docs.pipewire.org/page_module_combine_stream.html`) that merges multiple existing streams
   into one virtual node with `combine.audio.position` mapping input channels to output
   channels. This is a server-side graph module (loaded via `pipewire.conf`/session-manager
   config), not a client-side Rust API — it would let you present "mic + system" as a single
   virtual sink/source with one clock, but wiring it means shipping/loading a PipeWire module
   config rather than calling a crate function. `[unverified]` — I did not find confirmation
   this module is meant for (or commonly used for) mic+monitor combination specifically versus
   its documented multi-device use case; verify before building a deployment story around it.
3. **Timestamps**: `pw_time`/`pw_stream_get_nsec()` give you, per `process` callback, the
   graph tick count and a delay figure — "time it will take for the next output sample to be
   presented" for playback, "time a sample traveled from the capture device" for capture.
   Practical alignment approach for meetrs: read `pw_time` on both streams each callback and
   align buffers by tick count rather than wall-clock `SystemTime`, same principle as the
   macOS doc's aggregate-device drift compensation, just done in your own mixer code instead
   of Core Audio's.

If you take the subprocess fallback (`pw-record`/`parec` for system + `cpal` for mic), you
lose both of the above and are back to timestamp-based post-hoc alignment — acceptable for
an MVP, same caveat as the macOS BlackHole path.

## Sandboxing / portals

- **`org.freedesktop.portal.ScreenCast`**: covers video screen/window capture with a
  fine-grained per-session dialog. As of the current spec
  (`xdg-desktop-portal/data/org.freedesktop.portal.ScreenCast.xml`) it has **no audio
  parameter** — screen-cast audio (e.g. "share this window's audio") is explicitly an open
  request, not a shipped feature: see `flatpak/xdg-desktop-portal` discussion #1142 ("Audio
  portal"), opened October 2023 and still unresolved as of the last available comments. The
  workaround sandboxed apps use today is a static permission grant, not a per-use portal
  prompt: Flatpak manifests add `--socket=pulseaudio` (talks to the pipewire-pulse compat
  socket) or `--filesystem=xdg-run/pipewire-0` (raw PipeWire socket access) — both are
  coarse-grained, no-user-prompt escapes rather than the narrow, revocable grant a real Audio
  portal would give.
- **`ashpd`** (0.13.13, MIT, 11.9M downloads) is the Rust wrapper for these DBus portal
  interfaces (`zbus`-based). Relevant to meetrs only if/when it ships as a Flatpak — it would
  get you the ScreenCast **video** flow (and any future Audio portal, once one ships) with
  proper prompts; for audio today under Flatpak you'd still fall back to the
  `--socket=pulseaudio`/`--filesystem=xdg-run/pipewire-0` permission plus a normal
  `pipewire`/`libpulse-binding` client running inside the sandbox.
- **Snap**: analogous story — `audio-record`/`audio-playback` interface connections plug the
  snap into the host's PulseAudio/PipeWire socket; no finer-grained portal-style audio
  control either.
- **Practical guidance for meetrs**: ship as a native package (deb/rpm/AppImage) first, where
  none of this matters — a normal desktop user session already has full PipeWire/PulseAudio
  socket access. Revisit the portal story only if/when a Flatpak build is requested.

## Wayland vs X11

Audio capture itself is display-server-agnostic — PipeWire/PulseAudio sit below the display
server entirely, so mic and monitor-source capture behave identically on X11 and Wayland.
The place the display server matters is **screen capture** (video), where X11 apps can grab
frames directly (X11 has no portal requirement) while Wayland compositors route screen
capture through `org.freedesktop.portal.ScreenCast` + PipeWire for security reasons.
Confirmed via the same portal docs above: Wayland's use of PipeWire for screen sharing is
about the *video* path, not audio, and doesn't change any of the audio guidance in this
document. If meetrs ever adds screen recording (not just audio) alongside the meeting
recording, that's the point where Wayland forces the portal path and X11 doesn't — audio
capture is unaffected either way.

## Distribution

- **Dynamic linking is the norm and the sane default.** `libpipewire-0.3.so`/`libpulse.so`
  ship as system libraries on any desktop Linux with PipeWire/PulseAudio installed at all —
  which is to say, virtually every desktop install. Link dynamically (the default for the
  `pipewire`/`libpulse-binding` crates via their `-sys` crates' `pkg-config`-driven build
  scripts) and let the system library satisfy it.
- **Build-time deps**: `pkg-config` plus the relevant `-dev`/`-devel` package —
  `libpipewire-0.3-dev` (Debian/Ubuntu) or `pipewire-devel` (Fedora) for the `pipewire` crate;
  `libpulse-dev`/`pulseaudio-libs-devel` for `libpulse-binding`; `libasound2-dev`/
  `alsa-lib-devel` unconditionally (see ALSA section). These are compile-time only — end
  users never need them, only your CI/build image does.
  `[unverified: exact devel package names per current Fedora/Debian release — confirm against
  each distro's package search before writing install docs]`.
  Runtime needs the plain (non `-dev`) library packages, which are already present on any
  system that has PipeWire/PulseAudio running — i.e. every supported desktop.
- **Static linking**: possible in principle (`pipewire`/`pulse` are open-source C libraries)
  but not how either crate's `-sys` build script works out of the box, and not how any
  distro ships them; pursuing it buys nothing since the dynamic library is guaranteed present
  on any machine actually running PipeWire/PulseAudio in the first place.
- **AppImage**: bundles your Rust binary + non-system deps, but PipeWire/PulseAudio
  deliberately should NOT be bundled — you need to talk to the *running* system instance
  (it holds the actual audio devices), not a private copy. AppImage is otherwise a clean fit:
  no portal/socket restrictions beyond what a normal binary gets.
  `[unverified]`
- **Flatpak**: adds the sandboxing constraints in the Sandboxing/Portals section above —
  audio needs an explicit `--socket=pulseaudio` or `--filesystem=xdg-run/pipewire-0`
  permission declared in the manifest; there's no portal-mediated audio grant to use instead
  as of this writing.
  **Security note (current as of this doc's write date, Aug 2026):** `--socket=pulseaudio`
  was the vector for CVE-2026-5674 (CVSS 8.8), a real sandbox-escape chain in the
  `pipewire-pulse` compat layer — a Flatpak app with only `--socket=pulseaudio` plus *any*
  host-writable path (even `--filesystem=/tmp`) could get code execution outside the sandbox
  by abusing an unvalidated auth cookie and unrestricted LADSPA module loading. It's fixed
  upstream (patch "Prevent dlopen of absolute paths"; distro updates landed by late July
  2026), so a current PipeWire build is safe, but it's a concrete illustration of exactly the
  "coarse-grained, no-user-prompt escape" risk this doc already flags in the Audio-portal
  discussion below — grant `--socket=pulseaudio` only when actually needed, and keep the host
  PipeWire package patched.

## Permissions

Linux has **no OS-level permission gate for audio capture** comparable to macOS TCC — this
is the single biggest structural difference from the macOS doc. Any process running as the
logged-in user can open any PipeWire/PulseAudio/ALSA input it can see, silently, with no
prompt, no entitlement, no Info.plist string. Confirmed by the entire shape of the tooling
above: `pw-record`/`parec`/`arecord` all work with zero consent UI the moment you run them.
Also confirmed directly: GNOME's own Settings → Privacy → Microphones panel carries an
explicit disclaimer that it can only control apps that go through the (nonexistent) audio
portal — system apps and anything with plain PipeWire/PulseAudio socket access are always
able to record, panel or no panel. Same story on KDE (tracked as a known gap on KDE's own
Discuss forum). Neither desktop has shipped a functional per-app mic consent prompt as of
this doc's write date (Aug 2026); this is exactly the gap the still-open Audio-portal
discussion (#1142, below) exists to close.

Exceptions/adjacent gates worth knowing:
- **Flatpak/Snap sandboxing** (see above) is the one place a permission-like gate exists —
  but it's a manifest-declared static grant checked at install/build time, not a runtime
  user-facing consent prompt the way TCC or the ScreenCast portal are.
- **ALSA device file permissions**: on systems using bare ALSA without a sound server (rare
  on a 2026 desktop, more plausible on a minimal/embedded/server image), access to
  `/dev/snd/*` is gated by Unix group membership — historically the `audio` group. A user not
  in that group gets `EACCES` opening the raw ALSA device. Irrelevant once PipeWire/PulseAudio
  own the device, since the sound server itself runs with the necessary access and mediates
  everyone else's requests via its own socket — no group membership needed by *your*
  meetrs process. `[unverified: exact group name and udev rule details vary by distro —
  confirm against the target distro's `/lib/udev/rules.d/` before assuming `audio` group
  gating applies]`.
- **SELinux**: Fedora/RHEL-family systems run SELinux, and a *confined* application (one
  running under a restrictive SELinux domain, not the ordinary unconfined desktop-user
  session) could in principle be denied access to the PipeWire/PulseAudio socket by policy.
  For an ordinarily-installed desktop app run by an interactive user this does not apply in
  practice — flagging it as the one theoretical MAC-layer exception, `[unverified]` beyond
  that.
- **No screen-recording-style "first launch after grant, must relaunch" dance**: unlike
  macOS ScreenCaptureKit, there is no first-run consent flow to design UX around for the
  non-sandboxed build.

## Open questions

- ~~Does `cpal`'s `pulseaudio`/`pipewire` feature actually enumerate `.monitor` sources as
  `cpal::Device`s?~~ **Resolved** — yes for both features, confirmed by reading
  `src/host/pulseaudio/mod.rs` and `src/host/pipewire/device.rs` at the `v0.18.1` tag; see the
  `cpal` on Linux section above.
- ~~What is the correct `pipewire` 0.10.0 `keys` constant for `stream.capture.sink` and
  `target.object`?~~ **Resolved** — `pw::keys::STREAM_CAPTURE_SINK` and
  `pw::keys::TARGET_OBJECT`, confirmed via cpal v0.18.1's own use of `pipewire = "0.10"`.
- Is `libpipewire-module-combine-stream` actually usable/commonly used to merge a mic node
  and a sink-monitor into one synchronized virtual device, or is that outside its intended
  use (multi-device combining, e.g. multiple mics)? If usable, it would simplify the
  drift-alignment story considerably; if not, stick to per-stream `pw_time` alignment.
  Currently `[unverified]`.
- Current status of the XDG "Audio portal" proposal (discussion #1142) — re-checked for this
  fact-check pass (Aug 2026): still an open GitHub Discussion, opened Oct 2023, most visible
  substantial activity in 2024, no confirmed 2025/2026 movement found. Still genuinely
  unresolved, but "no visible recent activity" is a weaker claim than "confirmed dead" — worth
  a fresh look at actual implementation time rather than trusting this doc's snapshot.
- Exact `-dev`/`-devel` package names and minimum versions for `libpipewire-0.3-dev` /
  `pipewire-devel` on the specific distros meetrs' CI/build images target.
- Whether ALSA `audio` group gating is even reachable on meetrs' actual target distros (likely
  moot if PipeWire/PulseAudio is guaranteed present) — confirm before writing install docs
  that mention group membership.

## Sources

- [pipewire — crates.io](https://crates.io/crates/pipewire) — v0.10.0, MIT, verified via crates.io API (`updated_at` 2026-05-17, downloads 1,274,913 / 651,448 recent)
- [pipewire-rs API docs](https://pipewire.pages.freedesktop.org/pipewire-rs/pipewire/) — module structure (stream, registry, node/port/device/client, main_loop)
- [pipewire_sys::pw_time](https://pipewire.pages.freedesktop.org/pipewire-rs/pipewire_sys/struct.pw_time.html) and [PipeWire pw_time struct reference](https://docs.pipewire.org/structpw__time.html) — timestamp/tick/delay semantics
- [PipeWire Protocol Simple module docs](https://docs.pipewire.org/page_module_protocol_simple.html) — `stream.capture.sink` / monitor-port capture semantics, `target.object`
- [PipeWire Combine Stream module docs](https://docs.pipewire.org/page_module_combine_stream.html) and [Arch manual page](https://man.archlinux.org/man/libpipewire-module-combine-stream.7.en) — multi-stream merge into one virtual node
- [How to Capture Audio Using Pipewire and Rust](https://acalustra.com/playing-with-pipewire-audio-streams-and-rust.html) — Rust stream setup code pattern
- [cpal — crates.io](https://crates.io/crates/cpal) — v0.18.1, Apache-2.0, 17,123,752 downloads (crates.io API)
- [RustAudio/cpal README](https://github.com/RustAudio/cpal) — Linux ALSA default backend, optional `jack`/`pipewire`/`pulseaudio` features, ALSA dev headers always required, DeviceBusy/bridge-device caveat
- [cpal PR #938 — PipeWire implementation](https://github.com/RustAudio/cpal/pull/938)
- [libpulse-binding — crates.io](https://crates.io/crates/libpulse-binding) — v2.30.1, MIT OR Apache-2.0, 5,591,839 downloads
- [libpulse-simple-binding — crates.io](https://crates.io/crates/libpulse-simple-binding) — v2.29.0, same license, 4,765,686 downloads
- [alsa — crates.io](https://crates.io/crates/alsa) — v0.12.1, Apache-2.0/MIT, 18,270,643 downloads
- [alsa-sys — crates.io](https://crates.io/crates/alsa-sys) — v0.6.1, MIT, 16,607,874 downloads
- [jack — crates.io](https://crates.io/crates/jack) — v0.13.5, MIT, 1,075,263 downloads
- [ashpd — crates.io](https://crates.io/crates/ashpd) — v0.13.13, MIT, 11,936,582 downloads; [ashpd GitHub](https://github.com/bilelmoussaoui/ashpd) — XDG portals wrapper via zbus
- [xdg-desktop-portal ScreenCast.xml spec](https://github.com/flatpak/xdg-desktop-portal/blob/main/data/org.freedesktop.portal.ScreenCast.xml) and [ScreenCast portal docs](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.impl.portal.ScreenCast.html) — no audio parameter present
- [flatpak/xdg-desktop-portal discussion #1142 "Audio portal"](https://github.com/flatpak/xdg-desktop-portal/discussions/1142) — status of proposed audio portal, current Flatpak workarounds (`--socket=pulseaudio`, `--filesystem=xdg-run/pipewire-0`)
- [pw-cat / pw-record man pages](https://docs.pipewire.org/page_man_pw-cat_1.html), [Debian manpage](https://manpages.debian.org/testing/pipewire-bin/pw-record.1.en.html), [Arch manpage](https://man.archlinux.org/man/pw-cat.1.en)
- [PipeWire pipewire-pulse man page](https://docs.pipewire.org/page_man_pipewire-pulse_1.html) and [ArchWiki PipeWire](https://wiki.archlinux.org/title/PipeWire) — PulseAudio-compatible daemon, `pactl`/`pavucontrol`/`paplay` work unmodified
- [Debian Wiki PipeWire](https://wiki.debian.org/PipeWire) — Debian 13 Trixie defaults (PipeWire 1.4.2, WirePlumber 0.5.8)
- Distro-default timeline (Fedora 34+, Ubuntu 22.10+, Debian 12+) cross-checked across [oneuptime.com Ubuntu PipeWire post](https://oneuptime.com/blog/post/2026-03-02-how-to-set-up-pipewire-as-audio-server-on-ubuntu/view), [Baeldung PulseAudio→PipeWire](https://www.baeldung.com/linux/pulseaudio-pipewire-replace), [sumguy.com "Linux Audio in 2026"](https://sumguy.com/linux-audio-pipewire-2026/) — general-web sources, lower confidence than official docs/wikis, cross-referenced against Debian/Arch wikis above
- crates.io API queried directly (`https://crates.io/api/v1/crates/<name>`) for authoritative version/license/download figures, 2026-08-03
- [Fedora Change wiki — DefaultPipeWire](https://fedoraproject.org/wiki/Changes/DefaultPipeWire) and [Fedora Magazine PipeWire interview](https://fedoramagazine.org/pipewire-the-new-audio-and-video-daemon-in-fedora-linux-34/) — Fedora 34 default confirmation
- [OMG! Ubuntu — Ubuntu 22.10 Makes PipeWire Default for Audio](https://www.omgubuntu.co.uk/2022/05/ubuntu-22-10-makes-pipewire-default) and [Phoronix — Ubuntu 22.10 Switching To PipeWire](https://www.phoronix.com/news/Ubuntu-22.10-PipeWire) — Ubuntu 22.04 (video-only PipeWire) vs 22.10 (audio default) distinction
- [RustAudio/cpal source, tag v0.18.1](https://github.com/RustAudio/cpal/tree/v0.18.1/src/host) — read directly: `Cargo.toml` (feature flags, `pipewire = "0.10"` pin), `src/host/pipewire/device.rs` (sink-as-Duplex-device, `STREAM_CAPTURE_SINK` auto-set), `src/host/pulseaudio/mod.rs` (`devices()` unfiltered `list_sources()`) — resolves the cpal-monitor-source open question
- [flatpak/xdg-desktop-portal discussion #1142](https://github.com/flatpak/xdg-desktop-portal/discussions/1142) — re-fetched for this pass; still open, opened Oct 2023, latest visible substantial activity 2024
- [Embrace The Red — CVE-2026-5674 writeup](https://embracethered.com/blog/posts/2026/pipewire-flatpak-linux-sandbox-escape-cve-2026-5674/) — pipewire-pulse sandbox escape via `--socket=pulseaudio`, fixed upstream, distro patches by late July 2026
- [Debian Wiki PipeWire](https://wiki.debian.org/PipeWire) (re-checked) — Debian 12/Bookworm default is GNOME-specific, not distro-wide; Debian 13/Trixie is default across "most" desktop environments
- GNOME Settings Privacy/Microphones panel disclaimer and [KDE Discuss — No Privacy option to block mic access](https://discuss.kde.org/t/no-privacy-option-on-kde-to-block-programs-from-accessing-mic/31211) — confirms neither desktop has a functional per-app mic consent prompt as of Aug 2026

## Fact-check log (2026-08-03)

**CONFIRMED (no change needed):**
- All eight crate versions, licenses, and download counts in the API-layers table and Sources
  list — checked directly against `crates.io/api/v1/crates/<name>` with a proper User-Agent
  header (the bare API call 403s without one). Every number matched exactly: `pipewire`
  0.10.0/MIT, `cpal` 0.18.1/Apache-2.0, `libpulse-binding` 2.30.1/MIT OR Apache-2.0,
  `libpulse-simple-binding` 2.29.0/same, `alsa` 0.12.1/Apache-2.0 OR MIT, `alsa-sys`
  0.6.1/MIT, `ashpd` 0.13.13/MIT, `jack` 0.13.5/MIT.
- `pipewire` crate is the official freedesktop.org binding — `repository` field on crates.io
  points to `gitlab.freedesktop.org/pipewire/pipewire-rs`.
- Fedora 34+ as the PipeWire-audio-default distro claim — Fedora Magazine and the Fedora
  Change wiki both confirm Fedora 34, not 35, despite contemporaneous press uncertainty.
- Ubuntu 22.10+ claim — confirmed; Ubuntu 22.04 used PipeWire for video only, PulseAudio kept
  audio until 22.10 dropped PulseAudio from the desktop image.
- `stream.capture.sink` and `target.object` as the correct, current PipeWire property names
  for monitor capture — confirmed verbatim against `docs.pipewire.org/page_module_protocol_simple.html`.
- `org.freedesktop.portal.ScreenCast` has no audio parameter — confirmed against the live
  `ScreenCast.xml` spec on the `flatpak/xdg-desktop-portal` `main` branch; only video-related
  methods/options exist.
- Discussion #1142 "Audio portal" exists, opened October 2023, and is genuinely still open/
  unresolved — confirmed by re-fetching the discussion.
- `libpipewire-module-combine-stream` exists, is documented, and its documented use cases are
  multi-sink/multi-source aggregation (e.g. building a 5.1 virtual sink from stereo pairs) —
  not documented for mic+monitor combination specifically. The doc's `[unverified]` marker on
  that specific use case was correct to leave in place; confirmed, not resolved.
- `pw_time` is the correct timestamp API for aligning two streams sharing one PipeWire graph —
  confirmed against `docs.pipewire.org/structpw__time.html`; its `delay`/`ticks`/`queued`
  fields are exactly what's needed for cross-stream alignment via extrapolation.
- Linux has no OS-level audio-capture permission gate comparable to macOS TCC, and neither
  GNOME nor KDE has shipped one as of Aug 2026 — confirmed via GNOME Settings' own privacy-
  panel disclaimer and a live KDE Discuss thread tracking the same gap.
- Flatpak's `--socket=pulseaudio` / `--filesystem=xdg-run/pipewire-0` grant syntax is current
  — confirmed against Flatpak's own sandbox-permissions docs.

**CORRECTED (said → true, with source):**
- "PipeWire is the default... on... Debian 12+" → **too broad**. Debian 12 (Bookworm) defaults
  to PipeWire *only for the GNOME desktop*; other Debian 12 desktop-environment flavors are not
  covered by that default. Debian 13 (Trixie) extends the default to "most" desktop
  environments per the Debian Wiki, still not stated as universal. Source:
  [wiki.debian.org/PipeWire](https://wiki.debian.org/PipeWire). (The Debian 13 PipeWire
  1.4.2/WirePlumber 0.5.8 version figures were already correct and are unchanged.)
- The `[unverified]` on whether `cpal` enumerates PulseAudio/PipeWire monitor sources as input
  devices → **resolved to yes**, for both the `pulseaudio` and `pipewire` cpal features,
  contrary to the original doc's cautious "don't rely on cpal for the system-audio leg"
  guidance. Source: direct read of `RustAudio/cpal` tag `v0.18.1` — `src/host/pulseaudio/mod.rs`
  applies no monitor-source filter in `devices()`, and `src/host/pipewire/device.rs` exposes
  sink nodes as `Duplex` devices that auto-set `STREAM_CAPTURE_SINK` when opened for input.
  This changes the practical guidance (see the updated `cpal` on Linux section and
  Recommendation) but not the primary recommendation, since the native `pipewire` crate still
  offers finer-grained targeting cpal's device model lacks.
- The `[unverified]` on the exact `pipewire` 0.10.0 `keys` constant names for
  `stream.capture.sink`/`target.object` → **resolved**: `pw::keys::STREAM_CAPTURE_SINK` and
  `pw::keys::TARGET_OBJECT` are correct, confirmed via cpal v0.18.1's direct dependency on
  `pipewire = "0.10"` and its use of exactly those constants.

**STILL UNVERIFIED (honest — not resolved by this pass):**
- Whether `libpipewire-module-combine-stream` is commonly/practically used to merge a mic node
  and a sink-monitor specifically (as opposed to its documented multi-mic/multi-sink use
  cases). No example or discussion found either confirming or ruling this out.
- Exact `-dev`/`-devel` package names and minimum versions per current Fedora/Debian release —
  not re-verified against live package search in this pass; the doc's own `[unverified]`
  marker on this stands.
- Exact `audio`-group/udev-rule details for bare-ALSA device permissions on meetrs' actual
  target distros — not re-verified; doc's `[unverified]` marker stands.
- Whether AppImage genuinely imposes zero portal/socket restrictions beyond a normal binary —
  not independently re-checked in this pass; doc's `[unverified]` marker stands.
- Whether the XDG Audio portal discussion (#1142) has any un-surfaced 2025/2026 activity not
  visible via a single page fetch — the fetch tool showed activity through 2024 only; GitHub
  Discussions can have comments that don't render fully via a simple fetch, so treat "still
  open, no visible recent movement" as a snapshot, not a guarantee of zero 2026 progress.
