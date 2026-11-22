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
# The system openssl (LibreSSL) writes PKCS#12 files the keychain can read; a newer
# OpenSSL on PATH (Homebrew) writes ones it cannot.
ssl=/usr/bin/openssl
"$ssl" req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$dir/cert.cnf" \
    -keyout "$dir/key.pem" -out "$dir/cert.pem" 2>/dev/null
"$ssl" pkcs12 -export -inkey "$dir/key.pem" -in "$dir/cert.pem" \
    -name "$name" -passout pass:rbs -out "$dir/id.p12"
security import "$dir/id.p12" -k "$keychain" -P rbs -T /usr/bin/codesign >/dev/null
echo "Created \"$name\". bundle.sh now signs RBS.app with it."
echo "Grants given to an earlier, ad hoc signed RBS.app do not carry over: remove"
echo "RBS from the privacy lists once (or run tccutil reset All app.rbs.RBS)."
