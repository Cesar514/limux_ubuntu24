#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TOOLS_DIR="${ROOT_DIR}/.tools"
TOOLS_BIN_DIR="${TOOLS_DIR}/bin"
HOST_PYTHON="/usr/bin/python3"
BLUEPRINT_VERSION="${LIMUX_BLUEPRINT_COMPILER_VERSION:-0.20.4}"
BLUEPRINT_VENV_DIR="${TOOLS_DIR}/blueprint-venv"
RUN_CHECKS=false
SKIP_SYSTEM_DEPS=false

usage() {
    cat <<'EOF'
Usage: scripts/bootstrap_ubuntu24.sh [--with-checks] [--skip-system-deps]

Bootstraps an Ubuntu 24.04 checkout for Limux, provisions repo-local Zig and
Blueprint compiler tooling, and builds the release artifacts in dist/.

Options:
  --with-checks       Run ./scripts/check.sh after packaging succeeds.
  --skip-system-deps  Refuse to install missing apt packages automatically.
  -h, --help          Show this help text.
EOF
}

log() {
    printf '==> %s\n' "$1"
}

fail() {
    printf 'ERROR: %s\n' "$1" >&2
    exit 1
}

for arg in "$@"; do
    case "$arg" in
        --with-checks) RUN_CHECKS=true ;;
        --skip-system-deps) SKIP_SYSTEM_DEPS=true ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument: ${arg}"
            ;;
    esac
done

require_ubuntu_24() {
    if [ ! -f /etc/os-release ]; then
        fail "/etc/os-release is missing; cannot verify Ubuntu 24.04."
    fi

    # shellcheck disable=SC1091
    . /etc/os-release

    if [ "${ID:-}" != "ubuntu" ] || [ "${VERSION_ID:-}" != "24.04" ]; then
        fail "scripts/bootstrap_ubuntu24.sh only supports Ubuntu 24.04.x. Found ${PRETTY_NAME:-unknown}."
    fi
}

package_installed() {
    dpkg-query -W -f='${Status}\n' "$1" 2>/dev/null | grep -qx 'install ok installed'
}

ensure_system_packages() {
    local packages=(
        build-essential
        curl
        git
        pkg-config
        python3-gi
        python3-venv
        libadwaita-1-dev
        libbz2-dev
        libepoxy-dev
        libgtk-4-dev
        libwebkitgtk-6.0-dev
        libxml2-utils
    )
    local missing=()
    local pkg

    for pkg in "${packages[@]}"; do
        if ! package_installed "$pkg"; then
            missing+=("$pkg")
        fi
    done

    if [ "${#missing[@]}" -eq 0 ]; then
        log "System packages already satisfied."
        return 0
    fi

    if $SKIP_SYSTEM_DEPS; then
        fail "missing required Ubuntu packages: ${missing[*]}. Re-run without --skip-system-deps or install them manually."
    fi

    if ! command -v sudo >/dev/null 2>&1; then
        fail "sudo is required to install missing Ubuntu packages: ${missing[*]}"
    fi

    log "Installing missing Ubuntu packages: ${missing[*]}"
    sudo apt-get update
    sudo apt-get install -y "${missing[@]}"
}

ensure_ghostty_submodule() {
    if [ ! -f "${ROOT_DIR}/ghostty/build.zig" ]; then
        log "Initializing Ghostty submodule..."
        git -C "${ROOT_DIR}" submodule update --init --recursive
    fi
}

ensure_rust_toolchain() {
    if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
        log "Rust toolchain already available."
        if $RUN_CHECKS && command -v rustup >/dev/null 2>&1; then
            rustup component add rustfmt clippy >/dev/null
        fi
        return 0
    fi

    if ! command -v curl >/dev/null 2>&1; then
        fail "curl is required to install the Rust toolchain."
    fi

    log "Installing Rust toolchain via rustup..."
    curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
    export PATH="${HOME}/.cargo/bin:${PATH}"
    if $RUN_CHECKS; then
        rustup component add rustfmt clippy >/dev/null
    fi
}

read_zig_version() {
    local version

    version="$(sed -n -E 's/^[[:space:]]*\.minimum_zig_version = "([^"]+)".*/\1/p' "${ROOT_DIR}/ghostty/build.zig.zon" | head -n1)"
    if [ -z "${version}" ]; then
        fail "unable to read Ghostty minimum Zig version from ghostty/build.zig.zon"
    fi

    printf '%s\n' "${version}"
}

read_zig_arch() {
    case "$(uname -m)" in
        x86_64) printf 'x86_64\n' ;;
        aarch64) printf 'aarch64\n' ;;
        *) fail "unsupported architecture: $(uname -m)" ;;
    esac
}

ensure_zig() {
    local zig_version
    local zig_arch
    local zig_dir
    local zig_tarball
    local zig_url

    zig_version="$(read_zig_version)"
    zig_arch="$(read_zig_arch)"
    zig_dir="${TOOLS_DIR}/zig-${zig_arch}-linux-${zig_version}"
    zig_tarball="${TOOLS_DIR}/zig-${zig_arch}-linux-${zig_version}.tar.xz"
    zig_url="https://ziglang.org/download/${zig_version}/zig-${zig_arch}-linux-${zig_version}.tar.xz"

    mkdir -p "${TOOLS_DIR}"

    if [ -x "${zig_dir}/zig" ] && [ "$("${zig_dir}/zig" version)" = "${zig_version}" ]; then
        log "Using repo-local Zig ${zig_version}."
    else
        log "Downloading Zig ${zig_version}..."
        rm -rf "${zig_dir}"
        curl -fsSL "${zig_url}" -o "${zig_tarball}"
        tar -xf "${zig_tarball}" -C "${TOOLS_DIR}"
        rm -f "${zig_tarball}"
    fi

    if [ ! -x "${zig_dir}/zig" ]; then
        fail "Zig ${zig_version} was not provisioned correctly at ${zig_dir}/zig"
    fi

    ZIG_DIR="${zig_dir}"
}

ensure_blueprint_compiler() {
    local wrapper_path

    if [ ! -x "${HOST_PYTHON}" ]; then
        fail "required system Python interpreter not found at ${HOST_PYTHON}"
    fi

    log "Provisioning repo-local blueprint-compiler ${BLUEPRINT_VERSION}..."
    mkdir -p "${TOOLS_BIN_DIR}"

    if [ ! -d "${BLUEPRINT_VENV_DIR}" ]; then
        "${HOST_PYTHON}" -m venv --system-site-packages "${BLUEPRINT_VENV_DIR}"
    fi

    "${BLUEPRINT_VENV_DIR}/bin/python" -m pip install \
        --disable-pip-version-check \
        --quiet \
        --upgrade \
        --force-reinstall \
        "blueprint-compiler==${BLUEPRINT_VERSION}"

    wrapper_path="${TOOLS_BIN_DIR}/blueprint-compiler"
    cat > "${wrapper_path}" <<EOF
#!/usr/bin/python3
import os
import sys

PINNED_VERSION = "${BLUEPRINT_VERSION}"
TOOLS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SITE_PACKAGES = os.path.join(
    TOOLS_DIR,
    "blueprint-venv",
    "lib",
    f"python{sys.version_info.major}.{sys.version_info.minor}",
    "site-packages",
)

if not os.path.isdir(SITE_PACKAGES):
    raise SystemExit(f"FATAL: missing blueprint site-packages at {SITE_PACKAGES}")

sys.path.insert(0, SITE_PACKAGES)

if len(sys.argv) > 1 and sys.argv[1] == "--version":
    print(PINNED_VERSION)
    raise SystemExit(0)

from blueprintcompiler.main import BlueprintApp

BlueprintApp().main()
EOF
    chmod 755 "${wrapper_path}"

    if [ "$("${wrapper_path}" --version)" != "${BLUEPRINT_VERSION}" ]; then
        fail "blueprint-compiler wrapper did not report the expected version ${BLUEPRINT_VERSION}"
    fi
}

build_path() {
    local path="${TOOLS_BIN_DIR}:${ZIG_DIR}:/usr/bin:/bin"

    if [ -d "${HOME}/.cargo/bin" ]; then
        path="${path}:${HOME}/.cargo/bin"
    fi

    if [ -n "${PATH:-}" ]; then
        path="${path}:${PATH}"
    fi

    printf '%s\n' "${path}"
}

run_builds() {
    local path

    path="$(build_path)"

    log "Building Limux release artifacts..."
    env PATH="${path}" ./scripts/package.sh

    if $RUN_CHECKS; then
        log "Running repository checks..."
        env PATH="${path}" ./scripts/check.sh
    fi
}

main() {
    require_ubuntu_24
    ensure_system_packages
    ensure_ghostty_submodule
    ensure_rust_toolchain
    ensure_zig
    ensure_blueprint_compiler
    run_builds

    log "Artifacts available in ${ROOT_DIR}/dist"
}

main
