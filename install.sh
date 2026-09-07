#!/bin/sh
set -eu

PACKAGE='@obelisk-apps/cli'
REPO='eric8810/obelisk'
BIN_DIR="${OBELISK_BIN_DIR:-"$HOME/.local/bin"}"

usage() {
  cat >&2 <<'EOF'
Usage: install.sh [--npm]

  (default) Install the standalone Rust binary from GitHub Releases into
            OBELISK_BIN_DIR (default ~/.local/bin). No Node required.
  --npm     Install the npm-distributed wrapper (requires Node.js 22.13+).
EOF
}

mode='binary'
if [ "${1:-}" = '--npm' ]; then
  mode='npm'
elif [ $# -gt 0 ]; then
  usage
  exit 1
fi

install_binary() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Linux)  os_name='linux' ;;
    Darwin) os_name='macos' ;;
    *) echo "Unsupported OS: $os" >&2; exit 1 ;;
  esac
  case "$arch" in
    x86_64|amd64) arch_name='x86_64' ;;
    aarch64|arm64) arch_name='aarch64' ;;
    *) echo "Unsupported architecture: $arch" >&2; exit 1 ;;
  esac
  if [ "$os_name" = 'macos' ] && [ "$arch_name" = 'x86_64' ]; then
    # Rosetta 2 runs the aarch64 build; the x86_64 artifact stays available
    # for native use under Rosetta-less environments.
    artifact="obelisk-macos-x86_64.zip"
    extract='unzip -o'
    inner='obelisk.exe.dSYM 2>/dev/null || true; obelisk.exe'
  else
    artifact="obelisk-${os_name}-${arch_name}.tar.gz"
    extract='tar -xzf'
    inner='obelisk'
  fi

  if ! command -v curl >/dev/null 2>&1; then
    echo 'Binary installation requires curl.' >&2
    exit 1
  fi

  echo "Fetching the latest Obelisk binary release for ${os_name}-${arch_name}..."
  latest=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  if [ -z "$latest" ]; then
    echo 'Unable to resolve the latest release.' >&2
    exit 1
  fi
  base="https://github.com/${REPO}/releases/download/${latest}"
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  echo "Downloading ${artifact} (${latest})..."
  curl -fsSL -o "$tmp/$artifact" "$base/$artifact"
  curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || true
  if [ -f "$tmp/SHA256SUMS" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      (cd "$tmp" && grep " $artifact\$" SHA256SUMS | sha256sum -c -) || {
        echo 'Checksum verification failed.' >&2
        exit 1
      }
    elif command -v shasum >/dev/null 2>&1; then
      (cd "$tmp" && grep " $artifact\$" SHA256SUMS | shasum -a 256 -c -) || {
        echo 'Checksum verification failed.' >&2
        exit 1
      }
    fi
  fi
  (cd "$tmp" && $extract "$artifact")
  mkdir -p "$BIN_DIR"
  mv "$tmp/obelisk" "$BIN_DIR/obelisk" 2>/dev/null || mv "$tmp/obelisk.exe" "$BIN_DIR/obelisk" 2>/dev/null || {
    echo 'Unexpected archive layout.' >&2
    exit 1
  }
  chmod +x "$BIN_DIR/obelisk"
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Note: $BIN_DIR is not on PATH; add it to use \`obelisk\`." >&2 ;;
  esac
  "$BIN_DIR/obelisk" --version
  echo "Obelisk CLI installed at $BIN_DIR/obelisk. Run \`obelisk install\` to install the agent skill."
}

install_npm() {
  if ! command -v node >/dev/null 2>&1; then
    echo 'Obelisk requires Node.js 22.13 or newer (or re-run with --binary for the standalone install).' >&2
    exit 1
  fi

  if ! node -e "const [major, minor] = process.versions.node.split('.').map(Number); process.exit(major > 22 || (major === 22 && minor >= 13) ? 0 : 1)"; then
    echo 'Obelisk requires Node.js 22.13 or newer.' >&2
    exit 1
  fi

  if ! command -v npm >/dev/null 2>&1; then
    echo 'Obelisk installation requires npm.' >&2
    exit 1
  fi

  echo "Installing ${PACKAGE}..."
  npm install --global "$PACKAGE"

  if ! command -v obelisk >/dev/null 2>&1; then
    echo 'The CLI was installed, but `obelisk` is not on PATH.' >&2
    echo 'Add the npm global bin directory to PATH, then run `obelisk --version`.' >&2
    exit 1
  fi

  obelisk --version
  echo 'Obelisk CLI installed. Run `obelisk install` to install the agent skill.'
}

if [ "$mode" = 'binary' ]; then
  install_binary
else
  install_npm
fi
