#!/bin/sh
# Sign Buddy.app with the stable local self-signed identity ("Buddy Local Dev").
#
# Why: macOS TCC (Documents/Desktop/Downloads permission prompts) keys consent
# to the app's code signature. Ad-hoc signed builds change hash every build, so
# macOS re-prompts after every rebuild. Signing with a stable certificate makes
# the consent persist across rebuilds — allow once, never asked again.
#
# One-time setup (already done on this machine, see docs/UPDATER.md):
#   1. Generate a self-signed code-signing cert (CN="Buddy Local Dev")
#   2. Import it into ~/Library/Keychains/buddy-dev.keychain-db (password: buddy-dev)
#
# Usage: sh scripts/sign-local.sh [path/to/Buddy.app]

set -e

APP="${1:-$(dirname "$0")/../src-tauri/target/release/bundle/macos/Buddy.app}"
KEYCHAIN="$HOME/Library/Keychains/buddy-dev.keychain-db"
IDENTITY="Buddy Local Dev"

security unlock-keychain -p buddy-dev "$KEYCHAIN"
codesign --force --deep --keychain "$KEYCHAIN" --sign "$IDENTITY" "$APP"
codesign -dv "$APP" 2>&1 | grep -E '^(Identifier|Signature)' || true
echo "signed: $APP"
