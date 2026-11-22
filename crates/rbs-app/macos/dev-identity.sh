#!/bin/sh
# Creates the local code signing identity "RBS Development" in the login keychain,
# once per Mac. bundle.sh signs RBS.app with it, so macOS privacy grants (screen
# recording, input monitoring, camera, microphone) survive rebuilds: TCC keys a
# grant on the bundle id and the signing certificate instead of the build's code
# hash (ADR-0141). The certificate is self-signed and only for development builds
# on this Mac; it is never used for a release.
#   crates/rbs-app/macos/dev-identity.sh
set -eu
name=${RBS_SIGN_IDENTITY:-RBS Development}
keychain="$HOME/Library/Keychains/login.keychain-db"

if security find-certificate -c "$name" "$keychain" >/dev/null 2>&1; then
    echo "\"$name\" already exists."
    exit 0
fi

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
cat >"$dir/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $name
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$dir/cert.cnf" \
    -keyout "$dir/key.pem" -out "$dir/cert.pem" 2>/dev/null
# OpenSSL 3 writes PKCS#12 files the macOS keychain cannot read unless asked for
# the legacy algorithms; LibreSSL (the system openssl) does not know the flag.
legacy=""
if openssl version | grep -q '^OpenSSL 3'; then
    legacy="-legacy"
fi
# shellcheck disable=SC2086
openssl pkcs12 -export $legacy -inkey "$dir/key.pem" -in "$dir/cert.pem" \
    -name "$name" -passout pass:rbs -out "$dir/id.p12"
security import "$dir/id.p12" -k "$keychain" -P rbs -T /usr/bin/codesign >/dev/null
echo "Created \"$name\". bundle.sh now signs RBS.app with it."
echo "Grants given to an earlier, ad hoc signed RBS.app do not carry over: remove"
echo "RBS from the privacy lists once (or run tccutil reset All app.rbs.RBS)."
