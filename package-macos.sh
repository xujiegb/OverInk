#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'HELP'
Usage: ./package-macos.sh [--arch x64|arm64|all] [--version X.Y.Z] [--output DIR]

Environment:
  RUST_VERSION         Rust toolchain version (default: 1.99.1)
  MACOSX_DEPLOYMENT_TARGET  Minimum macOS version (default: 11.0)
  CODESIGN_IDENTITY    Developer ID Application identity (default: - for ad-hoc signing)
  NOTARYTOOL_PROFILE   Optional keychain profile for notarization (requires Developer ID)

Outputs:
  dist/OverInk-VERSION-macos-x64.dmg
  dist/OverInk-VERSION-macos-arm64.dmg
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

case "$arch" in x64|arm64|all) ;; *) echo 'Architecture must be x64, arm64, or all.' >&2; exit 2 ;; esac
[[ $(uname -s) == Darwin ]] || { echo 'Run this script on macOS.' >&2; exit 1; }

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
cd "$root"
for item in Cargo.toml Cargo.lock LICENSE assets/overink.png assets/fonts/OFL.txt; do
  [[ -f $item ]] || { printf 'Missing file: %s\n' "$item" >&2; exit 1; }
done
for cmd in rustup cargo rustc cmake xcrun sips iconutil plutil codesign lipo ditto hdiutil; do
  command -v "$cmd" >/dev/null 2>&1 || { printf 'Missing tool: %s\n' "$cmd" >&2; exit 1; }
done

if [[ -z $version ]]; then
  version=$(sed -nE 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' Cargo.toml | sed -n '1p')
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
  echo 'Notarization requires CODESIGN_IDENTITY (Developer ID Application).' >&2; exit 1;
}

sdk=$(xcrun --sdk macosx --show-sdk-path)
clang=$(xcrun --find clang)
clangxx=$(xcrun --find clang++)
runtime_dir="$(xcrun clang -print-resource-dir)/lib/darwin"
[[ -f "$runtime_dir/libclang_rt.osx.a" ]] || {
  echo 'Missing libclang_rt.osx.a in the Xcode toolchain.' >&2; exit 1;
}

rustup toolchain install "$rust_version" --profile minimal --component rustfmt --component clippy
current_work=
trap '[[ -z ${current_work:-} ]] || rm -rf -- "$current_work"' EXIT
mkdir -p "$output"
output=$(cd "$output" && pwd -P)

build_one() {
  local name=$1 target=$2 macho_arch=$3
  local work binary app contents toolchain rustflags_var saved_rustflags dmg
  work=$(mktemp -d "${TMPDIR:-/tmp}/overink-macos.XXXXXXXX")
  current_work=$work

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

  rustflags_var="CARGO_TARGET_$(printf '%s' "$target" | tr '[:lower:]-' '[:upper:]_')_RUSTFLAGS"
  saved_rustflags=${!rustflags_var-}
  printf -v "$rustflags_var" '%s' "${saved_rustflags:+$saved_rustflags }-Lnative=$runtime_dir -lstatic=clang_rt.osx"
  export "$rustflags_var"
  export CMAKE_TOOLCHAIN_FILE="$toolchain"
  export CARGO_TARGET_DIR="$root/target"

  echo "Building OverInk $version for $target"
  cargo fmt --all -- --check
  cargo clippy --locked --target "$target" --all-targets -- -D warnings
  if [[ $(rustc -vV | sed -n 's/^host: //p') == "$target" ]]; then
    cargo test --locked --release --target "$target"
  fi
  cargo build --locked --release --target "$target"

  binary="$CARGO_TARGET_DIR/$target/release/overink"
  [[ -s $binary ]] || { printf 'Missing binary: %s\n' "$binary" >&2; exit 1; }
  [[ $(lipo -archs "$binary") == "$macho_arch" ]] || {
    printf 'Unexpected binary architecture: %s\n' "$(lipo -archs "$binary")" >&2; exit 1;
  }

  app="$work/staging/OverInk.app"
  contents="$app/Contents"
  mkdir -p "$contents/MacOS" "$contents/Resources" "$work/OverInk.iconset"
  ditto "$binary" "$contents/MacOS/overink"
  chmod 755 "$contents/MacOS/overink"
  ditto LICENSE "$contents/Resources/LICENSE"
  ditto assets/fonts/OFL.txt "$contents/Resources/OFL.txt"

  local size scale pixels suffix
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
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleLocalizations</key>
  <array><string>en</string><string>zh-Hans</string><string>zh-Hant</string></array>
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
</dict>
</plist>
PLIST
  plutil -lint "$contents/Info.plist"

  if [[ $identity == - ]]; then
    codesign --force --sign - "$app"
  else
    codesign --force --options runtime --timestamp --sign "$identity" "$app"
  fi
  codesign --verify --strict --verbose=2 "$app"

  dmg="$output/OverInk-$version-macos-$name.dmg"
  rm -f -- "$dmg"
  if command -v create-dmg >/dev/null 2>&1; then
    local -a options=()
    [[ -z ${CI:-} ]] || options+=(--skip-jenkins)
    create-dmg \
      --volname "OverInk $version" \
      --window-size 700 420 \
      --icon-size 100 \
      --icon OverInk.app 160 185 \
      --hide-extension OverInk.app \
      --app-drop-link 500 185 \
      "${options[@]}" \
      "$dmg" "$work/staging"
  else
    ln -s /Applications "$work/staging/Applications"
    hdiutil create -quiet -volname "OverInk $version" -srcfolder "$work/staging" \
      -format UDZO -ov "$dmg"
  fi

  if [[ $identity != - ]]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
  fi
  if [[ -n $notary_profile ]]; then
    xcrun notarytool submit "$dmg" --keychain-profile "$notary_profile" --wait
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
  fi

  hdiutil verify "$dmg" >/dev/null
  [[ -s $dmg ]] || { printf 'Missing disk image: %s\n' "$dmg" >&2; exit 1; }
  echo "Created: $dmg"
  if [[ -n $saved_rustflags ]]; then
    printf -v "$rustflags_var" %s "$saved_rustflags"
    export "$rustflags_var"
  else
    unset "$rustflags_var"
  fi
  unset CMAKE_TOOLCHAIN_FILE
  rm -rf -- "$work"
  current_work=
}

case "$arch" in
  x64) build_one x64 x86_64-apple-darwin x86_64 ;;
  arm64) build_one arm64 aarch64-apple-darwin arm64 ;;
  all)
    build_one x64 x86_64-apple-darwin x86_64
    build_one arm64 aarch64-apple-darwin arm64
    ;;
esac
