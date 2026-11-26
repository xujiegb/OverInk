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
  --arch ARCH     native amd64/x64, aarch64/arm64, i386, powerpc64le or riscv64
  --no-install-deps  use already installed build/runtime dependencies

Environment:
  OVERINK_OUTPUT_DIR       output root (default: ~/pkgbuild)
  OVERINK_PACKAGE_VERSION  package current source using this version
  RUST_VERSION             override source Cargo.toml rust-version
  CARGO_BUILD_JOBS         parallel build jobs (default: 2)
  OVERINK_RUN_TESTS=1      run Cargo release tests before packaging

Each invocation builds from a new target directory. Cross-architecture packages
must be built on a matching FreeBSD host/VM with a working native Rust toolchain.
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
        *) return 1 ;;
    esac
}

detect_environment() {
    [ "$(uname -s)" = FreeBSD ] || die "This script only supports FreeBSD."
    HOST_ARCH=$(normalize_arch "$(uname -p)") || die "Unsupported FreeBSD architecture: $(uname -p)"
    if [ -n "$ARCH_OVERRIDE" ]; then
        requested=$(normalize_arch "$ARCH_OVERRIDE") || die "Unsupported --arch: $ARCH_OVERRIDE (native builds only)"
        [ "$requested" = "$HOST_ARCH" ] || die "Requested $requested, but this is $HOST_ARCH. Use a matching FreeBSD VM/host."
    fi
    case "$HOST_ARCH" in
        amd64) TARGET=x86_64-unknown-freebsd; MACHINE='X86-64' ;;
        aarch64) TARGET=aarch64-unknown-freebsd; MACHINE='AArch64' ;;
        i386) TARGET=i686-unknown-freebsd; MACHINE='80386' ;;
        powerpc64le) TARGET=powerpc64le-unknown-freebsd; MACHINE='PowerPC64' ;;
        riscv64) TARGET=riscv64gc-unknown-freebsd; MACHINE='RISC-V' ;;
    esac
    printf '[*] FreeBSD %s (%s, %s)\n' "$(uname -r)" "$HOST_ARCH" "$TARGET"
}

install_dependencies() {
    if [ "$INSTALL_DEPS" = yes ]; then
        if ! pkg -N >/dev/null 2>&1; then
            as_root env ASSUME_ALWAYS_YES=yes pkg bootstrap
        fi
        # Fixed list of package names, intentionally expanded into arguments.
        # shellcheck disable=SC2086
        as_root pkg install -y git curl jq ca_root_nss cmake ninja pkgconf \
            wayland-protocols $RUNTIME_DEPS
    fi
    for tool in pkg git curl jq cmake ninja pkgconf readelf cc c++; do
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
    host=$(rustc_run -vV | sed -n 's/^host: //p')
    [ "$host" = "$TARGET" ] || die "Rust host $host does not match $TARGET."
    cargo_run --version
}

build_binary() {
    # New target directory on every invocation, no compiler-result cache.
    export CARGO_TARGET_DIR="$WORK/target"
    export CARGO_BUILD_JOBS
    export CARGO_INCREMENTAL=0
    export RUSTC_WRAPPER=
    export RUSTC_WORKSPACE_WRAPPER=
    export OVERINK_BUILD_VERSION="$VERSION"
    export CMAKE_PREFIX_PATH=/usr/local
    unset CARGO_BUILD_TARGET CMAKE_TOOLCHAIN_FILE PKG_CONFIG_SYSROOT_DIR PKG_CONFIG_LIBDIR
    cd "$SOURCE"
    if [ "${OVERINK_RUN_TESTS:-0}" = 1 ]; then
        cargo_run test --locked --release --target "$TARGET"
    fi
    cargo_run build --locked --release --target "$TARGET"
    BINARY="$CARGO_TARGET_DIR/$TARGET/release/overink"
    [ -s "$BINARY" ] || die "Missing fresh executable: $BINARY"
    readelf -h "$BINARY" | grep -iE "Machine:.*$MACHINE" ||
        die "Incorrect ELF architecture"
    readelf -n "$BINARY" | grep -E 'FreeBSD[[:space:]].*NT_FREEBSD_ABI_TAG' ||
        die "Missing FreeBSD ELF ABI note"
    config=$(find "$CARGO_TARGET_DIR/$TARGET/release/build" -name SDL_build_config.h -print -quit)
    [ -n "$config" ] || die "Missing SDL build configuration"
    grep -q '^#define SDL_VIDEO_DRIVER_X11 1' "$config" || die "X11 backend is disabled"
    grep -q '^#define SDL_VIDEO_DRIVER_WAYLAND 1' "$config" || die "Wayland backend is disabled"
    OVERINK_STATE_DIR="$WORK/smoke" "$BINARY" --smoke-test
}

stage_assets() {
    STAGE="$WORK/stage"
    META="$WORK/meta"
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
    ABI=$(pkg config ABI)
    case "$ABI" in FreeBSD:*:"$HOST_ARCH") ;; *) die "Unexpected pkg ABI: $ABI" ;; esac
    # Resolve real names, versions and Ports origins from this target's pkg DB.
    : > "$META/dependencies"
    for dep in $RUNTIME_DEPS; do
        row=$(pkg query '%n %o %v' "$dep") || die "Missing runtime package: $dep"
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
    mkdir -p "$WORK/out"
    pkg create -r "$STAGE" -m "$META" -p "$META/pkg-plist" -f tzst -o "$WORK/out"
    archive="$WORK/out/overink-$PKG_VERSION.pkg"
    [ -s "$archive" ] || die "Missing generated pkg: $archive"
    [ "$(pkg query -F "$archive" '%n %v')" = "overink $PKG_VERSION" ] ||
        die "Incorrect package name/version"
    tar -xOf "$archive" +COMPACT_MANIFEST | jq -e --arg abi "$ABI" \
        '.abi == $abi and (.deps | has("desktop-file-utils") and has("xdg-utils"))' >/dev/null ||
        die "Incorrect package ABI or missing desktop dependencies"
    tar -tf "$archive" | grep -Eq '(^|/)usr/local/bin/overink$' ||
        die "Package missing executable"
    abi_dir=$(printf '%s' "$ABI" | tr ':' '-')
    mkdir -p "$OUTPUT_DIR/$abi_dir"
    output="$OUTPUT_DIR/$abi_dir/overink-$PKG_VERSION.pkg"
    mv -f "$archive" "$output"
    printf '\n[OK] %s\n' "$output"
    printf 'Install: sudo pkg install "%s"\n' "$output"
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
    build_binary
    stage_assets
    write_metadata
    package_binary
}

main "$@"
