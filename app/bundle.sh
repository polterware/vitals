#!/usr/bin/env bash
# Builds Vitals.app for macOS: the release binary, the agents in Resources/agent (from
# agent/build.sh), the Info.plist, and the Icon Composer icon (app/resources/vitals.icon,
# compiled into Assets.car and vitals.icns). It is signed ad hoc, which runs on this Mac; a
# release needs a Developer ID and notarization.
#
#   bash app/bundle.sh            # writes target/bundle/Vitals.app
#   VITALS_BUNDLE_OUT=dir bash app/bundle.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="${VITALS_BUNDLE_OUT:-$root/target/bundle}"
app="$out/Vitals.app"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"

cargo build --quiet --release --locked -p vitals-app --manifest-path "$root/Cargo.toml"
bash "$root/agent/build.sh"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/agent"
cp "$root/target/release/vitals" "$app/Contents/MacOS/vitals"
cp "$root"/agent/dist/vitals-agent-* "$app/Contents/Resources/agent/"

# The Icon Composer icon: Assets.car for macOS 26 and vitals.icns for the systems before it.
xcrun actool "$root/app/resources/vitals.icon" --compile "$app/Contents/Resources" \
  --platform macosx --minimum-deployment-target 13.0 --app-icon vitals \
  --output-partial-info-plist "$out/icon-info.plist" >/dev/null

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Vitals</string>
  <key>CFBundleDisplayName</key><string>Vitals</string>
  <key>CFBundleIdentifier</key><string>com.polterware.vitals</string>
  <key>CFBundleExecutable</key><string>vitals</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleIconName</key><string>vitals</string>
  <key>CFBundleIconFile</key><string>vitals</string>
</dict>
</plist>
PLIST

codesign --force --deep --sign - "$app" >/dev/null 2>&1 || echo "codesign failed; the bundle is unsigned" >&2

# The bundle is rebuilt in place, so tell Launch Services it changed: otherwise the Dock and
# Finder keep the icon they cached while the old bundle was gone, the blank placeholder.
touch "$app"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$app" >/dev/null 2>&1 || true
echo "$app"
