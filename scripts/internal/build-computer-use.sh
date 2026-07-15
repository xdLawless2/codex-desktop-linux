#!/usr/bin/env bash
set -euo pipefail

# Build the Wayland-native Computer Use helpers (sky_wayland + sky_mcp) and
# stage them into app_resources so packaging and the local launcher can ship
# them. These implement the OpenAI Codex Linux Computer Use (@oai/sky)
# coordinate contract on top of the xdg-desktop-portal RemoteDesktop +
# ScreenCast interfaces (Wayland-native), replacing the upstream X11-only
# sky_linux backend.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CRATE_DIR="${ROOT_DIR}/native/sky-wayland"
DEST_DIR="${ROOT_DIR}/app_resources/computer-use"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo (Rust toolchain) is required to build the Computer Use helper." >&2
  exit 1
fi

echo "Building Wayland Computer Use helpers..."
(
  cd "${CRATE_DIR}"
  cargo build --release
)

mkdir -p "${DEST_DIR}"
install -Dm755 "${CRATE_DIR}/target/release/sky_wayland" "${DEST_DIR}/sky_wayland"
install -Dm755 "${CRATE_DIR}/target/release/sky_mcp" "${DEST_DIR}/sky_mcp"
install -Dm644 \
  "${ROOT_DIR}/skills/computer-use-linux/SKILL.md" \
  "${DEST_DIR}/SKILL.md"

echo "Computer Use helpers staged in ${DEST_DIR}"
