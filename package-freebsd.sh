#!/bin/sh
# Build OverInk from source and create a native FreeBSD binary package.
# This script deliberately never consumes CI archives or existing target files.
set -eu

VERSION_ARG=
BUILD_FROM=
ARCH_OVERRIDE=
INSTALL_DEPS=yes
WORK=
WORKTREE=
SOURCE=
RUST_MODE=system
REPO_URL=https://github.com/xujiegb/OverInk
API_URL=https://api.github.com/repos/xujiegb/OverInk
OUTPUT_DIR="${OVERINK_OUTPUT_DIR:-$HOME/pkgbuild}"
CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
RUNTIME_DEPS="libX11 libXext libXi libXrandr libXcursor libXfixes libXrender libXScrnSaver libXtst libxcb libglvnd wayland libxkbcommon mesa-libs dbus desktop-file-utils xdg-utils hicolor-icon-theme"

die() {
    printf '%s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage: sh package-freebsd.sh [version | --buildfrom 1|2|3] [--arch ARCH]
                             [--no-install-deps]

  --buildfrom 1   latest stable release (default)
  --buildfrom 2   latest pre-release
  --buildfrom 3   current source checkout, including local changes
  --arch ARCH     x64/amd64, arm64/aarch64, i386, ppc64le, riscv64 or all
  --no-install-deps  use already installed build/runtime dependencies

Environment:
  OVERINK_OUTPUT_DIR       output root (default: ~/pkgbuild)
  OVERINK_PACKAGE_VERSION  package current source using this version
  RUST_VERSION             override source Cargo.toml rust-version
  CARGO_BUILD_JOBS         parallel build jobs (default: 2)
  OVERINK_RUN_TESTS=1      run Cargo release tests before packaging (native only)
  OVERINK_PKG_REPOS_DIR    trusted target repository configuration for cross builds

Each target builds from a new target directory. Cross builds use verified
FreeBSD release base archives and target pkg repository metadata.
--arch all builds x64, arm64, i386, ppc64le and riscv64.
Cross ppc64le uses FreeBSD 16.0-CURRENT and its official latest Ports repo.
Cross riscv64 uses FreeBSD 15.1 and the signed anyvm community Ports repo.
OVERINK_PKG_REPOS_DIR can override the default target repository.
EOF
}

parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --help|-h) usage; exit 0 ;;
            --arch|--buildfrom)
                [ "$#" -ge 2 ] || die "Missing value for $1"
                if [ "$1" = --arch ]; then ARCH_OVERRIDE="$2"; else BUILD_FROM="$2"; fi
                shift 2
                ;;
            --no-install-deps) INSTALL_DEPS=no; shift ;;
            --*) die "Unknown argument: $1" ;;
            *)
                [ -z "$VERSION_ARG" ] || die "Unexpected argument: $1"
                VERSION_ARG="$1"
                shift
                ;;
        esac
    done
    [ -z "$VERSION_ARG" ] || [ -z "$BUILD_FROM" ] || die "Specify a version or --buildfrom, not both"
    case "$BUILD_FROM" in ""|1|2|3) ;; *) die "Use --buildfrom 1, 2 or 3" ;; esac
    if [ -n "${OVERINK_PACKAGE_VERSION:-}" ]; then
        [ -z "$VERSION_ARG" ] || die "Do not combine a release tag and OVERINK_PACKAGE_VERSION"
        case "$BUILD_FROM" in ""|3) ;; *) die "OVERINK_PACKAGE_VERSION requires current source" ;; esac
    fi
    case "$CARGO_BUILD_JOBS" in ""|*[!0-9]*) die "Invalid CARGO_BUILD_JOBS" ;; esac
    [ "$CARGO_BUILD_JOBS" -gt 0 ] || die "CARGO_BUILD_JOBS must be positive"
}

as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    elif command -v sudo >/dev/null 2>&1; then
        sudo "$@"
    elif command -v doas >/dev/null 2>&1; then
        doas "$@"
    else
        die "Install build dependencies as root, or use sudo/doas or --no-install-deps."
    fi
}

normalize_arch() {
    case "$1" in
        amd64|x64|x86_64) printf '%s\n' amd64 ;;
        aarch64|arm64) printf '%s\n' aarch64 ;;
        i386|i686|x86) printf '%s\n' i386 ;;
        powerpc64le|ppc64le|ppc64el) printf '%s\n' powerpc64le ;;
        riscv64|riscv64gc) printf '%s\n' riscv64 ;;
        all) printf '%s\n' all ;;
        *) return 1 ;;
    esac
}

detect_environment() {
    [ "$(uname -s)" = FreeBSD ] || die "This script only supports FreeBSD."
    HOST_ARCH=$(normalize_arch "$(uname -p)") || die "Unsupported FreeBSD architecture: $(uname -p)"
    REQUESTED=$(normalize_arch "${ARCH_OVERRIDE:-$HOST_ARCH}") || die "Unsupported --arch: $ARCH_OVERRIDE"
    printf '[*] FreeBSD %s (%s)\n' "$(uname -r)" "$HOST_ARCH"
}

describe_target() {
    PACKAGE_ARCH="$1"
    case "$PACKAGE_ARCH" in
        amd64) TARGET=x86_64-unknown-freebsd; CPU=x86_64; MACHINE='X86-64'; LAYOUT=amd64/amd64; RELEASE=15.1 ;;
        aarch64) TARGET=aarch64-unknown-freebsd; CPU=aarch64; MACHINE='AArch64'; LAYOUT=arm64/aarch64; RELEASE=15.1 ;;
        i386) TARGET=i686-unknown-freebsd; CPU=i686; MACHINE='80386'; LAYOUT=i386/i386; RELEASE=14.4 ;;
        powerpc64le) TARGET=powerpc64le-unknown-freebsd; CPU=powerpc64le; MACHINE='PowerPC64'; LAYOUT=powerpc/powerpc64le; RELEASE=16.0 ;;
        riscv64) TARGET=riscv64gc-unknown-freebsd; CPU=riscv64; MACHINE='RISC-V'; LAYOUT=riscv/riscv64; RELEASE=15.1 ;;
        *) die "Unsupported target: $PACKAGE_ARCH" ;;
    esac
    NATIVE=no
    if [ "$PACKAGE_ARCH" = "$HOST_ARCH" ]; then
        NATIVE=yes
        ABI=$(pkg config ABI)
        case "$ABI" in FreeBSD:*:"$PACKAGE_ARCH") ;; *) die "Unexpected host ABI: $ABI" ;; esac
    else
        ABI="FreeBSD:${RELEASE%%.*}:$PACKAGE_ARCH"
    fi
    printf '[*] Target %s (%s, native=%s)\n' "$TARGET" "$ABI" "$NATIVE"
}


install_dependencies() {
    if [ "$INSTALL_DEPS" = yes ]; then
        if ! pkg -N >/dev/null 2>&1; then
            as_root env ASSUME_ALWAYS_YES=yes pkg bootstrap
        fi
        # Fixed list of package names, intentionally expanded into arguments.
        # shellcheck disable=SC2086
        as_root pkg install -y git curl jq ca_root_nss cmake ninja pkgconf \
            python3 wayland-protocols $RUNTIME_DEPS
    fi
    for tool in pkg git curl jq cmake ninja pkgconf readelf cc c++ python3; do
        command -v "$tool" >/dev/null 2>&1 || die "Missing build tool: $tool"
    done
    pkg -N >/dev/null 2>&1 || die "Bootstrap pkg before building."
}

cleanup() {
    if [ -n "$WORKTREE" ]; then
        git -C "$SCRIPT_DIR" worktree remove --force "$WORKTREE" >/dev/null 2>&1 ||
            printf '[WARN] Could not unregister temporary worktree: %s\n' "$WORKTREE" >&2
    fi
    [ -z "$WORK" ] || rm -rf -- "$WORK"
}

choose_channel() {
    if [ -n "$BUILD_FROM" ]; then printf '%s\n' "$BUILD_FROM"; return; fi
    if [ -t 0 ]; then
        printf '%s\n' '[?] 1) Latest stable (default)  2) Pre-release  3) Keep current' >&2
        printf 'Enter 1, 2 or 3 [1]: ' >&2
        read -r answer || answer=
        case "${answer:-1}" in 1|2|3) printf '%s\n' "${answer:-1}";; *) die "Invalid selection" ;; esac
    else
        printf '%s\n' 1
    fi
}

manifest_field() {
    # Read only [package] fields, not dependency versions.
    awk -v field="$1" '
        /^\[package\]/ { package=1; next }
        /^\[/ { package=0 }
        package && $0 ~ ("^" field "[[:space:]]*=") {
            if (match($0, /"[^"]+"/)) print substr($0, RSTART+1, RLENGTH-2)
            exit
        }
    ' "$SOURCE/Cargo.toml"
}

resolve_source() {
    SOURCE="$SCRIPT_DIR"
    tag=
    if [ -n "${OVERINK_PACKAGE_VERSION:-}" ]; then
        VERSION="${OVERINK_PACKAGE_VERSION#v}"
    elif [ -n "$VERSION_ARG" ]; then
        tag="$VERSION_ARG"
    elif git -C "$SCRIPT_DIR" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        channel=$(choose_channel)
        case "$channel" in
            1) tag=$(curl -fsSL --retry 3 "$API_URL/releases/latest" | jq -er '.tag_name') ||
                die "Unable to resolve latest stable release; use --buildfrom 3." ;;
            2) tag=$(curl -fsSL --retry 3 "$API_URL/releases?per_page=100" |
                jq -er 'first(.[] | select(.prerelease and (.draft | not)) | .tag_name)') ||
                die "Unable to resolve a pre-release; use an explicit version or --buildfrom 3." ;;
            3) ;;
        esac
    elif [ "$BUILD_FROM" = 1 ] || [ "$BUILD_FROM" = 2 ]; then
        die "Release selection requires a Git checkout; use --buildfrom 3 for a source archive."
    fi
    if [ -n "$tag" ]; then
        git -C "$SCRIPT_DIR" rev-parse --is-inside-work-tree >/dev/null 2>&1 ||
            die "Release selection requires a Git checkout; use --buildfrom 3 for a source archive."
        git -C "$SCRIPT_DIR" fetch --tags
        if ! git -C "$SCRIPT_DIR" show-ref --verify --quiet "refs/tags/$tag"; then
            tag="v${tag#v}"
        fi
        git -C "$SCRIPT_DIR" show-ref --verify --quiet "refs/tags/$tag" || die "Release tag not found: $tag"
        # Never checkout -f or alter the developer's current branch.
        WORKTREE="$WORK/source"
        git -C "$SCRIPT_DIR" worktree add --detach "$WORKTREE" "refs/tags/$tag"
        SOURCE="$WORKTREE"
        VERSION="${tag#v}"
    elif [ -z "${VERSION:-}" ]; then
        VERSION=$(manifest_field version)
    fi
    if [ -f "$SOURCE/.gitmodules" ]; then
        git -C "$SOURCE" submodule update --init --recursive
    fi
    printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([.+~-][0-9A-Za-z.+~-]+)?$' ||
        die "Invalid package version: $VERSION"
    # pkg versions cannot contain hyphens (name-version delimiter).
    PKG_VERSION=$(printf '%s' "$VERSION" | sed 's/[-+~]/./g')
    REQUIRED_RUST=$(manifest_field rust-version)
    RUST_VERSION="${RUST_VERSION:-${REQUIRED_RUST:-1.100.0}}"
    printf '%s\n' "$RUST_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
        die "RUST_VERSION must be a numeric Rust release"
    printf '[*] Source version %s; pkg version %s; Rust %s\n' "$VERSION" "$PKG_VERSION" "$RUST_VERSION"
}

rustc_run() {
    if [ "$RUST_MODE" = rustup ]; then rustup run "$RUST_VERSION" rustc "$@"; else rustc "$@"; fi
}

cargo_run() {
    if [ "$RUST_MODE" = rustup ]; then rustup run "$RUST_VERSION" cargo "$@"; else cargo "$@"; fi
}

prepare_rust() {
    export PATH="$HOME/.cargo/bin:/usr/local/bin:$PATH"
    case "$HOST_ARCH" in
        amd64|aarch64)
            if ! command -v rustup >/dev/null 2>&1 && [ "$INSTALL_DEPS" = yes ]; then
                if curl --proto '=https' --tlsv1.2 -fsSL --retry 3 https://sh.rustup.rs -o "$WORK/rustup.sh"; then
                    sh "$WORK/rustup.sh" -y --profile minimal --default-toolchain none --no-modify-path ||
                        printf '[WARN] rustup bootstrap unavailable; trying native Rust package.\n' >&2
                fi
            fi
            if command -v rustup >/dev/null 2>&1; then
                if rustup run "$RUST_VERSION" rustc --version >/dev/null 2>&1 ||
                    { [ "$INSTALL_DEPS" = yes ] && rustup toolchain install "$RUST_VERSION" --profile minimal; }; then
                    RUST_MODE=rustup
                fi
            fi
            ;;
    esac
    if [ "$RUST_MODE" = system ]; then
        if ! /usr/local/bin/rustc --version >/dev/null 2>&1 ||
            ! /usr/local/bin/cargo --version >/dev/null 2>&1; then
            [ "$INSTALL_DEPS" = yes ] || die "Native Rust/Cargo is missing."
            as_root pkg install -y rust
        fi
        # Prefer native package binaries to a rustup shim with no host toolchain.
        export PATH="/usr/local/bin:$PATH"
    fi
    actual=$(rustc_run --version | awk '{print $2}')
    [ -n "$actual" ] || die "Unable to determine Rust version"
    [ "$(pkg version -t "$actual" "$RUST_VERSION")" != '<' ] ||
        die "Rust $actual is older than required $RUST_VERSION. Install a suitable native toolchain."
    RUST_HOST=$(rustc_run -vV | sed -n 's/^host: //p')
    [ -n "$RUST_HOST" ] || die "Could not identify Rust host."
    cargo_run --version
}

target_pkg() {
    pkg -o "ABI=$ABI" -o "OSVERSION=$TARGET_OSVERSION" \
        -o "PKG_DBDIR=$SDK/pkgdb" -o "PKG_CACHEDIR=$SDK/pkgcache" \
        -o "REPOS_DIR=${OVERINK_PKG_REPOS_DIR:-$SDK/repos}" "$@"
}

prepare_target_sdk() {
    SDK="$WORK/sdk-$PACKAGE_ARCH"
    mkdir -p "$SDK"
    BUILD_STD=no
    if [ "$NATIVE" = yes ]; then
        [ "$RUST_HOST" = "$TARGET" ] || die "Rust host $RUST_HOST does not match $TARGET"
        return
    fi
    [ "$RUST_MODE" = rustup ] || die "Cross builds require rustup and rust-src."
    if rustup target list --toolchain "$RUST_VERSION" | awk '{print $1}' | grep -qx "$TARGET"; then
        rustup target add --toolchain "$RUST_VERSION" "$TARGET"
    else
        rustc_run --print target-list | grep -qx "$TARGET" || die "Unknown Rust target: $TARGET"
        rustup component add --toolchain "$RUST_VERSION" rust-src
        BUILD_STD=yes
    fi

    sysroot="$SDK/sysroot"
    mkdir -p "$sysroot" "$SDK/pkgdb" "$SDK/pkgcache" "$SDK/repos"
    if [ "$PACKAGE_ARCH" = powerpc64le ]; then
        url="https://download.freebsd.org/snapshots/$LAYOUT/$RELEASE-CURRENT"
    else
        url="https://download.freebsd.org/releases/$LAYOUT/$RELEASE-RELEASE"
    fi
    fetch -qo "$SDK/MANIFEST" "$url/MANIFEST"
    fetch -qo "$SDK/base.txz" "$url/base.txz"
    expected=$(awk '$1 == "base.txz" {print $2}' "$SDK/MANIFEST")
    [ -n "$expected" ] || die "Missing base.txz checksum"
    [ "$(sha256 -q "$SDK/base.txz")" = "$expected" ] || die "Invalid FreeBSD base checksum"
    tar -xf "$SDK/base.txz" -C "$sysroot"
    rm -f "$SDK/base.txz"
    TARGET_OSVERSION=$(awk '$1 == "#define" && $2 == "__FreeBSD_version" {print $3; exit}' "$sysroot/usr/include/sys/param.h")
    case "$TARGET_OSVERSION" in ""|*[!0-9]*) die "Invalid target OSVERSION" ;; esac
    [ "$((TARGET_OSVERSION / 100000))" = "${RELEASE%%.*}" ] ||
        die "Sysroot FreeBSD major version does not match $ABI"
    # Verify both the CPU and endianness of the downloaded base, not only its URL.
    readelf -h "$sysroot/bin/sh" | grep -iE "Machine:.*$MACHINE" ||
        die "Incorrect sysroot machine for $ABI"
    readelf -h "$sysroot/bin/sh" | grep -iE 'Data:.*little endian' ||
        die "Incorrect sysroot byte order for $ABI"
    # Match the existing FreeBSD build job: SDL dynamically loads graphics
    # libraries at runtime; native headers are shared across these ABIs.
    mkdir -p "$sysroot/usr/local/include"
    for header in /usr/local/include/X11 /usr/local/include/xcb /usr/local/include/wayland*.h \
        /usr/local/include/xkbcommon /usr/local/include/GL /usr/local/include/EGL \
        /usr/local/include/GLES* /usr/local/include/KHR; do
        if [ -e "$header" ]; then cp -RL "$header" "$sysroot/usr/local/include/"; fi
    done
    python3 - "$sysroot" <<'PY'
import os, pathlib, sys
root = pathlib.Path(sys.argv[1])
for directory, dirs, files in os.walk(root, followlinks=False):
    for name in dirs + files:
        path = pathlib.Path(directory) / name
        if path.is_symlink():
            target = os.readlink(path)
            if target.startswith('/'):
                path.unlink()
                path.symlink_to(os.path.relpath(root / target.lstrip('/'), path.parent))
PY
    extra=
    if [ "$CPU" = riscv64 ]; then extra='-march=rv64gc -mabi=lp64d'; fi
    for compiler in clang clang++; do
        cat > "$SDK/$compiler" <<EOF
#!/bin/sh
exec /usr/bin/$compiler --target=$CPU-unknown-freebsd$RELEASE --sysroot="$sysroot" -fuse-ld=lld -Qunused-arguments $extra "\$@"
EOF
        chmod +x "$SDK/$compiler"
    done
    printf '#include <X11/XKBlib.h>\n' |
        "$SDK/clang" -isystem "$sysroot/usr/local/include" -x c -fsyntax-only -
    key=$(printf '%s' "$TARGET" | tr '-' '_')
    upper=$(printf '%s' "$key" | tr '[:lower:]' '[:upper:]')
    export "CC_$key=$SDK/clang" "CXX_$key=$SDK/clang++" "AR_$key=ar"
    export "CARGO_TARGET_${upper}_LINKER=$SDK/clang"
    export PKG_CONFIG_ALLOW_CROSS=1
    export PKG_CONFIG_SYSROOT_DIR="$sysroot"
    export PKG_CONFIG_LIBDIR=/usr/local/libdata/pkgconfig:/usr/local/lib/pkgconfig:/usr/local/share/pkgconfig
    export CMAKE_TOOLCHAIN_FILE="$SDK/toolchain.cmake"
    cat > "$CMAKE_TOOLCHAIN_FILE" <<EOF
set(CMAKE_SYSTEM_NAME FreeBSD)
set(CMAKE_SYSTEM_PROCESSOR $CPU)
set(CMAKE_SYSROOT "$sysroot")
set(CMAKE_C_COMPILER "$SDK/clang")
set(CMAKE_CXX_COMPILER "$SDK/clang++")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY BOTH)
set(CMAKE_LIBRARY_PATH /usr/local/lib)
set(HAVE_XKBLIB_H ON CACHE INTERNAL "Cross-checked XKB header")
set(SDL_X11_SHARED ON CACHE BOOL "" FORCE)
set(SDL_WAYLAND_SHARED ON CACHE BOOL "" FORCE)
set(SDL_WAYLAND_LIBDECOR OFF CACHE BOOL "" FORCE)
set(SDL_KMSDRM OFF CACHE BOOL "" FORCE)
EOF
    prepare_target_repository
}

prepare_target_repository() {
    # Keep catalogues and signing keys out of the host package database.
    if [ -z "${OVERINK_PKG_REPOS_DIR:-}" ]; then
        case "$PACKAGE_ARCH" in
            riscv64)
                # anyvm publishes signed, numbered shards. Pin the public key
                # from its documented fingerprint and accept only the expected
                # repository URLs and signature settings from its index config.
                index=https://github.com/anyvm-org/freebsd-pkg-repo/releases/download/idx-FreeBSD-15-riscv64
                curl -fsSL --retry 3 "$index/repo.pub" -o "$SDK/anyvm.pub"
                [ "$(sha256 -q "$SDK/anyvm.pub")" = a9e2f84083b916f0f9f2bda18ebf9cc581cfb28aaadb6827836f1ddc672a3040 ] ||
                    die "Unexpected anyvm repository signing key"
                curl -fsSL --retry 3 "$index/anyvm.conf" -o "$SDK/anyvm.conf"
                python3 - "$SDK" <<'PY'
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
config = (root / "anyvm.conf").read_text(encoding="ascii")
pattern = re.compile(
    r'anyvm-(\d{3}):\s*\{\s*'
    r'url:\s*"https://github\.com/anyvm-org/freebsd-pkg-repo/releases/download/'
    r'pkg-FreeBSD-15-riscv64-\1",\s*enabled:\s*yes,\s*'
    r'signature_type:\s*"pubkey",\s*'
    r'pubkey:\s*"/usr/local/etc/pkg/keys/anyvm\.pub"\s*\}'
)
shards = pattern.findall(config)
if not shards or pattern.sub("", config).strip() or len(set(shards)) != len(shards):
    raise SystemExit("Unexpected anyvm repository configuration")
# Generate our own config rather than installing externally supplied UCL.
import json
with (root / "repos" / "anyvm.conf").open("w", encoding="ascii") as out:
    for shard in shards:
        out.write(
            f'anyvm-{shard}: {{\n'
            f'  url: "https://github.com/anyvm-org/freebsd-pkg-repo/releases/download/pkg-FreeBSD-15-riscv64-{shard}",\n'
            '  enabled: yes,\n  signature_type: "pubkey",\n'
            f'  pubkey: {json.dumps(str(root / "anyvm.pub"))}\n}}\n'
        )
PY
                ;;
            *)
                channel=quarterly
                [ "$PACKAGE_ARCH" != powerpc64le ] || channel=latest
                cat > "$SDK/repos/OverInk.conf" <<EOF
OverInk: {
  url: "pkg+https://pkg.FreeBSD.org/$ABI/$channel",
  mirror_type: "srv",
  signature_type: "fingerprints",
  fingerprints: "/usr/share/keys/pkg",
  enabled: yes
}
EOF
                ;;
        esac
    fi
    target_pkg update -f
}


build_binary() {
    # New target directory on every invocation, no compiler-result cache.
    export CARGO_TARGET_DIR="$WORK/target-$PACKAGE_ARCH"
    export CARGO_BUILD_JOBS
    export CARGO_INCREMENTAL=0
    export RUSTC_WRAPPER=
    export RUSTC_WORKSPACE_WRAPPER=
    export OVERINK_BUILD_VERSION="$VERSION"
    export CMAKE_PREFIX_PATH=/usr/local
    unset CARGO_BUILD_TARGET
    cd "$SOURCE"
    if [ "$NATIVE" = yes ] && [ "${OVERINK_RUN_TESTS:-0}" = 1 ]; then
        cargo_run test --locked --release --target "$TARGET"
    fi
    if [ "$BUILD_STD" = yes ]; then
        RUSTC_BOOTSTRAP=1 CARGO_PROFILE_RELEASE_PANIC=abort cargo_run build \
            -Z build-std=std,panic_abort --locked --release --target "$TARGET"
    else
        cargo_run build --locked --release --target "$TARGET"
    fi
    BINARY="$CARGO_TARGET_DIR/$TARGET/release/overink"
    [ -s "$BINARY" ] || die "Missing fresh executable: $BINARY"
    readelf -h "$BINARY" | grep -iE "Machine:.*$MACHINE" ||
        die "Incorrect ELF architecture"
    readelf -h "$BINARY" | grep -iE 'Data:.*little endian' || die "Incorrect ELF byte order"
    readelf -n "$BINARY" | grep -E 'FreeBSD[[:space:]].*NT_FREEBSD_ABI_TAG' ||
        die "Missing FreeBSD ELF ABI note"
    config=$(find "$CARGO_TARGET_DIR/$TARGET/release/build" -name SDL_build_config.h -print -quit)
    [ -n "$config" ] || die "Missing SDL build configuration"
    grep -q '^#define SDL_VIDEO_DRIVER_X11 1' "$config" || die "X11 backend is disabled"
    grep -q '^#define SDL_VIDEO_DRIVER_WAYLAND 1' "$config" || die "Wayland backend is disabled"
    if [ "$NATIVE" = yes ]; then
        OVERINK_STATE_DIR="$WORK/smoke" "$BINARY" --smoke-test
    else
        grep -q '^#define SDL_VIDEO_DRIVER_X11_DYNAMIC "libX11\.so' "$config" || die "X11 must load dynamically"
        grep -q '^#define SDL_VIDEO_DRIVER_WAYLAND_DYNAMIC "libwayland-client\.so' "$config" || die "Wayland must load dynamically"
        if readelf -d "$BINARY" | grep -E 'NEEDED.*(libX|libwayland|libxkbcommon|libEGL|libGL)'; then
            die "Cross binary links host graphics libraries"
        fi
    fi
}

stage_assets() {
    STAGE="$WORK/stage-$PACKAGE_ARCH"
    META="$WORK/meta-$PACKAGE_ARCH"
    mkdir -p "$META" "$STAGE/usr/local/bin" \
        "$STAGE/usr/local/share/applications" \
        "$STAGE/usr/local/share/icons/hicolor/256x256/apps" \
        "$STAGE/usr/local/share/icons/hicolor/scalable/apps" \
        "$STAGE/usr/local/share/licenses/overink"
    install -m 755 "$BINARY" "$STAGE/usr/local/bin/overink"
    install -m 644 "$SOURCE/LICENSE" "$STAGE/usr/local/share/licenses/overink/LICENSE"
    install -m 644 "$SOURCE/assets/fonts/OFL.txt" "$STAGE/usr/local/share/licenses/overink/OFL.txt"
    install -m 644 "$SOURCE/assets/overink.png" "$STAGE/usr/local/share/icons/hicolor/256x256/apps/overink.png"
    install -m 644 "$SOURCE/assets/overink.svg" "$STAGE/usr/local/share/icons/hicolor/scalable/apps/overink.svg"
    cat > "$STAGE/usr/local/share/applications/overink.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=OverInk
Comment=Pressure-sensitive desktop annotation
Exec=/usr/local/bin/overink
Icon=overink
Terminal=false
Categories=Graphics;Utility;
StartupWMClass=org.overink.OverInk
EOF
    chmod 644 "$STAGE/usr/local/share/applications/overink.desktop"
    desktop-file-validate "$STAGE/usr/local/share/applications/overink.desktop"
}

write_metadata() {
    # Resolve names, origins and versions from the correct target package set.
    : > "$META/dependencies"
    for dep in $RUNTIME_DEPS; do
        if [ "$NATIVE" = yes ]; then
            row=$(pkg query '%n %o %v' "$dep") || die "Missing runtime package: $dep"
        else
            rows=$(target_pkg rquery -U '%n %o %v' "$dep") || die "Missing $ABI package: $dep"
            row=$(printf '%s\n' "$rows" | LC_ALL=C sort -u)
        fi
        [ "$(printf '%s\n' "$row" | wc -l | tr -d ' ')" = 1 ] || die "Ambiguous package: $dep"
        [ -n "$row" ] || die "Missing runtime package: $dep"
        printf '%s\n' "$row" >> "$META/dependencies"
    done
    jq -Rn '[inputs | split(" ") | {key: .[0], value: {origin: .[1], version: .[2]}}] | from_entries' \
        < "$META/dependencies" > "$META/deps.json"
    jq -n --arg version "$PKG_VERSION" --arg abi "$ABI" --arg url "$REPO_URL" \
        --slurpfile deps "$META/deps.json" '{
        name: "overink", version: $version, origin: "graphics/overink",
        comment: "Pressure-sensitive desktop annotation with egui and native SDL3 input",
        desc: "OverInk supports native stylus pressure, multi-touch drawing and click-through overlays.",
        maintainer: "noreply@github.com", www: $url, prefix: "/usr/local",
        abi: $abi, categories: ["graphics"], licenselogic: "and",
        licenses: ["BSD3CLAUSE", "OFL11", "CC-BY-SA-4.0"], deps: $deps[0]
    }' > "$META/+MANIFEST"
    {
        printf '%s\n' '@owner root' '@group wheel'
        (cd "$STAGE/usr/local" && find . -type f | LC_ALL=C sort | sed 's|^\./||')
        printf '%s\n' '@dir share/licenses/overink'
    } > "$META/pkg-plist"
}

package_binary() {
    mkdir -p "$WORK/out-$PACKAGE_ARCH"
    if [ "$NATIVE" = yes ]; then
        pkg create -r "$STAGE" -m "$META" -p "$META/pkg-plist" -f tzst -o "$WORK/out-$PACKAGE_ARCH"
    else
        target_pkg create -r "$STAGE" -m "$META" -p "$META/pkg-plist" -f tzst -o "$WORK/out-$PACKAGE_ARCH"
    fi
    archive="$WORK/out-$PACKAGE_ARCH/overink-$PKG_VERSION.pkg"
    [ -s "$archive" ] || die "Missing generated pkg: $archive"
    tar -xOf "$archive" +COMPACT_MANIFEST | jq -e --arg abi "$ABI" --arg version "$PKG_VERSION" \
        '.name == "overink" and .version == $version and .abi == $abi and (.deps | has("desktop-file-utils") and has("xdg-utils"))' >/dev/null ||
        die "Incorrect package ABI or missing desktop dependencies"
    tar -tf "$archive" | grep -Eq '(^|/)usr/local/bin/overink$' ||
        die "Package missing executable"
    abi_dir=$(printf '%s' "$ABI" | tr ':' '-')
    mkdir -p "$OUTPUT_DIR/$abi_dir"
    output="$OUTPUT_DIR/$abi_dir/overink-$PKG_VERSION.pkg"
    mv -f "$archive" "$output"
    printf '\n[OK] %s\n' "$output"
    if [ "$NATIVE" = yes ]; then
        printf 'Install: sudo pkg install "%s"\n' "$output"
    else
        printf 'Install on %s: sudo pkg install overink-%s.pkg\n' "$ABI" "$PKG_VERSION"
    fi
}

main() {
    parse_args "$@"
    detect_environment
    SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
    [ -f "$SCRIPT_DIR/Cargo.toml" ] || die "Place package-freebsd.sh in the OverInk source root."
    [ -f "$SCRIPT_DIR/Cargo.lock" ] || die "Cargo.lock not found."
    mkdir -p "$OUTPUT_DIR"
    OUTPUT_DIR=$(CDPATH= cd -- "$OUTPUT_DIR" && pwd)
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/overink-pkg.XXXXXXXX")
    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    export PATH="/usr/local/bin:$PATH"
    install_dependencies
    resolve_source
    prepare_rust
    if [ "$REQUESTED" = all ]; then
        targets="amd64 aarch64 i386 powerpc64le riscv64"
    else
        targets="$REQUESTED"
    fi
    for arch in $targets; do
        # A subshell isolates target-specific compiler variables and pkg ABI.
        (
            unset CMAKE_TOOLCHAIN_FILE PKG_CONFIG_SYSROOT_DIR PKG_CONFIG_LIBDIR PKG_CONFIG_ALLOW_CROSS
            describe_target "$arch"
            prepare_target_sdk
            build_binary
            stage_assets
            write_metadata
            package_binary
        )
    done
}

main "$@"
