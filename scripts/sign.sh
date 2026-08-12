#!/usr/bin/env bash
# Sign a bundle with meetrs' stable local identity, falling back to ad-hoc.
#
#   sign.sh [--if-needed] <path>
#
# This matters for more than tamper-proofing: TCC keys a consent grant to the
# bundle's *designated requirement*. Ad-hoc signing has no certificate, so that
# requirement degrades to the code hash —
#
#     designated => cdhash H"25a384c0..."
#
# — which changes on every rebuild, silently invalidating the grant. tccd logs
# "Failed to match existing code requirement", treats the app as unknown, and
# consent has to be given again. Signed with a certificate it becomes
# identity-based instead —
#
#     designated => identifier "com.kerryhatcher.meetrs" and certificate leaf = H"..."
#
# — which is stable across rebuilds, so consent is given once and stays. Create
# the identity with `just signing-cert`.
#
# --if-needed skips the work when the target already carries the expected
# identity and still verifies. Signing is normally ~0.2s, but writing a fresh
# binary can stall many seconds behind on-access antivirus, so the callers that
# run on every `just run` use it.
set -euo pipefail

IF_NEEDED=0
if [ "${1:-}" = "--if-needed" ]; then
    IF_NEEDED=1
    shift
fi
TARGET="${1:?usage: sign.sh [--if-needed] <path-to-bundle-or-binary>}"
IDENTITY="${MEETRS_SIGNING_IDENTITY:-meetrs local signing}"

# Quiet on success, loud on failure: codesign chatters "replacing existing
# signature" to stderr on every rebuild, but a real signing failure must not be
# swallowed the way `2>&1 >/dev/null` used to swallow it.
sign() {
    local output
    if ! output=$(codesign "$@" 2>&1); then
        printf '%s\n' "$output" >&2
        return 1
    fi
}

# The SHA-1 in find-identity's listing is the same hash the designated
# requirement records as `certificate leaf`, just upper-case there.
#
# Deliberately not `find-identity -v`: this certificate is self-signed, so it has
# no Apple anchor and is never listed among "valid" identities — while codesign
# still signs with it perfectly well. Matching on the name is what works.
identity_hash() {
    security find-identity -p codesigning \
        | awk -v want="\"$IDENTITY\"" 'index($0, want) { print tolower($2); exit }'
}

# True when TARGET is already sealed by this exact certificate and the seal is
# intact, so re-signing would be a no-op. `codesign -d` reports on stderr.
already_signed_with() {
    local want="$1" requirement
    requirement="$(codesign -d -r- "$TARGET" 2>&1 || true)"
    case "$requirement" in
        *"leaf = H\"$want\""*) codesign --verify "$TARGET" >/dev/null 2>&1 ;;
        *) return 1 ;;
    esac
}

hash="$(identity_hash)"
if [ -n "$hash" ]; then
    if [ "$IF_NEEDED" = 1 ] && already_signed_with "$hash"; then
        exit 0
    fi
    sign -s "$IDENTITY" -f "$TARGET"
    echo "signed $TARGET with '$IDENTITY'"
else
    sign -s - -f "$TARGET"
    echo "signed $TARGET ad-hoc — macOS will ask for audio consent again after"
    echo "  every rebuild. Run \`just signing-cert\` once to make consent stick."
fi
