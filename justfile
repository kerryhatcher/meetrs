set shell := ["bash", "-uc"]

# release only: this is a realtime audio path, a debug build risks ring-buffer overruns.
profile := "release"
bin := "target/" + profile + "/meetrs"
app := bin + ".app"
exe := app + "/Contents/MacOS/meetrs"

# macOS only: where `just install` puts things. appdir holds the bundle (TCC consent is keyed
# to it, so it needs a stable home outside target/); bindir holds the PATH entry.
appdir := env("MEETRS_APPDIR", "$HOME/Applications")
bindir := env("MEETRS_BINDIR", "$HOME/.local/bin")
installed := appdir + "/meetrs.app"
installed_exe := installed + "/Contents/MacOS/meetrs"

# list available recipes
default:
    @just --list

# GPU backend compiled into the binary: MEETRS_FEATURES=vulkan, =cuda, or unset for CPU.
gpu_features := env("MEETRS_FEATURES", "")

# build the release binary; skipped when nothing changed and the GPU features match
build:
    #!/usr/bin/env bash
    set -euo pipefail
    stamp="{{bin}}.features"
    want="{{gpu_features}}"
    have=$(cat "$stamp" 2>/dev/null || true)
    if [ -x "{{bin}}" ] && [ "$have" = "$want" ] && [ -z "$(find src Cargo.toml Cargo.lock -newer "{{bin}}" -print -quit)" ]; then
        echo "up to date: {{bin}} (features: ${want:-cpu})"
        exit 0
    fi
    args=()
    if [ -n "$want" ]; then args=(--features "$want"); fi
    cargo build --release "${args[@]}"
    printf '%s' "$want" > "$stamp"

# rebuild the .app only when missing or the binary is newer than what's bundled
[macos]
bundle: build
    #!/usr/bin/env bash
    if [ ! -e "{{exe}}" ] || [ "{{bin}}" -nt "{{exe}}" ]; then
        ./scripts/bundle.sh {{profile}}
    fi
    # An up-to-date bundle can still predate `just signing-cert`, and `check`/`search`
    # exec *this* copy rather than the installed one — so it has to carry the same
    # identity, or TCC consent ends up split between two of them. --if-needed makes
    # this a no-op once it matches.
    ./scripts/sign.sh --if-needed "{{app}}"

# Deliberately runs the *installed* bundle rather than target/: that is the copy
# System Settings lists and attaches consent to, and it lives at a stable path, so
# launching it is the configuration that was actually granted. meetrs preflights
# capture itself before taking the screen, and refuses to start a session that
# would provably record nothing.
#
# R2 credentials come from a SOPS-encrypted env file (MEETRS_SECRETS, default
# ~/secrets/meetrs.env), injected only into the app's process. If sops, the file,
# or its decryption is unavailable, the app still starts, with uploads off.
#
# build, install, and launch the TUI recorder
[macos]
run: install
    #!/usr/bin/env bash
    secrets="${MEETRS_SECRETS:-$HOME/secrets/meetrs.env}"
    app="{{installed_exe}}"
    start_without_uploads() {
        echo "meetrs: WARNING: $1; starting without R2 credentials, so uploads are off" >&2
        exec "$app"
    }
    command -v sops >/dev/null 2>&1 || start_without_uploads "sops is not installed"
    [ -f "$secrets" ] || start_without_uploads "$secrets not found"
    sops decrypt "$secrets" >/dev/null 2>&1 || start_without_uploads "could not decrypt $secrets"
    exec sops exec-env "$secrets" "\"$app\""

# Same SOPS-injected R2 credentials as the macOS `run` above; no bundle or install step.
# build, then launch the TUI recorder straight from target/release (Linux)
[linux]
run: build
    #!/usr/bin/env bash
    secrets="${MEETRS_SECRETS:-$HOME/secrets/meetrs.env}"
    app="{{bin}}"
    start_without_uploads() {
        echo "meetrs: WARNING: $1; starting without R2 credentials, so uploads are off" >&2
        exec "$app"
    }
    command -v sops >/dev/null 2>&1 || start_without_uploads "sops is not installed"
    [ -f "$secrets" ] || start_without_uploads "$secrets not found"
    sops decrypt "$secrets" >/dev/null 2>&1 || start_without_uploads "could not decrypt $secrets"
    exec sops exec-env "$secrets" "\"$app\""

# build, bundle if needed, and run headless calibration (sets MEETRS_SILENCE_RMS)
[macos]
check: bundle
    exec "{{exe}}" --check

# build, then run headless calibration: channel layout and per-leg levels (Linux)
[linux]
check: build
    exec "{{bin}}" --check

# full-text search every transcript: just search "budget OR headcount"
[macos]
search +QUERY: bundle
    exec "{{exe}}" --search {{QUERY}}

# full-text search every transcript (Linux): just search "budget OR headcount"
[linux]
search +QUERY: build
    exec "{{bin}}" --search {{QUERY}}

# rebuild the SQLite index from the JSON on disk (safe: the DB is derived)
[macos]
reindex: bundle
    exec "{{exe}}" --reindex

# rebuild the SQLite index from the JSON on disk (Linux; safe: the DB is derived)
[linux]
reindex: build
    exec "{{bin}}" --reindex

# Deletes a chunk's WAV only after its FLAC verifies; untranscribed chunks are
# left alone. New sessions compress themselves at session end.
#
# Arguments reach the script as "$@" and are quoted with printf %q, so a path
# with spaces or shell characters is passed through as one argument.
#
# upload sessions to R2 now, with the credentials from SOPS: just upload [session-dir...]
[macos]
[positional-arguments]
upload *DIRS: bundle
    #!/usr/bin/env bash
    set -euo pipefail
    secrets="${MEETRS_SECRETS:-$HOME/secrets/meetrs.env}"
    [ -f "$secrets" ] || { echo "no credentials file at $secrets" >&2; exit 1; }
    cmd=$(printf '%q ' "{{exe}}" --upload "$@")
    exec sops exec-env "$secrets" "$cmd"

# upload sessions to R2 now, with the credentials from SOPS (Linux): just upload [session-dir...]
[linux]
[positional-arguments]
upload *DIRS: build
    #!/usr/bin/env bash
    set -euo pipefail
    secrets="${MEETRS_SECRETS:-$HOME/secrets/meetrs.env}"
    [ -f "$secrets" ] || { echo "no credentials file at $secrets" >&2; exit 1; }
    cmd=$(printf '%q ' "{{bin}}" --upload "$@")
    exec sops exec-env "$secrets" "$cmd"

# compress older sessions to FLAC: just compress [session-dir...]
[macos]
[positional-arguments]
compress *DIRS: bundle
    #!/usr/bin/env bash
    set -euo pipefail
    exec "{{exe}}" --compress "$@"

# compress older sessions to FLAC (Linux): just compress [session-dir...]
[linux]
[positional-arguments]
compress *DIRS: build
    #!/usr/bin/env bash
    set -euo pipefail
    exec "{{bin}}" --compress "$@"

# install so `meetrs` works from any directory, recording included
[macos]
install: bundle
    #!/usr/bin/env bash
    set -euo pipefail
    dest="{{installed}}"
    link="{{bindir}}/meetrs"

    # The PATH entry is a symlink INTO the bundle, not a copy of the binary.
    # TCC keys audio consent to the bundle's Info.plist and code signature, so a
    # bare binary on PATH (what `cargo install` produces) can never be granted
    # microphone or system-audio access — see scripts/bundle.sh.
    mkdir -p "{{appdir}}" "{{bindir}}"

    # `just run` installs on every launch, so do nothing when nothing changed:
    # writing a fresh 6MB binary can stall for many seconds behind on-access
    # antivirus, which would otherwise be added to every single launch.
    #
    # Compared by CDHash, not by `cmp`: re-signing the copy below rewrites the CMS
    # blob, so the installed file is never byte-identical to the source even when
    # nothing changed. The CDHash covers the sealed content (executable, plist,
    # identifier) and not that blob, so it is stable across a re-sign and is what
    # actually answers "is the installed bundle this build?".
    cdhash() { codesign -dvvv "$1" 2>&1 | awk -F= '/^CDHash/ { print $2 }'; }
    built="$(cdhash "{{app}}")"
    if [ -n "$built" ] && [ "$built" = "$(cdhash "$dest")" ]; then
        echo "up to date $dest"
    else
        rm -rf "$dest"
        cp -R "{{app}}" "$dest"
        # Re-sign after the copy so the installed bundle's signature covers the
        # files at their final path. Same helper as bundle.sh, so the two cannot
        # drift onto different identities and split the TCC grant between them.
        ./scripts/sign.sh "$dest" >/dev/null
        echo "installed $dest"
    fi
    ln -sfn "{{installed_exe}}" "$link"
    echo "linked    $link"

    case ":$PATH:" in
        *":{{bindir}}:"*) ;;
        *) echo; echo "WARNING: {{bindir}} is not on your PATH — add it or set MEETRS_BINDIR." ;;
    esac

    # A bare `cargo install` binary earlier on PATH would silently take over and
    # fail to record, which is a miserable thing to debug.
    if [ -e "$HOME/.cargo/bin/meetrs" ] && [ ! -L "$HOME/.cargo/bin/meetrs" ]; then
        echo
        echo "NOTE: ~/.cargo/bin/meetrs is a bare binary and cannot get audio consent."
        echo "      Remove it so it can never shadow this install:  cargo uninstall meetrs"
    fi

# No bundle or consent step on Linux, so this just copies the binary.
# copy the release binary onto PATH (Linux; MEETRS_BINDIR, default ~/.local/bin)
[linux]
install: build
    #!/usr/bin/env bash
    set -euo pipefail
    dir="${MEETRS_BINDIR:-$HOME/.local/bin}"
    mkdir -p "$dir"
    install -m 0755 "{{bin}}" "$dir/meetrs"
    echo "installed $dir/meetrs"

    case ":$PATH:" in
        *":$dir:"*) ;;
        *) echo; echo "WARNING: $dir is not on your PATH — add it or set MEETRS_BINDIR." ;;
    esac

    if [ -e "$HOME/.cargo/bin/meetrs" ] && [ "$HOME/.cargo/bin/meetrs" != "$dir/meetrs" ]; then
        echo
        echo "NOTE: ~/.cargo/bin/meetrs exists and may shadow this install on PATH."
        echo "      Remove it if it is stale:  cargo uninstall meetrs"
    fi

# create this machine's local signing identity so audio consent survives
# rebuilds (once per machine; see scripts/sign.sh for why)
[macos]
signing-cert:
    ./scripts/signing-cert.sh

# run the test suite
test:
    cargo test

# fmt check + clippy, zero warnings
lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

# delete everything under ~/.meetrs/recordings (asks for confirmation first)
clean-recordings:
    #!/usr/bin/env bash
    dir="$HOME/.meetrs/recordings"
    if [ ! -d "$dir" ]; then
        echo "no recordings dir at $dir"
        exit 0
    fi
    echo "This will permanently delete everything in: $dir"
    ls -la "$dir"
    read -r -p "Type 'yes' to confirm: " confirm
    [ "$confirm" = "yes" ] || { echo "aborted"; exit 1; }
    rm -rf "${dir:?}"/*
