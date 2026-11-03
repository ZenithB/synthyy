#!/usr/bin/env bash
# Build, sign, notarize and verify the Synthyy macOS universal DMG.
#
#   scripts/release-macos.sh              signed + notarized (env vars required)
#   scripts/release-macos.sh --unsigned   local unsigned build, skips all checks
#
# Never put credentials in this file. Tauri reads everything from the
# environment (see docs/RELEASE.md):
#   APPLE_SIGNING_IDENTITY  "Developer ID Application: Name (TEAMID)"
#   APPLE_ID + APPLE_PASSWORD + APPLE_TEAM_ID      (app-specific password)   -or-
#   APPLE_API_KEY + APPLE_API_ISSUER + APPLE_API_KEY_PATH   (App Store Connect key)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

UNSIGNED=0
for arg in "$@"; do
  case "$arg" in
    --unsigned) UNSIGNED=1 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

# rustup-managed toolchain may not be on PATH in non-login shells
for tc in "$HOME"/.rustup/toolchains/stable-*-apple-darwin/bin; do
  [ -d "$tc" ] && export PATH="$tc:$PATH" && break
done
command -v cargo >/dev/null || { echo "cargo not found (install rustup)"; exit 1; }

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }

if [ "$UNSIGNED" = 0 ]; then
  missing=()
  [ -n "${APPLE_SIGNING_IDENTITY:-}" ] || missing+=("APPLE_SIGNING_IDENTITY  -> 'Developer ID Application: <Name> (<TEAMID>)'. Create the cert at developer.apple.com/account/resources/certificates (type: Developer ID Application), install it in login Keychain, then copy the name from: security find-identity -v -p codesigning")
  if [ -n "${APPLE_API_KEY:-}" ] || [ -n "${APPLE_API_ISSUER:-}" ] || [ -n "${APPLE_API_KEY_PATH:-}" ]; then
    [ -n "${APPLE_API_KEY:-}" ]       || missing+=("APPLE_API_KEY        -> App Store Connect API key ID (appstoreconnect.apple.com > Users and Access > Integrations > Team Keys)")
    [ -n "${APPLE_API_ISSUER:-}" ]    || missing+=("APPLE_API_ISSUER     -> Issuer ID shown on the same page")
    [ -n "${APPLE_API_KEY_PATH:-}" ]  || missing+=("APPLE_API_KEY_PATH   -> path to the downloaded AuthKey_<KEYID>.p8")
  else
    [ -n "${APPLE_ID:-}" ]       || missing+=("APPLE_ID        -> the Apple ID email of the developer account")
    [ -n "${APPLE_PASSWORD:-}" ] || missing+=("APPLE_PASSWORD  -> an APP-SPECIFIC password from appleid.apple.com > Sign-In and Security > App-Specific Passwords (not the account password)")
    [ -n "${APPLE_TEAM_ID:-}" ]  || missing+=("APPLE_TEAM_ID   -> 10-char Team ID from developer.apple.com/account > Membership details")
  fi
  if [ ${#missing[@]} -gt 0 ]; then
    red "Cannot sign/notarize — missing environment variables:"
    for m in "${missing[@]}"; do echo "  - $m"; done
    echo
    echo "Export them (or source a git-ignored .env.release.local) and re-run,"
    echo "or pass --unsigned for a local, unsigned DMG. See docs/RELEASE.md."
    exit 1
  fi
  if ! security find-identity -v -p codesigning | grep -Fq "$APPLE_SIGNING_IDENTITY"; then
    red "APPLE_SIGNING_IDENTITY '$APPLE_SIGNING_IDENTITY' is not in any keychain on this machine."
    security find-identity -v -p codesigning || true
    exit 1
  fi
else
  # Make sure a stray identity in the shell doesn't trigger signing.
  unset APPLE_SIGNING_IDENTITY APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH
  echo "Building UNSIGNED (no signing/notarization checks will run)."
fi

# Universal binary needs both std targets installed.
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  rustup target list --installed | grep -qx "$t" || rustup target add "$t"
done

echo "==> npm run tauri build --target universal-apple-darwin"
npm run tauri -- build --target universal-apple-darwin --bundles app,dmg

BUNDLE_DIR="$ROOT/target/universal-apple-darwin/release/bundle"
[ -d "$BUNDLE_DIR" ] || BUNDLE_DIR="$ROOT/src-tauri/target/universal-apple-darwin/release/bundle"
APP="$(ls -d "$BUNDLE_DIR"/macos/*.app | head -1)"
DMG="$(ls "$BUNDLE_DIR"/dmg/*.dmg | head -1)"

echo
echo "==> lipo -archs"
EXE="$APP/Contents/MacOS/$(/usr/libexec/PlistBuddy -c "Print :CFBundleExecutable" "$APP/Contents/Info.plist")"
lipo -archs "$EXE"

if [ "$UNSIGNED" = 0 ]; then
  echo "==> codesign -dv --verbose=4"
  codesign -dv --verbose=4 "$APP" 2>&1 | grep -E 'Authority|TeamIdentifier|Timestamp|flags' || true
  codesign --verify --deep --strict --verbose=2 "$APP"
  echo "==> entitlements"
  codesign -d --entitlements - "$APP" 2>/dev/null | grep -E 'audio-input' || red "warning: audio-input entitlement missing"
  echo "==> spctl -a -vv (Gatekeeper)"
  spctl -a -vv -t exec "$APP"
  echo "==> xcrun stapler validate"
  xcrun stapler validate "$APP"
  xcrun stapler validate "$DMG"
  spctl -a -vv -t open --context context:primary-signature "$DMG"
  green "Signed, notarized and stapled."
else
  codesign -dv "$APP" 2>&1 | head -3 || true
fi

echo
green "APP: $APP ($(du -sh "$APP" | cut -f1))"
green "DMG: $DMG ($(du -sh "$DMG" | cut -f1))"
