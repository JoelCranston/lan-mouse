#!/bin/bash
# Package the macOS app bundle for a release: sign it, build a DMG, notarize
# and staple it, and zip the app.
#
#   scripts/macos-release-package.sh lan-mouse-macos-arm64
#
# writes lan-mouse-macos-arm64.dmg and lan-mouse-macos-arm64.zip next to
# "Lan Mouse.app" in target/release/bundle/osx. Run it after cargo bundle and
# scripts/copy-macos-dylib.sh.
#
# Signing (all optional; without them the app keeps its ad-hoc signature):
#   MACOS_CERTIFICATE_P12       base64 of a "Developer ID Application" .p12
#   MACOS_CERTIFICATE_PASSWORD  password of that .p12
#   MACOS_KEYCHAIN_PASSWORD     password for the temporary keychain (any string)
#   MACOS_SIGN_IDENTITY         instead of the .p12: an identity already in
#                               your keychain (e.g. on your own Mac)
# Notarization (needs signing; skipped when unset):
#   APPLE_API_KEY_P8            contents of an App Store Connect API key (.p8)
#   APPLE_API_KEY_ID            its Key ID
#   APPLE_API_ISSUER_ID         its Issuer ID
#   NOTARY_KEYCHAIN_PROFILE     instead of the API key: a profile saved with
#                               `xcrun notarytool store-credentials`
set -euo pipefail

name="${1:?usage: $0 <artifact-name>}"
bundle_dir="${BUNDLE_DIR:-target/release/bundle/osx}"
app="$bundle_dir/Lan Mouse.app"
entitlements="build-aux/macos-entitlements.plist"
dmg="$bundle_dir/$name.dmg"
zip="$bundle_dir/$name.zip"

[ -d "$app" ] || { echo "missing $app; run cargo bundle first" >&2; exit 1; }

work="$(mktemp -d)"
keychain=""
cleanup() {
  if [ -n "$keychain" ]; then security delete-keychain "$keychain" || true; fi
  rm -rf "$work"
}
trap cleanup EXIT

identity="${MACOS_SIGN_IDENTITY:-}"

if [ -z "$identity" ] && [ -n "${MACOS_CERTIFICATE_P12:-}" ]; then
  keychain="$work/signing.keychain-db"
  kc_pass="${MACOS_KEYCHAIN_PASSWORD:-$(uuidgen)}"
  security create-keychain -p "$kc_pass" "$keychain"
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "$kc_pass" "$keychain"
  printf '%s' "$MACOS_CERTIFICATE_P12" | base64 --decode > "$work/cert.p12"
  security import "$work/cert.p12" -k "$keychain" \
    -P "${MACOS_CERTIFICATE_PASSWORD:-}" -T /usr/bin/codesign
  rm -f "$work/cert.p12"
  security set-key-partition-list -S apple-tool:,apple:,codesign: \
    -s -k "$kc_pass" "$keychain" > /dev/null
  # Put the temporary keychain first in the search list so codesign finds it.
  # shellcheck disable=SC2046
  security list-keychains -d user -s "$keychain" \
    $(security list-keychains -d user | tr -d '"')
  identity="$(security find-identity -v -p codesigning "$keychain" \
    | awk '/Developer ID Application/ { print $2; exit }')"
  [ -n "$identity" ] || { echo "no Developer ID Application identity in the .p12" >&2; exit 1; }
fi

if [ -n "$identity" ]; then
  echo "Signing with Developer ID"
  # Inside-out, like copy-macos-dylib.sh, but with the hardened runtime and a
  # secure timestamp, which notarization requires.
  sign=(codesign --force --timestamp --options runtime --sign "$identity")
  find "$app/Contents/Frameworks" -type f -exec "${sign[@]}" {} +
  "${sign[@]}" --entitlements "$entitlements" "$app"
  codesign --verify --strict --deep --verbose=2 "$app"
else
  echo "No signing identity; keeping the ad-hoc signature (users must clear quarantine)"
fi

# DMG: the app next to an Applications link to drag it onto.
stage="$work/dmg"
mkdir -p "$stage"
ditto "$app" "$stage/Lan Mouse.app"
ln -s /Applications "$stage/Applications"
rm -f "$dmg"
hdiutil create -volname "Lan Mouse" -srcfolder "$stage" -ov -format UDZO "$dmg"

if [ -n "$identity" ]; then
  codesign --force --timestamp --sign "$identity" "$dmg"
fi

auth=()
if [ -n "${NOTARY_KEYCHAIN_PROFILE:-}" ]; then
  auth=(--keychain-profile "$NOTARY_KEYCHAIN_PROFILE")
elif [ -n "${APPLE_API_KEY_P8:-}" ]; then
  key="$work/AuthKey.p8"
  printf '%s\n' "$APPLE_API_KEY_P8" > "$key"
  auth=(--key "$key" --key-id "${APPLE_API_KEY_ID:?}" --issuer "${APPLE_API_ISSUER_ID:?}")
fi

if [ -n "$identity" ] && [ ${#auth[@]} -gt 0 ]; then
  echo "Notarizing $dmg"
  xcrun notarytool submit "$dmg" "${auth[@]}" --wait --output-format json \
    | tee "$work/notary.json" || true
  status="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status",""))' "$work/notary.json")"
  if [ "$status" != "Accepted" ]; then
    id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("id",""))' "$work/notary.json")"
    [ -n "$id" ] && xcrun notarytool log "$id" "${auth[@]}" || true
    echo "notarization failed: $status" >&2
    exit 1
  fi
  xcrun stapler staple "$dmg"
  # The ticket also covers the app, so the zip works offline too.
  xcrun stapler staple "$app" || echo "warning: could not staple the app itself"
  spctl --assess --type execute --verbose=2 "$app" || true
elif [ -n "$identity" ]; then
  echo "No notary credentials; signed but not notarized"
fi

rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
echo "Wrote $dmg and $zip"
