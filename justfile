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
installed := appdir + "/meetrs.app"
installed_exe := installed + "/Contents/MacOS/meetrs"

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
    # An up-to-date bundle can still predate `just signing-cert`, and `check`/`search`
    # exec *this* copy rather than the installed one — so it has to carry the same
    # identity, or TCC consent ends up split between two of them. --if-needed makes
    # this a no-op once it matches.
    ./scripts/sign.sh --if-needed "{{app}}"

# build, install, and launch the TUI recorder
#
# Deliberately runs the *installed* bundle rather than target/: that is the copy
# System Settings lists and attaches consent to, and it lives at a stable path, so
# launching it is the configuration that was actually granted. meetrs preflights
# capture itself before taking the screen, and refuses to start a session that
# would provably record nothing.
run: install
    exec "{{installed_exe}}"

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

# create this machine's local signing identity so audio consent survives
# rebuilds (once per machine; see scripts/sign.sh for why)
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
