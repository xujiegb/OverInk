#!/usr/bin/env bash
set -euo pipefail

VERSION_ARG=""
ARCH_OVERRIDE=""
BUILD_FROM=""

PKGROOT="OverInk-publish"
PROJECT_HINT="Cargo.toml"
OUTPUT_DIR="${OVERINK_OUTPUT_DIR:-${HOME}/debbuild}"

RUST_VERSION="${RUST_VERSION:-1.99.1}"
CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
CARGO_TARGET_DIR=""
SDK_WORK_DIR=""
LLVM_TOOL_BIN=""

OS_ID=""
OS_NAME=""
OS_VERSION_ID=""
HOST_ARCH=""
SCRIPT_DIR=""
PROJECT=""
VERSION=""

declare -a BUILT_DEBS=()

die() {
  echo "$*" >&2
  exit 1
}

as_root() {
  if (( EUID == 0 )); then "$@"; else sudo "$@"; fi
}

parse_args() {
  while (( $# )); do
    case "$1" in
      --arch|--buildfrom)
        (( $# >= 2 )) || die "Missing value for $1"
        if [[ "$1" == --arch ]]; then ARCH_OVERRIDE="$2"; else BUILD_FROM="$2"; fi
        shift 2
        ;;
      --*) die "Unknown argument: $1" ;;
      *)
        [[ -z "$VERSION_ARG" ]] || die "Unexpected argument: $1"
        VERSION_ARG="$1"
        shift
        ;;
    esac
  done
  [[ -z "$VERSION_ARG" || -z "$BUILD_FROM" ]] || die "Specify a version or --buildfrom, not both"
}

detect_environment() {
  . /etc/os-release

  OS_ID="${ID:-}"
  OS_NAME="${NAME:-$OS_ID}"
  OS_VERSION_ID="${VERSION_ID:-}"
  HOST_ARCH="$(uname -m)"

  case "$OS_ID" in
    debian)
      echo "Detected supported system: ${OS_NAME:-$OS_ID} ${OS_VERSION_ID:-}"
      ;;
    *)
      die "Unsupported system: ${OS_NAME:-unknown} (${OS_ID:-unknown}).
This script only supports: Debian."
      ;;
  esac

  case "$HOST_ARCH" in
    x86_64|aarch64|i686|i386|riscv64|ppc64le|loongarch64|s390x) ;;
    *) die "Only supports x86_64 / aarch64 / i686 / riscv64 / ppc64le / loongarch64 / s390x" ;;
  esac

}

install_dependencies() {
  if [[ -n "${OVERINK_ARCHIVE_DIR:-}" ]]; then
    as_root apt-get update
    as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      dpkg-dev tar gzip binutils
    return
  fi
  as_root apt-get update
  as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    curl unzip tar jq rsync ca-certificates git dpkg-dev fakeroot file \
    desktop-file-utils xdg-utils wget clang lld llvm cmake make \
    pkg-config findutils diffutils binutils gzip xz-utils zstd \
    libexpat1-dev libxml2-dev dbus python3

  if ! command -v rustup >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 https://sh.rustup.rs -o /tmp/overink-rustup.sh
    sh /tmp/overink-rustup.sh -y --profile minimal --default-toolchain "$RUST_VERSION"
    rm -f /tmp/overink-rustup.sh
  fi
  [[ ! -f "$HOME/.cargo/env" ]] || . "$HOME/.cargo/env"
  rustup toolchain install "$RUST_VERSION" --profile minimal
  rustup default "$RUST_VERSION"
}

prepare_workspace() {
  SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  cd "$SCRIPT_DIR"

  sync_submodules

  PROJECT="$PROJECT_HINT"
  [[ -f "$PROJECT" ]] || PROJECT="$(find . -maxdepth 3 -name 'Cargo.toml' | head -n1 || true)"
  [[ -f "$PROJECT" ]] || die "Cargo.toml not found"
}

choose_channel() {
  local ch="latest"
  local sel=""

  if [[ -n "${BUILD_FROM:-}" ]]; then
    case "$BUILD_FROM" in
      1) echo "latest"; return 0 ;;
      2) echo "prerelease"; return 0 ;;
      3) echo "keep"; return 0 ;;
      *) die "[ERROR] Invalid --buildfrom value: ${BUILD_FROM}. Use 1|2|3." ;;
    esac
  fi

  if [[ -t 0 ]]; then
    echo "[?] Choose OverInk release channel:" >&2
    echo "    1) Latest (stable)  [default]" >&2
    echo "    2) Pre-release (preview)" >&2
    echo "    3) Keep current (do nothing)" >&2
    printf "Enter 1, 2 or 3 [default 1]: " >&2

    if read -r sel </dev/tty; then
      case "${sel:-}" in
        2) ch="prerelease" ;;
        3) ch="keep" ;;
      esac
    fi
  fi

  echo "$ch"
}

get_latest_tag_latest() {
  curl -fsSL "https://api.github.com/repos/xujiegb/OverInk/releases/latest" \
    | jq -re '.tag_name' \
    | sed 's/^v//'
}

get_latest_tag_prerelease() {
  curl -fsSL "https://api.github.com/repos/xujiegb/OverInk/releases?per_page=20" \
    | jq -re 'first(.[] | select(.prerelease == true) | .tag_name)' \
    | sed 's/^v//'
}

sync_submodules() {
  if [[ -f .gitmodules ]]; then
    git submodule sync --recursive || true
    git submodule update --init --recursive || true
  fi
}

git_try_checkout() {
  local want="$1"
  local ref=""

  if git rev-parse --git-dir >/dev/null 2>&1; then
    git fetch --tags --force --prune --depth=1 || true
    if git rev-parse "refs/tags/${want}" >/dev/null 2>&1; then
      ref="$want"
    elif git rev-parse "refs/tags/v${want#v}" >/dev/null 2>&1; then
      ref="v${want#v}"
    fi

    if [[ -n "$ref" ]]; then
      echo "[OK] Found ref '${ref}', checking out..."
      git checkout -f "$ref"
      sync_submodules
      return 0
    fi
  fi

  return 1
}

cargo_manifest_version() {
  sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$PROJECT" | head -n1
}

apply_channel_or_keep() {
  local ch="$1"
  local tag=""

  if [[ "$ch" == "keep" ]]; then
    echo "[*] Keep current repository state (no checkout)."
    VERSION="$(cargo_manifest_version)"
    VERSION="${VERSION#v}"
    return 0
  fi

  echo "[*] Resolving ${ch} tag from GitHub releases..."

  case "$ch" in
    latest)     tag="$(get_latest_tag_latest || true)" ;;
    prerelease) tag="$(get_latest_tag_prerelease || true)" ;;
    *)          die "Failed to resolve latest tag for channel '${ch}'." ;;
  esac

  if [[ -z "$tag" ]]; then
    echo "[WARN] No remote release tag found for '${ch}', using current repository version."
    VERSION="$(cargo_manifest_version || echo '0.1.0')"
    VERSION="${VERSION#v}"
    return 0
  fi

  echo "[*] Latest tag for '${ch}': ${tag}"
  git_try_checkout "$tag" || die "Failed to checkout '${tag}'."
  VERSION="${tag#v}"
}

resolve_version() {
  if [[ -n "${OVERINK_PACKAGE_VERSION:-}" ]]; then
    VERSION="${OVERINK_PACKAGE_VERSION#v}"
  elif [[ -n "$VERSION_ARG" ]]; then
    if git rev-parse --git-dir >/dev/null 2>&1; then
      git_try_checkout "${VERSION_ARG#v}" || die "Release tag not found: $VERSION_ARG"
    fi
    VERSION="${VERSION_ARG#v}"
  elif git rev-parse --git-dir >/dev/null 2>&1; then
    apply_channel_or_keep "$(choose_channel)"
  else
    VERSION="$(cargo_manifest_version)"
  fi

  [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.+~][0-9A-Za-z.+~-]+)?$ ]] || die "Invalid package version: $VERSION"
  echo "[*] Package version: $VERSION"
}

is_native_target() {
  local short="$1"

  case "$HOST_ARCH:$short" in
    x86_64:x64|aarch64:arm64|i686:i386|i386:i386|ppc64le:ppc64le|riscv64:riscv64|loongarch64:loong64|s390x:s390x)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

prepare_loongarch_compiler() {
  LLVM_TOOL_BIN="${OVERINK_LLVM_BIN_DIR:-$(dirname "$(command -v clang)")}"
  [[ -x "$LLVM_TOOL_BIN/clang" && -x "$LLVM_TOOL_BIN/clang++" ]] || die "Clang/Clang++ not found in $LLVM_TOOL_BIN"

  if [[ "$1" == loong64 ]] && ! "$LLVM_TOOL_BIN/clang" --print-targets 2>/dev/null | grep -qi loongarch; then
    local archive=""
    local checksum=""
    case "$HOST_ARCH" in
      x86_64|aarch64)
        case "$HOST_ARCH" in
          x86_64) checksum="8d5d1e52a06e1c10e457fd36489047053ac451ddad9c88017b0d0fc6b8e5d4c0" ;;
          aarch64) checksum="0b0131b9a6fb38c2fbe73e576d7812962268db610b8b2f1b6ef217dd5161ef77" ;;
        esac
        [[ -z "${OVERINK_LLVM_BIN_DIR:-}" ]] || die "The supplied LLVM toolchain lacks LoongArch support"
        archive="$SDK_WORK_DIR/llvm.tar.xz"
        mkdir -p "$SDK_WORK_DIR/llvm"
        curl -fL --retry 3 \
          "https://www.kernel.org/pub/tools/llvm/files/llvm-22.1.8-${HOST_ARCH}.tar.xz" \
          -o "$archive"
        printf '%s  %s\n' "$checksum" "$archive" | sha256sum -c -
        tar -xJf "$archive" -C "$SDK_WORK_DIR/llvm" --strip-components=1
        rm -f "$archive"
        LLVM_TOOL_BIN="$SDK_WORK_DIR/llvm/bin"
        ;;
      *) die "Supply OVERINK_LLVM_BIN_DIR with a LoongArch-capable Clang for $HOST_ARCH" ;;
    esac
  fi

  if [[ "$1" == loong64 ]]; then
    "$LLVM_TOOL_BIN/clang" --print-targets | grep -qi loongarch || die "Clang lacks the LoongArch backend"
    "$LLVM_TOOL_BIN/ld.lld" --version >/dev/null || die "ld.lld is missing from $LLVM_TOOL_BIN"
    echo "[OK] Using LLVM Clang with LoongArch support: $LLVM_TOOL_BIN/clang"
  fi
}
prepare_target_sdk() {
  local short="$1"
  local rust_target="$2"
  local deb_arch="$3"
  local suite="$4"
  local gcc_ver="$5"
  local repo="https://deb.debian.org/debian"

  rustup target add "$rust_target"

  SDK_WORK_DIR="$(mktemp -d)"
  mkdir -p "$SDK_WORK_DIR/sysroot/usr/share" "$SDK_WORK_DIR/debs" "$SDK_WORK_DIR/cc"
  prepare_loongarch_compiler "$short"

  for share_dir in /usr/share/wayland /usr/share/wayland-protocols; do
    [[ -d "$share_dir" ]] && cp -a "$share_dir" "$SDK_WORK_DIR/sysroot/usr/share/" || true
  done

  if [[ "$deb_arch" == "loong64" ]]; then
    repo="https://loong13.debian.net/debian-loong64"
  fi

  python3 - "$repo" "$suite" "$deb_arch" "$gcc_ver" "$SDK_WORK_DIR" "$short" "$HOST_ARCH" <<'PY'
import gzip, hashlib, lzma, os, pathlib, shutil, subprocess, sys, time, urllib.error, urllib.request

repo, suite, arch, gcc_ver, work_str, short, host_arch = sys.argv[1:8]
work, sysroot, debs_dir = pathlib.Path(work_str), pathlib.Path(work_str) / "sysroot", pathlib.Path(work_str) / "debs"

def fetch_index(base_url, dist, binary_arch):
    errors = []
    for ext, dec in (("xz", lzma.decompress), ("gz", gzip.decompress)):
        url = f"{base_url}/dists/{dist}/main/binary-{binary_arch}/Packages.{ext}"
        for attempt in range(3):
            try:
                with urllib.request.urlopen(url, timeout=30) as response:
                    return dec(response.read()).decode("utf-8", errors="replace")
            except urllib.error.HTTPError as exc:
                if exc.code == 404:
                    errors.append(f"{url}: {exc}")
                    break
                if attempt == 2:
                    errors.append(f"{url}: {exc}")
            except Exception as exc:
                if attempt == 2:
                    errors.append(f"{url}: {type(exc).__name__}: {exc}")
            if attempt < 2:
                time.sleep(attempt + 1)
    raise RuntimeError("Cannot fetch package index:\n" + "\n".join(errors))

def parse_packages(raw):
    pkgs = {}
    for block in raw.split("\n\n"):
        name = filename = checksum = None
        deps, provides = [], []
        for line in block.splitlines():
            if line.startswith("Package: "): name = line[9:].strip()
            elif line.startswith("Filename: "): filename = line[10:].strip()
            elif line.startswith("SHA256: "): checksum = line[8:].strip()
            elif line.startswith("Depends: "):
                deps.extend(p.strip().split("|")[0].strip().split()[0].split(":")[0] for p in line[9:].split(",") if p.strip())
            elif line.startswith("Provides: "):
                provides.extend(p.strip().split()[0].split(":")[0] for p in line[10:].split(",") if p.strip())
        if name and filename and checksum:
            pkgs[name] = (filename, deps, checksum)
            for p in provides: pkgs.setdefault(p, (filename, deps, checksum))
    return pkgs

def extract_deb(deb_path, dest):
    if shutil.which("dpkg-deb"):
        subprocess.run(["dpkg-deb", "-x", str(deb_path), str(dest)], check=True)
        return
    members = subprocess.check_output(["ar", "t", str(deb_path)], text=True).splitlines()
    member = next(m for m in members if m.startswith("data.tar"))
    if member.endswith(".xz"):
        flags = ["-xJf"]
    elif member.endswith(".gz"):
        flags = ["-xzf"]
    elif member.endswith(".zst"):
        flags = ["--zstd", "-xf"]
    elif member == "data.tar":
        flags = ["-xf"]
    else:
        raise RuntimeError(f"Unsupported DEB compression: {member}")
    with subprocess.Popen(["ar", "p", str(deb_path), member], stdout=subprocess.PIPE) as archive:
        subprocess.run(["tar", *flags, "-", "-C", str(dest)], stdin=archive.stdout, check=True)
        archive.stdout.close()
        if archive.wait():
            raise RuntimeError(f"Cannot extract {deb_path}")

pkgs = parse_packages(fetch_index(repo, suite, arch))
queue = [
    "libc6-dev", f"libgcc-{gcc_ver}-dev", f"libstdc++-{gcc_ver}-dev",
    "libx11-dev", "libxext-dev", "libxi-dev", "libxrandr-dev", "libxcursor-dev",
    "libxfixes-dev", "libxrender-dev", "libxss-dev", "libxtst-dev",
    "libwayland-dev", "wayland-protocols", "libxkbcommon-dev", "libegl-dev", "libgl-dev",
]
missing = [pkg for pkg in queue if pkg not in pkgs]
if missing:
    raise RuntimeError(f"Missing required {suite}/{arch} packages: {', '.join(missing)}")
seen, files = set(), []
while queue:
    pkg = queue.pop(0)
    if pkg in seen or pkg not in pkgs: continue
    seen.add(pkg)
    fname, deps, checksum = pkgs[pkg]
    if fname not in files: files.append((fname, checksum))
    queue.extend(d for d in deps if d not in seen)

for fname, checksum in files:
    deb_path = debs_dir / os.path.basename(fname)
    if not deb_path.exists(): urllib.request.urlretrieve(f"{repo}/{fname}", deb_path)
    if hashlib.sha256(deb_path.read_bytes()).hexdigest() != checksum: raise RuntimeError(f"Bad SHA256: {fname}")
    extract_deb(deb_path, sysroot)

if not shutil.which("wayland-scanner"):
    host_arch_map = {
        "x86_64": "amd64", "aarch64": "arm64", "i686": "i386", "i386": "i386",
        "ppc64le": "ppc64el", "riscv64": "riscv64", "s390x": "s390x", "loongarch64": "loong64",
    }
    h_arch = host_arch_map.get(host_arch)
    if not h_arch:
        raise RuntimeError(f"A host wayland-scanner is required on {host_arch}")
    h_repo = "https://loong13.debian.net/debian-loong64" if h_arch == "loong64" else "https://deb.debian.org/debian"
    host_packages = parse_packages(fetch_index(h_repo, "trixie", h_arch))
    fname, _, checksum = host_packages["libwayland-bin"]
    deb_path = debs_dir / os.path.basename(fname)
    if not deb_path.exists(): urllib.request.urlretrieve(f"{h_repo}/{fname}", deb_path)
    if hashlib.sha256(deb_path.read_bytes()).hexdigest() != checksum: raise RuntimeError(f"Bad SHA256: {fname}")
    (work / "host").mkdir(exist_ok=True)
    extract_deb(deb_path, work / "host")

for name in ("lib", "lib64", "lib32", "libx32"):
    old, new = sysroot / name, sysroot / "usr" / name
    if old.is_dir() and not old.is_symlink():
        shutil.copytree(old, new, dirs_exist_ok=True, symlinks=True)
        shutil.rmtree(old)
    if not old.is_symlink() and not old.exists() and new.is_dir():
        old.symlink_to("usr/" + name)

for directory, dirs, filenames in os.walk(sysroot, followlinks=False):
    for item in dirs + filenames:
        path = pathlib.Path(directory) / item
        if path.is_symlink() and os.readlink(path).startswith("/"):
            target = os.readlink(path).lstrip("/")
            path.unlink()
            path.symlink_to(os.path.relpath(sysroot / target, path.parent))
PY
}

describe_target() {
  local short="$1"

  case "$short" in
    x64)     printf '%s\n%s\n%s\n%s\n%s\n%s\n' "x86_64-unknown-linux-gnu" "x86_64-linux-gnu" "amd64" "trixie" "14" "amd64" ;;
    arm64)   printf '%s\n%s\n%s\n%s\n%s\n%s\n' "aarch64-unknown-linux-gnu" "aarch64-linux-gnu" "arm64" "trixie" "14" "arm64" ;;
    i386)    printf '%s\n%s\n%s\n%s\n%s\n%s\n' "i686-unknown-linux-gnu" "i686-linux-gnu" "i386" "trixie" "14" "i386" ;;
    ppc64le) printf '%s\n%s\n%s\n%s\n%s\n%s\n' "powerpc64le-unknown-linux-gnu" "powerpc64le-linux-gnu" "ppc64el" "trixie" "14" "ppc64el" ;;
    riscv64) printf '%s\n%s\n%s\n%s\n%s\n%s\n' "riscv64gc-unknown-linux-gnu" "riscv64-linux-gnu" "riscv64" "trixie" "14" "riscv64" ;;
    loong64) printf '%s\n%s\n%s\n%s\n%s\n%s\n' "loongarch64-unknown-linux-gnu" "loongarch64-linux-gnu" "loong64" "trixie" "14" "loong64" ;;
    s390x)   printf '%s\n%s\n%s\n%s\n%s\n%s\n' "s390x-unknown-linux-gnu" "s390x-linux-gnu" "s390x" "trixie" "14" "s390x" ;;
    *)       echo "Unknown arch '$short' (use ppc64le|i386|x64|arm64|loong64|riscv64|s390x)" >&2; return 1 ;;
  esac
}

publish_binary() {
  local short="$1"
  local rust_target="$2"
  local triple="$3"

  CARGO_TARGET_DIR="$(mktemp -d)"

  if [[ -z "$SDK_WORK_DIR" ]]; then
    CC="$(command -v clang)" CXX="$(command -v clang++)" \
    CARGO_BUILD_JOBS="$CARGO_BUILD_JOBS" \
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
      cargo build --manifest-path "$PROJECT" --locked --release --target "$rust_target"
    return 0
  fi

  local sysroot="$SDK_WORK_DIR/sysroot"
  local multiarch="$triple"
  [[ "$triple" == "i686-linux-gnu" ]] && multiarch="i386-linux-gnu"

  local gcc_dir=""
  gcc_dir="$(find "$sysroot/usr/lib/gcc" -name crtbeginS.o -print -quit)"
  [[ -n "$gcc_dir" ]] || die "crtbeginS.o not found in sysroot for $short"
  gcc_dir="${gcc_dir%/*}"

  local compiler=""
  for compiler in clang clang++; do
    cat > "$SDK_WORK_DIR/cc/$compiler" <<WRAPPER
#!/bin/sh
exec "$LLVM_TOOL_BIN/$compiler" --target=$triple --sysroot=$sysroot --gcc-install-dir=$gcc_dir -fuse-ld=lld -Qunused-arguments "\$@"
WRAPPER
    chmod +x "$SDK_WORK_DIR/cc/$compiler"
  done

  if [[ "$short" == loong64 ]]; then
    printf 'int main(void) { return 0; }\n' > "$SDK_WORK_DIR/smoke.c"
    "$SDK_WORK_DIR/cc/clang" "$SDK_WORK_DIR/smoke.c" -o "$SDK_WORK_DIR/smoke-c" || die "LoongArch Clang C link test failed"
    printf '#include <string>\nint main() { std::string s = "ok"; return s.size() != 2; }\n' > "$SDK_WORK_DIR/smoke.cpp"
    "$SDK_WORK_DIR/cc/clang++" "$SDK_WORK_DIR/smoke.cpp" -o "$SDK_WORK_DIR/smoke-cxx" || die "LoongArch Clang C++ link test failed"
    readelf -h "$SDK_WORK_DIR/smoke-c" | grep -q 'Machine:.*LoongArch' || die "LoongArch linker produced the wrong architecture"
    echo "[OK] LoongArch Clang cross-link tests passed"
  fi

  local archiver="$LLVM_TOOL_BIN/llvm-ar"
  local ranlib="$LLVM_TOOL_BIN/llvm-ranlib"

  local toolchain="$SDK_WORK_DIR/toolchain.cmake"
  cat > "$toolchain" <<CMAKE
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR ${triple%%-*})
set(CMAKE_SYSROOT $sysroot)
set(CMAKE_C_COMPILER $SDK_WORK_DIR/cc/clang)
set(CMAKE_CXX_COMPILER $SDK_WORK_DIR/cc/clang++)
set(CMAKE_AR $archiver)
set(CMAKE_RANLIB $ranlib)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
set(SDL_X11_SHARED ON CACHE BOOL "" FORCE)
set(SDL_WAYLAND_SHARED ON CACHE BOOL "" FORCE)
CMAKE

  local key="${rust_target//-/_}"
  local upper="${key^^}"

  env \
    PATH="$LLVM_TOOL_BIN:$SDK_WORK_DIR/host/usr/bin:$PATH" \
    CC="$LLVM_TOOL_BIN/clang" CXX="$LLVM_TOOL_BIN/clang++" \
    CARGO_BUILD_JOBS="$CARGO_BUILD_JOBS" \
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_SYSROOT_DIR="$sysroot" \
    PKG_CONFIG_LIBDIR="$sysroot/usr/lib/$multiarch/pkgconfig:$sysroot/usr/share/pkgconfig" \
    CMAKE_TOOLCHAIN_FILE="$toolchain" \
    "CC_${key}=$SDK_WORK_DIR/cc/clang" \
    "CXX_${key}=$SDK_WORK_DIR/cc/clang++" \
    "AR_${key}=$archiver" \
    "CARGO_TARGET_${upper}_LINKER=$SDK_WORK_DIR/cc/clang" \
    cargo build --manifest-path "$PROJECT" --locked --release --target "$rust_target"
}

verify_binary() {
  local short="$1"
  local rust_target="$2"
  local binary="$CARGO_TARGET_DIR/$rust_target/release/overink"
  local machine=""
  local header=""
  local config=""

  [[ -s "$binary" ]] || die "Missing compiled executable: $binary"
  case "$short" in
    x64) machine='X86-64' ;;
    i386) machine='80386' ;;
    arm64) machine='AArch64' ;;
    ppc64le) machine='PowerPC64' ;;
    riscv64) machine='RISC-V' ;;
    loong64) machine='LoongArch' ;;
    s390x) machine='S/390' ;;
  esac
  header="$(readelf -h "$binary")"
  grep -iq "Machine:.*$machine" <<< "$header" || die "Incorrect ELF machine for $short"
  if [[ "$short" == i386 ]]; then
    grep -q 'Class:.*ELF32' <<< "$header" || die "Expected ELF32 for i386"
  else
    grep -q 'Class:.*ELF64' <<< "$header" || die "Expected ELF64 for $short"
  fi
  [[ "${3:-0}" == 0 ]] || return 0
  config="$(find "$CARGO_TARGET_DIR/$rust_target/release/build" -name SDL_build_config.h -print -quit)"
  [[ -n "$config" ]] || die "Missing SDL build configuration"
  grep -q '^#define SDL_VIDEO_DRIVER_X11 1' "$config" || die "X11 backend is disabled"
  grep -q '^#define SDL_VIDEO_DRIVER_WAYLAND 1' "$config" || die "Wayland backend is disabled"
}

write_launcher_file() {
  local stage="$1"

  install -m 755 /dev/stdin "$stage/usr/bin/overink" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
DIR="/opt/OverInk"

if [[ -x "$DIR/overink" ]]; then
  exec "$DIR/overink" "$@"
fi

echo "OverInk launcher: no executable found in $DIR" >&2
ls -l "$DIR" >&2 || true
exit 1
EOF
}

write_desktop_file() {
  local stage="$1"

  install -m 644 /dev/stdin "$stage/usr/share/applications/overink.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=OverInk
Comment=Pressure-sensitive desktop annotation
Exec=overink
Icon=overink
Terminal=false
Categories=Graphics;Utility;
StartupWMClass=org.overink.OverInk
EOF
}

stage_runtime_assets() {
  local stage="$1"
  local rust_target="$2"
  local bin_path="$CARGO_TARGET_DIR/$rust_target/release/overink"

  [[ -f "$bin_path" ]] || die "Compiled binary not found: $bin_path"

  mkdir -p \
    "$stage/opt/OverInk" \
    "$stage/usr/bin" \
    "$stage/usr/share/applications" \
    "$stage/usr/share/icons/hicolor/256x256/apps" \
    "$stage/usr/share/icons/hicolor/scalable/apps" \
    "$stage/usr/share/gnome-shell/extensions/overink-overlay@overink"

  install -m 755 "$bin_path" "$stage/opt/OverInk/overink"
  install -m 644 "$SCRIPT_DIR/LICENSE" "$stage/opt/OverInk/LICENSE"
  install -m 644 "$SCRIPT_DIR/assets/fonts/OFL.txt" "$stage/opt/OverInk/OFL.txt"
  install -m 644 "$SCRIPT_DIR/assets/overink.png" "$stage/usr/share/icons/hicolor/256x256/apps/overink.png"
  install -m 644 "$SCRIPT_DIR/assets/overink.svg" "$stage/usr/share/icons/hicolor/scalable/apps/overink.svg"
  install -m 644 "$SCRIPT_DIR/assets/gnome-shell/extension.js" "$stage/usr/share/gnome-shell/extensions/overink-overlay@overink/extension.js"
  install -m 644 "$SCRIPT_DIR/assets/gnome-shell/metadata.json" "$stage/usr/share/gnome-shell/extensions/overink-overlay@overink/metadata.json"

  write_launcher_file "$stage"
  write_desktop_file "$stage"
}

write_package_metadata() {
  local debian_dir="$1"

  for hook in postinst postrm; do
    install -m 755 /dev/stdin "$debian_dir/$hook" <<'EOF'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database /usr/share/applications >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f /usr/share/icons/hicolor >/dev/null 2>&1 || true
fi
EOF
  done
}

package_binary() {
  local short="$1"
  local rust_target="$2"
  local pkg_arch="$3"
  local workdir=""
  local stage=""
  local debian_dir=""
  local binary=""
  local min_glibc=""
  local depends=""
  local needed=""
  local output=""

  workdir="$(mktemp -d)"
  stage="$workdir/$PKGROOT"
  debian_dir="$stage/DEBIAN"
  mkdir -p "$debian_dir" "$OUTPUT_DIR"
  stage_runtime_assets "$stage" "$rust_target"
  write_package_metadata "$debian_dir"

  binary="$stage/opt/OverInk/overink"
  min_glibc="$(readelf --version-info "$binary" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sed 's/GLIBC_//' | sort -V | tail -n1)"
  [[ -n "$min_glibc" ]] || die "Unable to determine glibc requirements for $short"
  depends="libc6 (>= $min_glibc), libx11-6, libxext6, libxi6, libxrandr2, libxcursor1, libxfixes3, libxrender1, libxtst6, libwayland-client0, libxkbcommon0, libegl1, libgl1, desktop-file-utils, hicolor-icon-theme"
  needed="$(readelf -d "$binary" | sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p')"
  if grep -qx 'libgcc_s.so.1' <<< "$needed"; then depends+=', libgcc-s1'; fi
  if grep -qx 'libstdc++.so.6' <<< "$needed"; then depends+=', libstdc++6'; fi
  if grep -qx 'libatomic.so.1' <<< "$needed"; then depends+=', libatomic1'; fi

  cat > "$debian_dir/control" <<EOF
Package: overink
Version: ${VERSION}
Architecture: ${pkg_arch}
Maintainer: JieXu <noreply@github.com>
Homepage: https://github.com/xujiegb/OverInk
Section: graphics
Priority: optional
Depends: ${depends}
Description: Pressure-sensitive desktop annotation with egui and native SDL3 input
 Supports native stylus pressure, multi-touch drawing, and click-through overlays.
EOF

  output="$OUTPUT_DIR/overink_${VERSION}_${pkg_arch}.deb"
  dpkg-deb --root-owner-group --build "$stage" "$output"
  [[ "$(dpkg-deb -f "$output" Architecture)" == "$pkg_arch" ]] || die "Wrong DEB architecture: $output"
  BUILT_DEBS+=("$output")
  echo "[OK] $output"
  rm -rf -- "$workdir"
}

select_targets() {
  case "${ARCH_OVERRIDE:-}" in
    all)                 printf '%s\n' x64 arm64 i386 ppc64le riscv64 loong64 s390x ;;
    x64|amd64|x86_64)    printf '%s\n' x64 ;;
    arm64|aarch64)       printf '%s\n' arm64 ;;
    i386|i686|x86)       printf '%s\n' i386 ;;
    ppc64le|ppc64el)     printf '%s\n' ppc64le ;;
    riscv64)             printf '%s\n' riscv64 ;;
    loong64|loongarch64) printf '%s\n' loong64 ;;
    s390x)               printf '%s\n' s390x ;;
    "")
      case "$HOST_ARCH" in
        x86_64)      printf '%s\n' x64 ;;
        aarch64)     printf '%s\n' arm64 ;;
        i686|i386)   printf '%s\n' i386 ;;
        ppc64le)     printf '%s\n' ppc64le ;;
        riscv64)     printf '%s\n' riscv64 ;;
        loongarch64) printf '%s\n' loong64 ;;
        s390x)       printf '%s\n' s390x ;;
        *)           return 1 ;;
      esac
      ;;
    *)
      echo "Unknown --arch '${ARCH_OVERRIDE}'. Use ppc64le|i386|x64|arm64|loong64|riscv64|s390x|all." >&2
      return 1
      ;;
  esac
}

build_one_target() {
  local short="$1"
  local meta=()
  local rust_target=""
  local triple=""
  local deb_arch=""
  local suite=""
  local gcc_ver=""
  local pkg_arch=""

  mapfile -t meta < <(describe_target "$short") || return 1
  rust_target="${meta[0]}"
  triple="${meta[1]}"
  deb_arch="${meta[2]}"
  suite="${meta[3]}"
  gcc_ver="${meta[4]}"
  pkg_arch="${meta[5]}"

  echo "[*] Building for target: $short  (target=$rust_target, arch=$pkg_arch)"
  if [[ -n "${OVERINK_ARCHIVE_DIR:-}" ]]; then
    local archive="$OVERINK_ARCHIVE_DIR/OverInk-$rust_target.tar.gz"
    [[ -s "$archive" ]] || die "Missing compiled archive: $archive"
    CARGO_TARGET_DIR="$(mktemp -d)"
    mkdir -p "$CARGO_TARGET_DIR/$rust_target/release"
    tar -xzf "$archive" -C "$CARGO_TARGET_DIR/$rust_target/release" overink
    verify_binary "$short" "$rust_target" 1
  else
    prepare_target_sdk "$short" "$rust_target" "$deb_arch" "$suite" "$gcc_ver"
    publish_binary "$short" "$rust_target" "$triple"
    verify_binary "$short" "$rust_target"
  fi
  package_binary "$short" "$rust_target" "$pkg_arch"

  [[ -z "$CARGO_TARGET_DIR" ]] || rm -rf -- "$CARGO_TARGET_DIR"
  [[ -z "$SDK_WORK_DIR" ]] || rm -rf -- "$SDK_WORK_DIR"
  CARGO_TARGET_DIR=""
  SDK_WORK_DIR=""
}

print_summary() {
  local pkg=""
  echo "================ DEB packages ================"
  for pkg in "${BUILT_DEBS[@]}"; do echo "$pkg"; done
}

main() {
  local targets=()
  local arch=""

  parse_args "$@"
  detect_environment
  install_dependencies
  prepare_workspace
  resolve_version

  mapfile -t targets < <(select_targets)
  (( ${#targets[@]} > 0 )) || die "No architectures selected"
  for arch in "${targets[@]}"; do
    build_one_target "$arch"
  done

  print_summary
}

main "$@"
