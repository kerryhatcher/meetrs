#!/usr/bin/env bash
# Create meetrs' local code-signing identity. Run once per machine.
#
# See scripts/sign.sh for why a certificate rather than ad-hoc signing is what
# keeps a TCC grant alive across rebuilds. This certificate is self-signed and
# never leaves this machine: it is not a Developer ID, it cannot notarize, and
# the system does not trust it — none of which matters, because codesign will
# still sign with it and TCC only needs the identity to be stable.
set -euo pipefail

IDENTITY="${MEETRS_SIGNING_IDENTITY:-meetrs local signing}"
KEYCHAIN="${MEETRS_SIGNING_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"

# Not `-v`, for the same reason as in sign.sh: self-signed means untrusted, and
# untrusted identities are omitted from the valid list while staying usable.
if security find-identity -p codesigning | grep -qF "\"$IDENTITY\""; then
    echo "identity '$IDENTITY' already exists — nothing to do"
    exit 0
fi

work="$(mktemp -d)"
# The private key sits in here until it is imported, so clean up on every exit.
trap 'rm -rf "$work"' EXIT

# Only protects the PKCS#12 file in between the two commands below, and that file
# is deleted on exit — so it is generated rather than chosen, and never stored.
password="$(openssl rand -hex 16)"

# codeSigning EKU plus digitalSignature is what makes codesign accept the
# identity at all; CA:false keeps it a leaf certificate.
openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
    -keyout "$work/key.pem" -out "$work/cert.pem" \
    -subj "/CN=$IDENTITY" \
    -addext "basicConstraints=critical,CA:false" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=codeSigning" 2>/dev/null

# -keypbe/-certpbe/-macalg pin the older algorithms Security.framework accepts.
# OpenSSL 3 defaults to a PKCS#12 MAC that `security import` rejects outright,
# and it does so as "MAC verification failed ... (wrong password?)" — which sends
# you hunting for a password problem that does not exist.
openssl pkcs12 -export -out "$work/identity.p12" \
    -inkey "$work/key.pem" -in "$work/cert.pem" \
    -passout "pass:$password" \
    -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1

# -T grants codesign access up front so signing never raises a keychain prompt.
# Scoped to codesign on purpose rather than `-A` (any application): a TCC grant
# is keyed to this certificate plus the bundle id, so anything able to sign with
# this key could adopt meetrs' microphone and system-audio consent.
security import "$work/identity.p12" -k "$KEYCHAIN" -P "$password" \
    -T /usr/bin/codesign

echo "created identity '$IDENTITY' in $KEYCHAIN"
echo
echo "Now rebuild and reinstall so the bundle carries it:"
echo "  just install"
echo
echo "This changes meetrs' code identity, so macOS asks for microphone and"
echo "system-audio consent one final time. After that, consent survives"
echo "rebuilds instead of resetting on every one."
