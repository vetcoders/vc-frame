#!/bin/zsh
# package-vibecrafted-app — assembles Vibecrafted.app: alacritty as the raw
# surface, vc-frame as the native runtime, identity baked into the bundle.
# The bundle carries its own alacritty.toml fallback, so a fresh machine gets
# the full brand without any ~/.config guessing; user config wins when present.
#
# Usage: package-vibecrafted-app.zsh [--install]
#   (no flag)  build into build/Vibecrafted.app and stop
#   --install  atomically swap /Applications/Vibecrafted.app (backup kept)
#
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI

set -eu
REPO="${0:A:h:h}"
BUILD="$REPO/build/Vibecrafted.app"
APP_VERSION="1.3.0"

VCFRAME_BIN="$REPO/target/release/vc-frame"
# alacritty: prefer a sibling fork build, fall back to the installed bundle
ALACRITTY_BIN=""
for cand in \
  "$REPO/../../alacritty/alacritty/target/release/alacritty" \
  "/Applications/Vibecrafted.app/Contents/MacOS/alacritty"
do
  [[ -x "$cand" ]] && { ALACRITTY_BIN="${cand:A}"; break }
done
ICNS="/Applications/Vibecrafted.app/Contents/Resources/alacritty.icns"

[[ -x "$VCFRAME_BIN" ]]   || { print "brak $VCFRAME_BIN — najpierw: cargo build --release"; exit 1 }
[[ -n "$ALACRITTY_BIN" ]] || { print "nie znalazłem binarki alacritty (fork ani bundle)"; exit 1 }

print "surface : $ALACRITTY_BIN ($($ALACRITTY_BIN --version))"
print "runtime : $VCFRAME_BIN ($($VCFRAME_BIN --version))"

rm -rf "$BUILD"
mkdir -p "$BUILD/Contents/MacOS" "$BUILD/Contents/Resources"

cp "$ALACRITTY_BIN" "$BUILD/Contents/MacOS/alacritty"
cp "$VCFRAME_BIN"   "$BUILD/Contents/MacOS/vc-frame"
[[ -f "$ICNS" ]] && cp "$ICNS" "$BUILD/Contents/Resources/vibecrafted.icns"

# ── baked identity: alacritty fallback config (fresh machine = full brand).
# Live user config (~/.config/alacritty/alacritty.toml) still wins at launch.
cat > "$BUILD/Contents/Resources/alacritty.toml" <<'EOF'
# 𝓥𝓲𝓫𝓮𝓬𝓻𝓪𝓯𝓽𝓮𝓭. — baked bundle identity (fallback when no user config)

[window]
title = "𝓥𝓲𝓫𝓮𝓬𝓻𝓪𝓯𝓽𝓮𝓭."
decorations = "Transparent"
startup_mode = "Maximized"
padding = { x = 4, y = 28 }
dynamic_padding = true
option_as_alt = "Both"

[font]
size = 13.0

[font.normal]
family = "Monaco"

[mouse]
hide_when_typing = true

# VetCoders copper palette
[colors.primary]
background = "#1a1a2e"
foreground = "#e0def4"

[colors.cursor]
cursor = "#d4a574"
text = "#1a1a2e"

[colors.normal]
black   = "#26233a"
red     = "#eb6f92"
green   = "#a6da95"
yellow  = "#f6c177"
blue    = "#7dc4e4"
magenta = "#c4a7e7"
cyan    = "#9ccfd8"
white   = "#e0def4"

[colors.bright]
black   = "#6e6a86"
red     = "#eb6f92"
green   = "#a6da95"
yellow  = "#f6c177"
blue    = "#7dc4e4"
magenta = "#c4a7e7"
cyan    = "#9ccfd8"
white   = "#e0def4"
EOF

# ── launcher: no guessing. Surface and runtime ship in the bundle;
# every launch lands in THE console session (attach --create).
cat > "$BUILD/Contents/MacOS/vibecrafted-launch" <<'EOF'
#!/bin/zsh -l
# 𝓥𝓲𝓫𝓮𝓬𝓻𝓪𝓯𝓽𝓮𝓭. — surface: alacritty · runtime: vc-frame
DIR="${0:A:h}"
RES="${DIR:h}/Resources"

CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/alacritty/alacritty.toml"
[[ -f "$CONFIG" ]] || CONFIG="$RES/alacritty.toml"

exec "$DIR/alacritty" --config-file "$CONFIG" \
  -e "$DIR/vc-frame" attach --create "vibecrafted console"
EOF
chmod +x "$BUILD/Contents/MacOS/"*

cat > "$BUILD/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>vibecrafted-launch</string>
	<key>CFBundleIconFile</key>
	<string>vibecrafted.icns</string>
	<key>CFBundleIdentifier</key>
	<string>space.div0.vibecrafted</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>𝓥𝓲𝓫𝓮𝓬𝓻𝓪𝓯𝓽𝓮𝓭.</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$APP_VERSION</string>
	<key>CFBundleVersion</key>
	<string>1</string>
	<key>LSMinimumSystemVersion</key>
	<string>14.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
EOF

codesign --force --deep --sign - "$BUILD" 2>/dev/null
print "zbudowane: $BUILD"

if [[ "${1:-}" == "--install" ]]; then
  BAKDIR="$HOME/.vibecrafted/backups"
  mkdir -p "$BAKDIR"
  if [[ -d /Applications/Vibecrafted.app ]]; then
    BAK="$BAKDIR/Vibecrafted.app-$(date +%Y%m%d_%H%M%S)"
    mv /Applications/Vibecrafted.app "$BAK"
    print "backup: $BAK"
  fi
  cp -R "$BUILD" /Applications/Vibecrafted.app
  print "zainstalowane: /Applications/Vibecrafted.app ($APP_VERSION)"
fi
