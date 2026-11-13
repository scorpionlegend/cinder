#!/usr/bin/env bash
# Build a compressed Cinder .dmg from a built Cinder.app.
#
#   make-dmg.sh <Cinder.app> <volume name> <output.dmg>
#
# The image holds the app plus the usual /Applications symlink so the user can
# drag it into place. It is then mounted again and checked, because a dmg that
# builds but does not mount is worse than no dmg at all.
set -euo pipefail

app=${1:?usage: make-dmg.sh <Cinder.app> <volume name> <output.dmg>}
volume=${2:?usage: make-dmg.sh <Cinder.app> <volume name> <output.dmg>}
out=${3:?usage: make-dmg.sh <Cinder.app> <volume name> <output.dmg>}

if [ ! -d "$app" ]; then
  echo "make-dmg: $app is not a directory" >&2
  exit 1
fi

stage=$(mktemp -d "${TMPDIR:-/tmp}/cinder-dmg.XXXXXX")
mount=$(mktemp -d "${TMPDIR:-/tmp}/cinder-mnt.XXXXXX")
cleanup() {
  hdiutil detach "$mount" -quiet -force >/dev/null 2>&1 || true
  rm -rf "$stage" "$mount"
}
trap cleanup EXIT

# ditto preserves the framework's symlinks and extended attributes; cp -R does
# not reliably do either for a bundle.
ditto "$app" "$stage/Cinder.app"
ln -s /Applications "$stage/Applications"

mkdir -p "$(dirname "$out")"
rm -f "$out"
hdiutil create \
  -volname "$volume" \
  -srcfolder "$stage" \
  -fs HFS+ \
  -format UDZO \
  -ov \
  "$out"

hdiutil attach "$out" -nobrowse -readonly -mountpoint "$mount" -quiet
if [ ! -x "$mount/Cinder.app/Contents/MacOS/cinder" ]; then
  echo "make-dmg: mounted image has no Cinder.app/Contents/MacOS/cinder" >&2
  exit 1
fi
if [ ! -L "$mount/Applications" ]; then
  echo "make-dmg: mounted image has no /Applications symlink" >&2
  exit 1
fi
hdiutil detach "$mount" -quiet

echo "make-dmg: $out ($(du -h "$out" | cut -f1))"
