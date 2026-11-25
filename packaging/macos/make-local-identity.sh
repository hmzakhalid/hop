#!/bin/bash
# Create "Hop Local Development", a self-signed code-signing certificate in the login keychain.
#
# build.sh signs with it when there is no Developer ID. macOS ties Hop's permissions
# (Accessibility, Input Monitoring) to the signing identity. An ad-hoc signature is new on every
# build, so every rebuild asked for them again; this certificate stays the same. It is for this
# Mac only: other Macs do not trust it, so releases still need a Developer ID.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/config.sh"
if security find-identity -p codesigning | grep -q "\"$LOCAL_SIGN_NAME\""; then
  echo "$LOCAL_SIGN_NAME is already in the keychain."
  exit 0
fi
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cat > "$TMP/cert.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $LOCAL_SIGN_NAME
O = $APP_NAME
OU = Local Development
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
CNF
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 3650 -config "$TMP/cert.cnf" 2>/dev/null
PW=$(openssl rand -hex 16)
# The older PKCS#12 algorithms, which every macOS keychain imports.
openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -name "$LOCAL_SIGN_NAME" -out "$TMP/id.p12" \
  -passout "pass:$PW" -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES -macalg sha1
security import "$TMP/id.p12" -k "$HOME/Library/Keychains/login.keychain-db" -P "$PW" -T /usr/bin/codesign
echo "Created $LOCAL_SIGN_NAME. The next build.sh signs with it."
