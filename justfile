set shell := ["bash", "-uc"]

# release only: this is a realtime audio path, a debug build risks ring-buffer overruns.
profile := "release"
bin := "target/" + profile + "/meetrs"
app := bin + ".app"
exe := app + "/Contents/MacOS/meetrs"

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

# compress older sessions to FLAC (new ones compress themselves at session end).
# Deletes a chunk's WAV only after its FLAC verifies; untranscribed chunks are
# left alone. Pass session dirs to limit it: just compress ~/.meetrs/recordings/2026-08-03T17-57-50
compress *DIRS: bundle
    exec "{{exe}}" --compress {{DIRS}}

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
