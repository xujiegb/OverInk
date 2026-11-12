#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'HELP'
Usage: ./package-macos.sh [--arch x64|arm64|all] [--version X.Y.Z] [--output DIR]

Environment:
  RUST_VERSION               Rust toolchain version (default: 1.99.1)
  MACOSX_DEPLOYMENT_TARGET   Minimum macOS version (default: 11.0)
  CODESIGN_IDENTITY          Developer ID Application identity (default: -)
  NOTARYTOOL_PROFILE         Optional notarytool keychain profile

Builds native OverInk.app and a branded drag-to-Applications DMG, with a
bilingual installation background and optional Gatekeeper troubleshooting tip.
Requires macOS, Xcode Command Line Tools and Python >= 3.10; Python build
packages (dmgbuild and Pillow) are installed in an isolated temporary venv.
The repository needs only this script, not an external DMG theme or icon file.
HELP
}

arch=all
version=
output=dist
while (($#)); do
  case "$1" in
    -a|--arch) (($# >= 2)) || { usage >&2; exit 2; }; arch=$2; shift 2 ;;
    -v|--version) (($# >= 2)) || { usage >&2; exit 2; }; version=${2#v}; shift 2 ;;
    -o|--output) (($# >= 2)) || { usage >&2; exit 2; }; output=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done
case "$arch" in x64|arm64|all) ;; *) echo 'Architecture must be x64, arm64 or all.' >&2; exit 2 ;; esac
[[ $(uname -s) == Darwin ]] || { echo 'Run this script on macOS.' >&2; exit 1; }

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
cd "$root"
for item in Cargo.toml Cargo.lock LICENSE assets/overink.png assets/fonts/OFL.txt; do
  [[ -f $item ]] || { printf 'Missing file: %s\n' "$item" >&2; exit 1; }
done
for cmd in rustup cargo rustc cmake xcrun sips iconutil plutil codesign lipo ditto hdiutil python3; do
  command -v "$cmd" >/dev/null 2>&1 || { printf 'Missing tool: %s\n' "$cmd" >&2; exit 1; }
done
python3 -c 'import sys; assert sys.version_info >= (3, 10), "Python 3.10 or later is required"'

if [[ -z $version ]]; then
  version=$(sed -nE 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' Cargo.toml | head -n1)
fi
[[ $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || {
  printf 'Invalid release version: %s\n' "$version" >&2; exit 1;
}

rust_version=${RUST_VERSION:-1.99.1}
export RUSTUP_TOOLCHAIN="$rust_version"
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
identity=${CODESIGN_IDENTITY:--}
notary_profile=${NOTARYTOOL_PROFILE:-}
[[ -z $notary_profile || $identity != - ]] || {
  echo 'Notarization requires a Developer ID Application certificate.' >&2; exit 1;
}

sdk=$(xcrun --sdk macosx --show-sdk-path)
clang=$(xcrun --find clang)
clangxx=$(xcrun --find clang++)
runtime_dir="$(xcrun clang -print-resource-dir)/lib/darwin"
[[ -f $runtime_dir/libclang_rt.osx.a ]] || { echo 'Missing libclang_rt.osx.a.' >&2; exit 1; }

mkdir -p "$output"
output=$(cd "$output" && pwd -P)
work_root=$(mktemp -d "${TMPDIR:-/tmp}/overink-dmg.XXXXXXXX")
trap 'rm -rf -- "$work_root"' EXIT

rustup toolchain install "$rust_version" --profile minimal --component rustfmt --component clippy

# DMG layouts are written directly into .DS_Store. Finder/AppleScript and
# create-dmg --skip-jenkins are not used, so CI builds retain the layout.
python3 -m venv "$work_root/venv"
"$work_root/venv/bin/python" -m pip install --disable-pip-version-check --quiet \
  'dmgbuild==1.6.7' 'Pillow>=11,<13'

cat > "$work_root/draw-background.py" <<'PY'
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont
import sys

out = Path(sys.argv[1])
version = sys.argv[2]
W, H = 800, 500


def font_paths():
    candidates = [
        '/System/Library/Fonts/PingFang.ttc',
        '/System/Library/Fonts/Supplemental/Arial Unicode.ttf',
        '/System/Library/Fonts/STHeiti Medium.ttc',
    ]
    for folder in ('/System/Library/Fonts', '/Library/Fonts'):
        root = Path(folder)
        if root.exists():
            candidates.extend(str(p) for p in root.rglob('*')
                              if p.suffix.lower() in ('.ttc', '.otf', '.ttf')
                              and any(x in p.name.lower() for x in
                                      ('pingfang', 'heiti', 'arial unicode', 'notosanscjk')))
    return list(dict.fromkeys(candidates))


def load_font(size, bold=False, mono=False):
    if mono:
        paths = ['/System/Library/Fonts/Menlo.ttc',
                 '/System/Library/Fonts/Monaco.ttf']
    else:
        paths = font_paths()
    for path in paths:
        if Path(path).is_file():
            try:
                return ImageFont.truetype(path, size, index=1 if bold and path.endswith('.ttc') else 0)
            except (OSError, IndexError):
                try:
                    return ImageFont.truetype(path, size)
                except OSError:
                    pass
    raise RuntimeError('A macOS system font is required to render the DMG background')


def render(scale):
    S = scale
    im = Image.new('RGB', (W*S, H*S), '#f7f8fd')
    d = ImageDraw.Draw(im)
    def xy(a):
        return tuple(round(v*S) for v in a)
    def rect(bbox, fill, radius=0, outline=None, width=1):
        d.rounded_rectangle(xy(bbox), radius=round(radius*S), fill=fill,
                            outline=outline, width=round(width*S))
    def text(x, y, value, size, color, bold=False, mono=False, anchor=None):
        d.text((round(x*S), round(y*S)), value, font=load_font(round(size*S), bold, mono),
               fill=color, anchor=anchor)

    # A calm, light desktop-friendly surface using OverInk's default ink palette.
    d.ellipse(xy((650, -140, 1000, 210)), fill='#f0f0ff')
    d.ellipse(xy((-150, 345, 140, 645)), fill='#eff7f1')
    rect((0, 0, 800, 12), '#636896')
    rect((42, 37, 88, 83), '#636896', radius=13)
    d.line(xy((54, 66, 65, 56, 78, 65)), fill='#ffffff', width=round(5*S), joint='curve')
    text(102, 33, 'OverInk', 34, '#252842', bold=True)
    text(103, 73, 'DRAW · PRESENT · EXPRESS', 10, '#69708b', bold=True)
    rect((670, 46, 750, 78), '#ececf8', radius=16)
    text(710, 62, f'v{version}', 12, '#636896', bold=True, anchor='mm')

    text(400, 114, '将 OverInk 拖入「应用程序」文件夹', 27, '#272b45', bold=True, anchor='mt')
    text(400, 155, 'Drag OverInk into Applications to install', 15, '#69708b', anchor='mt')

    rect((42, 194, 758, 374), '#edf0fa', 24, '#dfe3f2')
    rect((100, 218, 286, 357), '#ffffff', 20, '#e1e5f0')
    rect((516, 218, 702, 357), '#ffffff', 20, '#e1e5f0')
    # The two white plates are where Finder will place the REAL App and folder icons.
    # No fake icons: the drag-and-drop works in Finder.
    rect((116, 207, 147, 234), '#636896', 9)
    text(131, 220, '1', 14, '#ffffff', bold=True, anchor='mm')
    rect((532, 207, 563, 234), '#3c775e', 9)
    text(547, 220, '2', 14, '#ffffff', bold=True, anchor='mm')
    d.line(xy((345, 287, 446, 287)), fill='#636896', width=round(6*S), joint='curve')
    d.polygon([xy((444, 274)), xy((465, 287)), xy((444, 300))], fill='#636896')
    text(402, 315, '拖动安装  ·  DRAG TO INSTALL', 11, '#636896', bold=True, anchor='mt')

    rect((42, 394, 758, 478), '#ffffff', 17, '#e0e4f0')
    rect((55, 407, 60, 465), '#ecad45', 3)
    text(73, 407, 'macOS 提示应用“已损坏”或无法打开？', 16, '#30344e', bold=True)
    text(73, 440, '请先确认下载来源可信，再考虑在「终端」执行：', 12, '#727992')
    rect((419, 431, 743, 464), '#f1f2f9', 9)
    text(581, 448, 'xattr -cr /Applications/OverInk.app', 12, '#505779', mono=True, anchor='mm')
    return im

out.parent.mkdir(parents=True, exist_ok=True)
render(1).save(out, optimize=True)
render(2).save(out.with_name(out.stem + '@2x' + out.suffix), optimize=True)
print(f'Rendered branded 1x/2x Finder backgrounds: {out}')
PY

cat > "$work_root/dmg-settings.py" <<'PY'
import os

files = [os.environ['OVERINK_DMG_APP']]
symlinks = {'Applications': '/Applications'}
background = os.environ['OVERINK_DMG_BACKGROUND']
icon = os.environ['OVERINK_DMG_ICON']
format = 'UDZO'
compression_level = 9
window_rect = ((160, 110), (800, 500))
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
default_view = 'icon-view'
include_icon_view_settings = True
arrange_by = None
icon_size = 110
text_size = 13
label_pos = 'bottom'
show_icon_preview = False
icon_locations = {'OverInk.app': (194, 278), 'Applications': (608, 278)}
PY

build_one() {
  local name=$1 target=$2 macho_arch=$3
  local work toolchain key upper flags_key saved_flags binary app contents dmg size scale pixels suffix
  work="$work_root/$name"
  mkdir -p "$work" "$work/OverInk.iconset"

  rustup target add --toolchain "$rust_version" "$target"
  toolchain="$work/macos-toolchain.cmake"
  cat > "$toolchain" <<CMAKE
set(CMAKE_SYSTEM_NAME Darwin)
set(CMAKE_OSX_ARCHITECTURES "$macho_arch" CACHE STRING "" FORCE)
set(CMAKE_OSX_DEPLOYMENT_TARGET "$MACOSX_DEPLOYMENT_TARGET" CACHE STRING "" FORCE)
set(CMAKE_OSX_SYSROOT "$sdk" CACHE PATH "" FORCE)
set(CMAKE_C_COMPILER "$clang" CACHE FILEPATH "" FORCE)
set(CMAKE_CXX_COMPILER "$clangxx" CACHE FILEPATH "" FORCE)
CMAKE

  key=$(printf '%s' "$target" | tr '-' '_')
  upper=$(printf '%s' "$key" | tr '[:lower:]' '[:upper:]')
  flags_key="CARGO_TARGET_${upper}_RUSTFLAGS"
  saved_flags=${!flags_key-}
  printf -v "$flags_key" '%s' "${saved_flags:+$saved_flags }-Lnative=$runtime_dir -lstatic=clang_rt.osx"
  export "$flags_key"
  export CMAKE_TOOLCHAIN_FILE="$toolchain"
  export CARGO_TARGET_DIR="$root/target"

  printf '\nBuilding OverInk %s for %s\n' "$version" "$target"
  cargo fmt --all -- --check
  cargo clippy --locked --target "$target" --all-targets -- -D warnings
  if [[ $(rustc -vV | sed -n 's/^host: //p') == "$target" ]]; then
    cargo test --locked --release --target "$target"
  fi
  cargo build --locked --release --target "$target"

  binary="$CARGO_TARGET_DIR/$target/release/overink"
  [[ -s $binary ]] || { printf 'Missing binary: %s\n' "$binary" >&2; return 1; }
  [[ $(lipo -archs "$binary") == "$macho_arch" ]] || {
    printf 'Wrong Mach-O architecture: %s\n' "$(lipo -archs "$binary")" >&2; return 1;
  }

  app="$work/staging/OverInk.app"
  contents="$app/Contents"
  mkdir -p "$contents/MacOS" "$contents/Resources"
  ditto "$binary" "$contents/MacOS/overink"
  chmod 755 "$contents/MacOS/overink"
  ditto LICENSE "$contents/Resources/LICENSE"
  ditto assets/fonts/OFL.txt "$contents/Resources/OFL.txt"

  for size in 16 32 128 256 512; do
    for scale in 1 2; do
      pixels=$((size * scale))
      suffix=
      ((scale == 1)) || suffix=@2x
      sips -s format png -z "$pixels" "$pixels" assets/overink.png \
        --out "$work/OverInk.iconset/icon_${size}x${size}${suffix}.png" >/dev/null
    done
  done
  iconutil -c icns "$work/OverInk.iconset" -o "$contents/Resources/OverInk.icns"

  cat > "$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleLocalizations</key>
  <array><string>en</string><string>zh-Hans</string><string>zh-Hant</string><string>fr</string></array>
  <key>CFBundleDisplayName</key><string>OverInk</string>
  <key>CFBundleExecutable</key><string>overink</string>
  <key>CFBundleIdentifier</key><string>org.overink.OverInk</string>
  <key>CFBundleName</key><string>OverInk</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleIconFile</key><string>OverInk.icns</string>
  <key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
  plutil -lint "$contents/Info.plist"
  if [[ $identity == - ]]; then
    codesign --force --sign - "$app"
  else
    codesign --force --options runtime --timestamp --sign "$identity" "$app"
  fi
  codesign --verify --strict --verbose=2 "$app"

  "$work_root/venv/bin/python" "$work_root/draw-background.py" "$work/background.png" "$version"
  dmg="$output/OverInk-$version-macos-$name.dmg"
  rm -f -- "$dmg"
  export OVERINK_DMG_APP="$app"
  export OVERINK_DMG_BACKGROUND="$work/background.png"
  export OVERINK_DMG_ICON="$contents/Resources/OverInk.icns"
  "$work_root/venv/bin/dmgbuild" -s "$work_root/dmg-settings.py" "OverInk $version" "$dmg"
  unset OVERINK_DMG_APP OVERINK_DMG_BACKGROUND OVERINK_DMG_ICON

  if [[ $identity != - ]]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
  fi
  if [[ -n $notary_profile ]]; then
    xcrun notarytool submit "$dmg" --keychain-profile "$notary_profile" --wait
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
  fi
  hdiutil verify "$dmg" >/dev/null
  [[ -s $dmg ]] || { printf 'Missing disk image: %s\n' "$dmg" >&2; return 1; }
  printf 'Created styled DMG: %s\n' "$dmg"

  if [[ -n $saved_flags ]]; then
    printf -v "$flags_key" '%s' "$saved_flags"
    export "$flags_key"
  else
    unset "$flags_key"
  fi
  unset CMAKE_TOOLCHAIN_FILE
}

case "$arch" in
  x64) build_one x64 x86_64-apple-darwin x86_64 ;;
  arm64) build_one arm64 aarch64-apple-darwin arm64 ;;
  all)
    build_one x64 x86_64-apple-darwin x86_64
    build_one arm64 aarch64-apple-darwin arm64
    ;;
esac
