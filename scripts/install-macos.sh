#!/bin/bash
#
# Teach macOS to show previews for .blad archives.
#
# There is no Quick Look plugin here and no code of any kind. A blad archive starts with
# a complete JPEG — decoders stop at the FFD9 end marker and ignore everything after it —
# so all macOS needs is to be told that .blad is a kind of JPEG. It then renders previews
# in Finder, Quick Look, Spotlight and Preview using Apple's own decoder, which we never
# have to maintain, ship, or code-sign.
#
# The only reason an app bundle exists at all is that UTI declarations must live in one.
# It contains a plist and a stub; it is never launched.
#
# Undo with:  ./install-macos.sh --uninstall

set -euo pipefail

APP="${HOME}/Applications/blad.app"
UTI="dev.blad.archive"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

if [[ "${1:-}" == "--uninstall" ]]; then
    if [[ -d "$APP" ]]; then
        "$LSREGISTER" -u "$APP" 2>/dev/null || true
        rm -rf "$APP"
        echo "removed $APP"
    else
        echo "nothing installed at $APP"
    fi
    exit 0
fi

if [[ "$(uname)" != "Darwin" ]]; then
    echo "this script is macOS-only; on Linux use a .thumbnailer file instead" >&2
    exit 1
fi

mkdir -p "${APP}/Contents/MacOS"

cat > "${APP}/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.blad.uti</string>
  <key>CFBundleName</key><string>blad</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>blad-uti</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>LSUIElement</key><true/>

  <key>UTExportedTypeDeclarations</key>
  <array>
    <dict>
      <key>UTTypeIdentifier</key><string>dev.blad.archive</string>
      <key>UTTypeDescription</key><string>blad archive</string>
      <!-- Conforming to public.jpeg is what makes previews work: a blad archive opens
           with a complete JPEG, so Apple's decoder reads it correctly and ignores the
           archive payload that follows. -->
      <key>UTTypeConformsTo</key>
      <array>
        <string>public.jpeg</string>
      </array>
      <key>UTTypeTagSpecification</key>
      <dict>
        <key>public.filename-extension</key>
        <array><string>blad</string></array>
      </dict>
    </dict>
  </array>
</dict>
</plist>
PLIST

printf '#!/bin/sh\nexit 0\n' > "${APP}/Contents/MacOS/blad-uti"
chmod +x "${APP}/Contents/MacOS/blad-uti"

"$LSREGISTER" -f "$APP"

echo "installed $APP"
echo

# Verify against a real archive rather than declaring success. A registration that
# silently failed looks identical to one that worked until you open a folder.
probe=$(mktemp -d)
trap 'rm -rf "$probe"' EXIT

sample=""
if [[ -n "${1:-}" && -f "${1:-}" ]]; then
    sample="$1"
fi

if [[ -n "$sample" ]]; then
    cp "$sample" "${probe}/probe.blad"
    sleep 1
    kind=$(mdls -name kMDItemContentType -raw "${probe}/probe.blad" 2>/dev/null || echo "?")
    if [[ "$kind" == "$UTI" ]]; then
        echo "  type recognised: $kind"
    else
        echo "  WARNING: type is '$kind', expected '$UTI'" >&2
        echo "  LaunchServices may need a moment, or a logout/login." >&2
    fi
    if qlmanage -t -s 128 -o "$probe" "${probe}/probe.blad" >/dev/null 2>&1 \
        && ls "${probe}"/*.png >/dev/null 2>&1; then
        echo "  preview rendered: yes"
    else
        echo "  preview rendered: NO — the archive may have no embedded thumbnail" >&2
    fi
else
    echo "  pass an existing .blad file to verify:  $0 path/to/archive.blad"
fi

echo
echo "Finder may cache old icons; open a new window or run:"
echo "  qlmanage -r cache"
