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
trilingual (Chinese, English, French) installation background and an
optional, source-trust-gated Gatekeeper troubleshooting tip.
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
for item in Cargo.toml Cargo.lock LICENSE assets/overink.svg assets/overink.png assets/fonts/OFL.txt; do
  [[ -f $item ]] || { printf 'Missing file: %s\n' "$item" >&2; exit 1; }
done
for cmd in rustup cargo rustc cmake xcrun sips iconutil plutil codesign lipo ditto hdiutil tiffutil python3; do
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
from functools import lru_cache
from PIL import Image, ImageDraw, ImageFont, ImageFilter
import sys
import xml.etree.ElementTree as ET

out = Path(sys.argv[1])
version = sys.argv[2]
icon_file = Path(sys.argv[3])
svg_file = Path(sys.argv[4])
W, H = 900, 640

# Read the artwork palette from the actual assets/overink.svg.
svg = ET.parse(svg_file).getroot()
colors = {v.lower() for el in svg.iter() for key, v in el.attrib.items()
          if key in ('fill', 'stroke') and v.startswith('#')}
for value in ('#132940', '#58d6ed', '#ffb65e'):
    if value not in colors:
        raise SystemExit(f'Unexpected OverInk SVG palette: missing {value}')
navy, aqua, amber = '#132940', '#58d6ed', '#ffb65e'

@lru_cache(maxsize=None)
def font(size, role='normal'):
    if role == 'mono':
        paths = ['/System/Library/Fonts/Menlo.ttc',
                 '/System/Library/Fonts/Monaco.ttf',
                 '/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf']
    else:
        paths = ['/System/Library/Fonts/PingFang.ttc',
                 '/System/Library/Fonts/PingFang SC.ttc',
                 '/System/Library/Fonts/STHeiti Medium.ttc',
                 '/System/Library/Fonts/Supplemental/Arial Unicode.ttf',
                 '/System/Library/Fonts/Supplemental/Arial.ttf',
                 '/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc']
    for p in paths:
        if Path(p).is_file():
            try:
                return ImageFont.truetype(p, size, index=1 if role == 'bold' and p.endswith('.ttc') else 0)
            except (OSError, IndexError):
                try:
                    return ImageFont.truetype(p, size)
                except OSError:
                    continue
    raise RuntimeError('No macOS font available for Chinese, English and French')


def render(scale):
    S = scale
    image = Image.new('RGB', (W*S, H*S), '#f7fafc')
    d = ImageDraw.Draw(image)
    def xy(box): return tuple(round(v*S) for v in box)
    def rounded(box, radius, fill, outline=None, stroke=1):
        d.rounded_rectangle(xy(box), radius=round(radius*S), fill=fill,
                            outline=outline, width=round(stroke*S))
    def label(x, y, value, size, color, weight='normal', anchor=None):
        f = font(round(size*S), weight)
        px, py = round(x*S), round(y*S)
        if anchor == 'center':
            width = d.textbbox((0,0),value,font=f)[2]
            px -= width//2
        d.text((px,py), value, font=f, fill=color, stroke_width=0)
    def line(points, fill, width):
        d.line(xy(points), fill=fill, width=round(width*S), joint='curve')

    # Brand header and negative space
    d.rectangle(xy((0,0,W,126)), fill=navy)
    d.ellipse(xy((718,-165,1080,210)),fill='#18394d')
    d.ellipse(xy((815,-112,1140,172)),fill='#1c4559')
    d.rectangle(xy((0,126,W,H)), fill='#f7fafc')
    d.rectangle(xy((0,122,W,126)), fill=aqua)
    logo = Image.open(icon_file).convert('RGBA').resize((76*S,76*S), Image.Resampling.LANCZOS)
    image.paste(logo, (49*S,25*S), logo)
    label(143,25,'OverInk',37,'#ffffff','bold')
    label(145,82,'DRAW  ·  PRESENT  ·  EXPRESS',11,'#a7c6d1')
    rounded((751,42,853,80),19,'#25465a', '#466777')
    label(802,50,'v'+version,15,'#ffffff','bold','center')

    # Trilingual title, each language gets its own baseline
    rounded((42,152,48,240),3,aqua)
    label(67,145,'将 OverInk 拖入「应用程序」文件夹',27,navy,'bold')
    label(67,185,'Drag OverInk to the Applications folder',18,'#36576a')
    label(67,214,'Glissez OverInk dans le dossier Applications',18,'#36576a')

    # Two live Finder icons; all decoration stays outside their hit targets
    rounded((40,267,860,490),23,'#eaf2f6','#d5e6ed')
    rounded((82,293,337,468),19,'#ffffff','#d5e6ed')
    rounded((563,293,818,468),19,'#ffffff','#d5e6ed')
    rounded((101,283,140,319),13,navy)
    label(120.5,290,'01',15,'#ffffff','bold','center')
    rounded((582,283,621,319),13,'#237988')
    label(601.5,290,'02',15,'#ffffff','bold','center')
    # Finder positions real OverInk.app / Applications icons at (210,385) and (690,385).
    # Background deliberately does not draw fake icons or labels.
    line((392,376,501,376), '#2b98a8', 7)
    d.polygon([xy((491,361)),xy((516,376)),xy((491,391))],fill='#2b98a8')
    label(454,418,'拖动   ·   Drag   ·   Glisser',14,'#375967','bold','center')

    # Troubleshooting: non-destructive reminder, no auto-run
    rounded((40,508,860,628),19,'#ffffff','#d9e6ed')
    rounded((58,525,64,610),3,amber)
    label(80,519,'无法打开？  ·  Can’t open it?  ·  Impossible de l’ouvrir ?',16,navy,'bold')
    label(80,549,'请先确认安装包来源可信，再考虑使用以下命令。',12,'#607584')
    label(80,571,'Only if you trust the source.',12,'#607584')
    label(80,593,'Seulement si la source est fiable.',12,'#607584')
    rounded((485,557,838,599),11,'#e8f3f6','#d2e5e9')
    label(661.5,569,'xattr -cr /Applications/OverInk.app',12,'#1b485c','mono','center')

    return image

out.parent.mkdir(parents=True, exist_ok=True)
render(1).save(out, optimize=True)
render(2).save(out.with_name(out.stem+'@2x'+out.suffix), optimize=True)
print(f'Branded EN/ZH/FR DMG backgrounds: {out}')
PY

cat > "$work_root/dmg-settings.py" <<'PY'
import os

files = [os.environ['OVERINK_DMG_APP']]
symlinks = {'Applications': '/Applications'}
background = os.environ['OVERINK_DMG_BACKGROUND']
icon = os.environ['OVERINK_DMG_ICON']
format = 'UDZO'
compression_level = 9
window_rect = ((160, 110), (900, 640))
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
icon_locations = {'OverInk.app': (210, 374), 'Applications': (690, 374)}
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
  OVERINK_BUILD_VERSION="$version" cargo build --locked --release --target "$target"

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

  "$work_root/venv/bin/python" "$work_root/draw-background.py" "$work/background.png" "$version" "$root/assets/overink.png" "$root/assets/overink.svg"
  sips -s dpiWidth 72 -s dpiHeight 72 "$work/background.png" >/dev/null
  sips -s dpiWidth 144 -s dpiHeight 144 "$work/background@2x.png" >/dev/null
  tiffutil -cathidpicheck "$work/background.png" "$work/background@2x.png" -out "$work/background.tiff"
  dmg="$output/OverInk-$version-macos-$name.dmg"
  rm -f -- "$dmg"
  export OVERINK_DMG_APP="$app"
  export OVERINK_DMG_BACKGROUND="$work/background.tiff"
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
