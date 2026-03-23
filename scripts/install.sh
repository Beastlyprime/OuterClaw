#!/usr/bin/env bash
#
# OuterClaw installer — download the latest release binary.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/user/outerclaw/main/scripts/install.sh | sudo bash
#   curl -fsSL ... | sudo bash -s -- --setup   # also run 'outerclaw setup'
#
set -euo pipefail

REPO="user/outerclaw"
INSTALL_DIR="/usr/local/bin"
BINARY_NAME="outerclaw"
RUN_SETUP=false

# ── Parse flags ──────────────────────────────────────────────────────────
for arg in "$@"; do
  case "$arg" in
    --setup) RUN_SETUP=true ;;
    *) echo "Unknown flag: $arg"; exit 1 ;;
  esac
done

# ── Detect OS & arch ────────────────────────────────────────────────────
detect_platform() {
  local os arch target

  os="$(uname -s)"
  arch="$(uname -m)"

  if [ "$os" != "Linux" ]; then
    echo "Error: OuterClaw only supports Linux (detected: $os)" >&2
    exit 1
  fi

  case "$arch" in
    x86_64|amd64)   target="x86_64-unknown-linux-musl" ;;
    aarch64|arm64)   target="aarch64-unknown-linux-musl" ;;
    *)
      echo "Error: Unsupported architecture: $arch" >&2
      echo "Supported: x86_64 (amd64), aarch64 (arm64)" >&2
      exit 1
      ;;
  esac

  echo "$target"
}

# ── Fetch latest release tag from GitHub API ────────────────────────────
latest_tag() {
  local tag
  if command -v curl &>/dev/null; then
    tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
           | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
  elif command -v wget &>/dev/null; then
    tag="$(wget -qO- "https://api.github.com/repos/${REPO}/releases/latest" \
           | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
  else
    echo "Error: curl or wget is required" >&2
    exit 1
  fi

  if [ -z "$tag" ]; then
    echo "Error: Could not determine latest release tag" >&2
    exit 1
  fi
  echo "$tag"
}

# ── Download helper ─────────────────────────────────────────────────────
download() {
  local url="$1" dest="$2"
  if command -v curl &>/dev/null; then
    curl -fsSL -o "$dest" "$url"
  else
    wget -qO "$dest" "$url"
  fi
}

# ── Main ────────────────────────────────────────────────────────────────
main() {
  echo "=== OuterClaw Installer ==="
  echo

  # Must run as root for /usr/local/bin
  if [ "$(id -u)" -ne 0 ]; then
    echo "Error: This script must be run as root (use sudo)" >&2
    exit 1
  fi

  local target tag base_url binary_url checksum_url tmpdir

  target="$(detect_platform)"
  echo "Detected platform: $target"

  tag="$(latest_tag)"
  echo "Latest release:    $tag"

  base_url="https://github.com/${REPO}/releases/download/${tag}"
  binary_url="${base_url}/${BINARY_NAME}-${target}"
  checksum_url="${base_url}/${BINARY_NAME}-${target}.sha256"

  tmpdir="$(mktemp -d)"
  trap 'rm -rf "$tmpdir"' EXIT

  echo
  echo "Downloading binary..."
  download "$binary_url" "${tmpdir}/${BINARY_NAME}"

  echo "Downloading checksum..."
  download "$checksum_url" "${tmpdir}/${BINARY_NAME}.sha256"

  echo "Verifying checksum..."
  (
    cd "$tmpdir"
    # The .sha256 file contains: <hash>  outerclaw
    # Rewrite it to match the local filename
    expected_hash="$(awk '{print $1}' "${BINARY_NAME}.sha256")"
    actual_hash="$(sha256sum "${BINARY_NAME}" | awk '{print $1}')"
    if [ "$expected_hash" != "$actual_hash" ]; then
      echo "Error: Checksum mismatch!" >&2
      echo "  Expected: $expected_hash" >&2
      echo "  Actual:   $actual_hash" >&2
      exit 1
    fi
    echo "Checksum OK: $expected_hash"
  )

  echo
  echo "Installing to ${INSTALL_DIR}/${BINARY_NAME}..."
  install -m 0755 "${tmpdir}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"

  echo "Installed: $(${INSTALL_DIR}/${BINARY_NAME} --version 2>/dev/null || echo "${BINARY_NAME}")"

  if [ "$RUN_SETUP" = true ]; then
    echo
    echo "Running setup..."
    "${INSTALL_DIR}/${BINARY_NAME}" setup
  fi

  echo
  echo "Done. Run 'outerclaw --help' to get started."
}

main
