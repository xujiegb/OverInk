#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
VERSION="${OVERINK_VERSION:-}"
ARCH="all"
OUTPUT="${OVERINK_OUTPUT_DIR:-$ROOT/dist/packages}"
SYSROOTS="${OVERINK_SYSROOTS:-$ROOT/sysroots}"
SYSROOT=""
RUST_VERSION="${RUST_VERSION:-1.99.1}"
CLANG="${OVERINK_CLANG:-clang}"
CLANGXX="${OVERINK_CLANGXX:-clang++}"
BUILD_DIR="${OVERINK_TARGET_DIR:-$ROOT/target}"
JOBS="${CARGO_BUILD_JOBS:-2}"
MAX_GLIBC="${OVERINK_MAX_GLIBC:-}"
INSTALL_DEPS=0
WORK=""
HOST=""

die() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "Command not found: $1"
}

cleanup() {
  [[ -z "$WORK" ]] || rm -rf -- "$WORK"
}

parse_args() {
  while (($#)); do
    case "$1" in
      --version|--arch|--output|--sysroots|--sysroot|--toolchain|--jobs|--max-glibc)
        (($# >= 2)) || die "Missing value for $1"
        case "$1" in
          --version) VERSION="${2#v}" ;;
          --arch) ARCH="$2" ;;
          --output) OUTPUT="$2" ;;
          --sysroots) SYSROOTS="$2" ;;
          --sysroot) SYSROOT="$2" ;;
          --toolchain) RUST_VERSION="$2" ;;
          --jobs) JOBS="$2" ;;
          --max-glibc) MAX_GLIBC="$2" ;;
        esac
        shift 2
        ;;
      --install-deps) INSTALL_DEPS=1; shift ;;
      -h|--help)
        printf 'Usage: %s [version] [--arch native|all|x64|i386|arm64|ppc64le|loong64|riscv64|s390x] [--sysroots DIR] [--sysroot DIR] [--output DIR] [--toolchain VERSION] [--jobs N] [--max-glibc VERSION] [--install-deps]\n' "${0##*/}"
        exit 0
        ;;
      *)
        [[ -z "$VERSION" && "$1" != -* ]] || die "Unknown argument: $1"
        VERSION="${1#v}"
        shift
        ;;
    esac
  done
  [[ "$JOBS" =~ ^[1-9][0-9]*$ ]] || die 'Invalid build job count'
  [[ -z "$MAX_GLIBC" || "$MAX_GLIBC" =~ ^[0-9]+\.[0-9]+$ ]] || die 'Invalid maximum GLIBC version'
  [[ "$RUST_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die 'Invalid Rust toolchain version'
  [[ -f "$ROOT/Cargo.toml" && -f "$ROOT/Cargo.lock" ]] || die 'Cargo.toml and Cargo.lock must be in the script directory'
  if [[ -z "$VERSION" ]]; then
    VERSION="$(awk '
      /^\[package\][[:space:]]*$/ { p=1; next }
      /^\[/ { p=0 }
      p && /^[[:space:]]*version[[:space:]]*=/ {
        sub(/^[^=]*=[[:space:]]*"/, ""); sub(/".*/, ""); print; exit
      }
    ' "$ROOT/Cargo.toml")"
  fi
  [[ "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || die 'Version must be MAJOR.MINOR.PATCH'
}

target_triple() {
  case "$1" in
    x64) printf '%s' x86_64-unknown-linux-gnu ;;
    i386) printf '%s' i686-unknown-linux-gnu ;;
    arm64) printf '%s' aarch64-unknown-linux-gnu ;;
    ppc64le) printf '%s' powerpc64le-unknown-linux-gnu ;;
    loong64) printf '%s' loongarch64-unknown-linux-gnu ;;
    riscv64) printf '%s' riscv64gc-unknown-linux-gnu ;;
    s390x) printf '%s' s390x-unknown-linux-gnu ;;
  esac
}

linker_triple() {
  case "$1" in
    x64) printf '%s' x86_64-linux-gnu ;;
    i386) printf '%s' i686-linux-gnu ;;
    arm64) printf '%s' aarch64-linux-gnu ;;
    ppc64le) printf '%s' powerpc64le-linux-gnu ;;
    loong64) printf '%s' loongarch64-linux-gnu ;;
    riscv64) printf '%s' riscv64-linux-gnu ;;
    s390x) printf '%s' s390x-linux-gnu ;;
  esac
}

resolve_targets() {
  local item
  case "$ARCH" in
    all) TARGETS=(x64 i386 arm64 ppc64le loong64 riscv64 s390x) ;;
    native)
      for item in x64 i386 arm64 ppc64le loong64 riscv64 s390x; do
        if [[ "$(target_triple "$item")" == "$HOST" ]]; then
          TARGETS=("$item")
          return 0
        fi
      done
      die "Unsupported native Rust host: $HOST"
      ;;
    amd64|x86_64) TARGETS=(x64) ;;
    x86|i686) TARGETS=(i386) ;;
    aarch64) TARGETS=(arm64) ;;
    ppc64el) TARGETS=(ppc64le) ;;
    loongarch64) TARGETS=(loong64) ;;
    x64|i386|arm64|ppc64le|loong64|riscv64|s390x) TARGETS=("$ARCH") ;;
    *) die "Unsupported architecture: $ARCH" ;;
  esac
  [[ -z "$SYSROOT" || "${#TARGETS[@]}" == 1 ]] || die '--sysroot requires a single --arch target'
}

package_arch() {
  case "$1" in
    x64) printf '%s' x86_64 ;;
    i386) printf '%s' i686 ;;
    arm64) printf '%s' aarch64 ;;
    ppc64le) printf '%s' ppc64le ;;
    loong64) printf '%s' loongarch64 ;;
    riscv64) printf '%s' riscv64 ;;
    s390x) printf '%s' s390x ;;
  esac
}

install_dependencies() {
  local -a elevate=()
  ((EUID == 0)) || elevate=(sudo)
  "${elevate[@]}" dnf install -y \
    clang lld llvm cmake make ninja-build pkgconf-pkg-config wayland-devel \
    libX11-devel libXext-devel libXi-devel libXrandr-devel libXcursor-devel \
    libXfixes-devel libXrender-devel libXScrnSaver-devel libXtst-devel \
    libxkbcommon-devel mesa-libEGL-devel mesa-libGL-devel rpm-build rpm
}

check_tools() {
  local tool
  for tool in rustup cargo rustc cmake pkg-config wayland-scanner ld.lld llvm-ar llvm-ranlib readelf od sort grep install tar "$CLANG" "$CLANGXX"; do
    need "$tool"
  done
  rustup toolchain install "$RUST_VERSION" --profile minimal
  HOST="$(rustc "+$RUST_VERSION" -vV | sed -n 's/^host: //p')"
  [[ "$HOST" == *-unknown-linux-gnu ]] || die "A Linux GNU Rust host is required: $HOST"
}

select_sysroot() {
  local arch="$1" triple="$2" directory=""
  if [[ -n "$SYSROOT" ]]; then
    directory="$SYSROOT"
  elif [[ -d "$SYSROOTS/$arch" ]]; then
    directory="$SYSROOTS/$arch"
  else
    directory="$SYSROOTS/$triple"
  fi
  [[ -d "$directory/usr/include" && -d "$directory/usr/lib" ]] || die "Missing target sysroot for $arch: $directory"
  (cd -- "$directory" && pwd -P)
}

check_sysroots() {
  local arch triple directory
  for arch in "${TARGETS[@]}"; do
    triple="$(target_triple "$arch")"
    if [[ "$triple" != "$HOST" ]]; then
      directory="$(select_sysroot "$arch" "$triple")"
      [[ -d "$directory/usr/lib/gcc" ]] || die "Missing cross GCC runtime in $directory/usr/lib/gcc"
      find "$directory/usr/lib/gcc" -name crtbeginS.o -print -quit | grep -q . || die "Missing crtbeginS.o for $arch"
    fi
  done
}

write_cmake() {
  local file="$1" compiler="$2" cxx="$3" sysroot="$4" processor="$5"
  if [[ -n "$sysroot" ]]; then
    printf 'set(CMAKE_SYSTEM_NAME Linux)\nset(CMAKE_SYSTEM_PROCESSOR "%s")\nset(CMAKE_SYSROOT "%s")\n' "$processor" "$sysroot" > "$file"
    printf 'set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)\nset(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)\nset(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)\nset(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)\n' >> "$file"
  else
    : > "$file"
  fi
  printf 'set(CMAKE_C_COMPILER "%s")\nset(CMAKE_CXX_COMPILER "%s")\nset(CMAKE_AR "%s")\nset(CMAKE_RANLIB "%s")\n' "$compiler" "$cxx" "$(command -v llvm-ar)" "$(command -v llvm-ranlib)" >> "$file"
  printf 'set(SDL_X11_SHARED ON CACHE BOOL "" FORCE)\nset(SDL_WAYLAND_SHARED ON CACHE BOOL "" FORCE)\n' >> "$file"
}

build_target() (
  local arch="$1" triple="$2" work="$WORK/tool-$1" sysroot="" processor linker multi gcc_dir key upper tool
  mkdir -p "$work"
  processor="$(linker_triple "$arch")"
  key="${triple//-/_}"
  upper="${key^^}"
  export CARGO_BUILD_JOBS="$JOBS" CARGO_TARGET_DIR="$BUILD_DIR"
  export "CC_$key=$CLANG" "CXX_$key=$CLANGXX" "AR_$key=llvm-ar"
  export "CARGO_TARGET_${upper}_LINKER=$CLANG"
  export CMAKE_TOOLCHAIN_FILE="$work/toolchain.cmake"
  if [[ "$triple" != "$HOST" ]]; then
    sysroot="$(select_sysroot "$arch" "$triple")"
    gcc_dir="$(find "$sysroot/usr/lib/gcc" -name crtbeginS.o -print | sort -V | tail -n1)"
    [[ -n "$gcc_dir" ]] || die "Missing cross GCC startup files for $arch"
    gcc_dir="${gcc_dir%/*}"
    for tool in clang clang++; do
      if [[ "$tool" == clang ]]; then linker="$CLANG"; else linker="$CLANGXX"; fi
      printf '#!/usr/bin/env bash\nexec %q --target=%q --sysroot=%q --gcc-install-dir=%q -fuse-ld=lld -Qunused-arguments "$@"\n' \
        "$(command -v "$linker")" "$processor" "$sysroot" "$gcc_dir" > "$work/$tool"
      chmod 0755 "$work/$tool"
    done
    export "CC_$key=$work/clang" "CXX_$key=$work/clang++"
    export "CARGO_TARGET_${upper}_LINKER=$work/clang"
    export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR="$sysroot"
    multi="$processor"
    [[ "$arch" != i386 ]] || multi=i386-linux-gnu
    export PKG_CONFIG_LIBDIR="$sysroot/usr/lib/$multi/pkgconfig:$sysroot/usr/lib/pkgconfig:$sysroot/usr/share/pkgconfig:$sysroot/usr/lib64/pkgconfig"
    export PKG_CONFIG_PATH=""
    write_cmake "$CMAKE_TOOLCHAIN_FILE" "$work/clang" "$work/clang++" "$sysroot" "${processor%%-*}"
    printf 'Cross-compiling %s (%s)\n' "$arch" "$triple"
  else
    pkg-config --exists x11 wayland-client xkbcommon egl gl || die 'Native X11/Wayland/EGL development packages are missing; use --install-deps'
    write_cmake "$CMAKE_TOOLCHAIN_FILE" "$(command -v "$CLANG")" "$(command -v "$CLANGXX")" '' ''
    printf 'Native-compiling %s (%s)\n' "$arch" "$triple"
  fi
  rustup target add "$triple" --toolchain "$RUST_VERSION"
  cd -- "$ROOT"
  cargo "+$RUST_VERSION" build --locked --release --target "$triple" --bin overink
  [[ -s "$BUILD_DIR/$triple/release/overink" ]] || die "Cargo did not produce $triple/release/overink"
  validate_elf "$BUILD_DIR/$triple/release/overink" "$arch"
  local config
  config="$(find "$BUILD_DIR/$triple/release/build" -name SDL_build_config.h -print -quit 2>/dev/null || true)"
  [[ -n "$config" ]] || die "SDL configuration not found for $arch"
  grep -q '^#define SDL_VIDEO_DRIVER_X11 1' "$config" || die "SDL X11 backend disabled for $arch"
  grep -q '^#define SDL_VIDEO_DRIVER_WAYLAND 1' "$config" || die "SDL Wayland backend disabled for $arch"
  grep -q '^#define SDL_VIDEO_DRIVER_X11_DYNAMIC "libX11\.so' "$config" || die "SDL dynamic X11 backend missing for $arch"
  grep -q '^#define SDL_VIDEO_DRIVER_WAYLAND_DYNAMIC "libwayland-client\.so' "$config" || die "SDL dynamic Wayland backend missing for $arch"
)

validate_elf() {
  local binary="$1" arch="$2" machine class endian actual bytes=()
  read -r -a bytes <<< "$(od -An -tu1 -N20 "$binary" | tr '\n' ' ')"
  ((${#bytes[@]} == 20)) || die "Truncated ELF: $binary"
  [[ ${bytes[0]} == 127 && ${bytes[1]} == 69 && ${bytes[2]} == 76 && ${bytes[3]} == 70 ]] || die "Invalid ELF: $binary"
  class=${bytes[4]}
  endian=${bytes[5]}
  if ((endian == 1)); then
    actual=$((bytes[18] + 256 * bytes[19]))
  else
    actual=$((256 * bytes[18] + bytes[19]))
  fi
  case "$arch" in
    x64) machine=62; [[ $class == 2 && $endian == 1 ]] || die 'Invalid x64 ELF' ;;
    i386) machine=3; [[ $class == 1 && $endian == 1 ]] || die 'Invalid i386 ELF' ;;
    arm64) machine=183; [[ $class == 2 && $endian == 1 ]] || die 'Invalid arm64 ELF' ;;
    ppc64le) machine=21; [[ $class == 2 && $endian == 1 ]] || die 'Invalid ppc64le ELF' ;;
    loong64) machine=258; [[ $class == 2 && $endian == 1 ]] || die 'Invalid loong64 ELF' ;;
    riscv64) machine=243; [[ $class == 2 && $endian == 1 ]] || die 'Invalid riscv64 ELF' ;;
    s390x) machine=22; [[ $class == 2 && $endian == 2 ]] || die 'Invalid s390x ELF' ;;
  esac
  ((actual == machine)) || die "ELF architecture mismatch: $arch (e_machine=$actual)"
}

required_glibc() {
  local version
  version="$(LC_ALL=C readelf --version-info "$1" | grep -oE 'Name: GLIBC_[0-9]+(\.[0-9]+)+' | sed 's/Name: GLIBC_//' | sort -Vu | tail -n1 || true)"
  [[ -n "$version" ]] || die "Cannot determine glibc requirement for $1"
  printf '%s' "$version"
}

stage_target() {
  local arch="$1" triple="$2" stage="$3" doc
  mkdir -p "$stage/usr/bin" "$stage/usr/share/applications" "$stage/usr/share/pixmaps" "$stage/usr/share/doc/overink"
  install -m0755 "$BUILD_DIR/$triple/release/overink" "$stage/usr/bin/overink"
  validate_elf "$stage/usr/bin/overink" "$arch"
  [[ -f "$ROOT/assets/overink.png" ]] || die 'Missing assets/overink.png'
  install -m0644 "$ROOT/assets/overink.png" "$stage/usr/share/pixmaps/overink.png"
  for doc in LICENSE assets/fonts/OFL.txt; do
    [[ -f "$ROOT/$doc" ]] || die "Missing $doc"
    install -m0644 "$ROOT/$doc" "$stage/usr/share/doc/overink/${doc##*/}"
  done
  cat > "$stage/usr/share/applications/overink.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=OverInk
Comment=On-screen drawing and annotation
Exec=overink
Icon=overink
Terminal=false
Categories=Graphics;Education;
DESKTOP
}

check_packager() {
  need rpmbuild
  need rpm
  [[ "${OVERINK_LICENSE:-LicenseRef-OverInk}" =~ ^[A-Za-z0-9.+_-]+$ ]] || die 'Invalid SPDX license identifier'
}

package_target() {
  local stage="$1" arch="$2" glibc="$3" top="$WORK/rpm-$arch" artifact out
  mkdir -p "$top"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
  tar -czf "$top/SOURCES/payload.tar.gz" -C "$stage" .
  cat > "$top/SPECS/overink.spec" <<SPEC
%global debug_package %{nil}
%global __os_install_post %{nil}
Name: overink
Version: $VERSION
Release: 1
Summary: Cross-platform on-screen annotation tool
License: ${OVERINK_LICENSE:-LicenseRef-OverInk}
Source0: payload.tar.gz
AutoReqProv: no
Requires: glibc >= $glibc
Requires: libX11, libwayland-client, libxkbcommon, libglvnd-egl, libglvnd-glx

%description
OverInk is a cross-platform on-screen drawing and annotation tool.

%install
mkdir -p %{buildroot}
tar -xzf %{SOURCE0} -C %{buildroot}

%files
/usr/bin/overink
/usr/share/applications/overink.desktop
/usr/share/pixmaps/overink.png
%license /usr/share/doc/overink/LICENSE
/usr/share/doc/overink/OFL.txt
SPEC
  rpmbuild -bb --target "$arch" --define "_topdir $top" "$top/SPECS/overink.spec"
  artifact="$top/RPMS/$arch/overink-$VERSION-1.$arch.rpm"
  [[ -s "$artifact" ]] || die "RPM not created: $artifact"
  [[ "$(rpm -qp --qf '%{ARCH}' "$artifact")" == "$arch" ]] || die "RPM architecture mismatch: $arch"
  out="$OUTPUT/overink-$VERSION-1.$arch.rpm"
  install -m0644 "$artifact" "$out"
  printf 'Built %s\n' "$out"
}

main() {
  local arch triple pkg_arch stage glibc
  parse_args "$@"
  if ((INSTALL_DEPS)); then
    install_dependencies
  fi
  check_packager
  check_tools
  resolve_targets
  check_sysroots
  WORK="$(mktemp -d)"
  trap cleanup EXIT
  mkdir -p "$OUTPUT" "$BUILD_DIR"
  OUTPUT="$(cd -- "$OUTPUT" && pwd -P)"
  BUILD_DIR="$(cd -- "$BUILD_DIR" && pwd -P)"
  for arch in "${TARGETS[@]}"; do
    triple="$(target_triple "$arch")"
    pkg_arch="$(package_arch "$arch")"
    stage="$WORK/stage-$arch"
    build_target "$arch" "$triple"
    stage_target "$arch" "$triple" "$stage"
    glibc="$(required_glibc "$stage/usr/bin/overink")"
    if [[ -n "$MAX_GLIBC" && "$(printf '%s\n%s\n' "$glibc" "$MAX_GLIBC" | sort -Vu | tail -n1)" != "$MAX_GLIBC" ]]; then
      die "GLIBC $glibc exceeds the required maximum $MAX_GLIBC for $arch"
    fi
    package_target "$stage" "$pkg_arch" "$glibc"
  done
}

main "$@"
