set shell := ["bash", "-uc"]

# release only: this is a realtime audio path, a debug build risks ring-buffer overruns.
profile := "release"
bin := "target/" + profile + "/meetrs"
app := bin + ".app"
exe := app + "/Contents/MacOS/meetrs"

# where `just install` puts things. appdir holds the bundle (TCC consent is keyed
# to it, so it needs a stable home outside target/); bindir holds the PATH entry.
appdir := env("MEETRS_APPDIR", "$HOME/Applications")
bindir := env("MEETRS_BINDIR", "$HOME/.local/bin")

# list available recipes
default:
    @just --list

# build the release binary
build:
    cargo build --release

# rebuild the .app only when missing or the binary is newer than what's bundled
bundle: build
    #!/usr/bin/env bash
    if [ ! -e "{{exe}}" ] || [ "{{bin}}" -nt "{{exe}}" ]; then
        ./scripts/bundle.sh {{profile}}
    fi

# build, bundle if needed, and launch the TUI recorder
run: bundle
    exec "{{exe}}"

# build, bundle if needed, and run headless calibration (sets MEETRS_SILENCE_RMS)
check: bundle
    exec "{{exe}}" --check

# full-text search every transcript: just search "budget OR headcount"
search +QUERY: bundle
    exec "{{exe}}" --search {{QUERY}}

# rebuild the SQLite index from the JSON on disk (safe: the DB is derived)
reindex: bundle
    exec "{{exe}}" --reindex

# Deletes a chunk's WAV only after its FLAC verifies; untranscribed chunks are
# left alone. New sessions compress themselves at session end.
#
# compress older sessions to FLAC: just compress [session-dir...]
compress *DIRS: bundle
    exec "{{exe}}" --compress {{DIRS}}

# install so `meetrs` works from any directory, recording included
install: bundle
    #!/usr/bin/env bash
    set -euo pipefail
    dest="{{appdir}}/meetrs.app"
    link="{{bindir}}/meetrs"

    # The PATH entry is a symlink INTO the bundle, not a copy of the binary.
    # TCC keys audio consent to the bundle's Info.plist and code signature, so a
    # bare binary on PATH (what `cargo install` produces) can never be granted
    # microphone or system-audio access — see scripts/bundle.sh.
    mkdir -p "{{appdir}}" "{{bindir}}"
    rm -rf "$dest"
    cp -R "{{app}}" "$dest"
    # Re-sign after the copy so the installed bundle's ad-hoc signature covers
    # the files at their final path.
    codesign -s - -f "$dest" >/dev/null 2>&1
    ln -sfn "$dest/Contents/MacOS/meetrs" "$link"

    echo "installed $dest"
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
