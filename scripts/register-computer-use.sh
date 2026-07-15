#!/usr/bin/env bash
set -euo pipefail

# Register the Wayland Computer Use helper as a Codex MCP server so the desktop
# app (and CLI) expose desktop-control tools. Use Codex's own config command so
# subsequent desktop settings writes preserve the server.

HELPER_DIR="$(realpath "${1:?usage: register-computer-use.sh <helper-dir>}")"
SKY_MCP="${HELPER_DIR}/sky_mcp"
SKY_WAYLAND="${HELPER_DIR}/sky_wayland"
SCREENSHOT_DESKTOP_ENTRY="${HOME}/.local/share/applications/org.openai.CodexComputerUse.desktop"

if [[ ! -x "${SKY_MCP}" || ! -x "${SKY_WAYLAND}" ]]; then
  echo "Computer Use helpers not found in ${HELPER_DIR}" >&2
  exit 1
fi

if ! command -v codex >/dev/null 2>&1; then
  echo "The codex CLI is required to register Computer Use." >&2
  exit 1
fi

if ! command -v busctl >/dev/null 2>&1; then
  echo "busctl is required to enable Linux accessibility." >&2
  exit 1
fi

for variable in XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS WAYLAND_DISPLAY; do
  if [[ -z "${!variable:-}" ]]; then
    echo "${variable} is required to register Wayland Computer Use." >&2
    exit 1
  fi
done

# Chromium/CEF applications only publish complete AT-SPI trees when the
# session advertises an active assistive technology. These properties persist
# through the desktop accessibility settings and are also refreshed by the
# helper at runtime.
busctl --user set-property \
  org.a11y.Bus /org/a11y/bus org.a11y.Status ScreenReaderEnabled b true
busctl --user set-property \
  org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true

# KWin ScreenShot2 authorizes the executable that owns the D-Bus connection by
# matching it to a desktop entry with this restricted-interface declaration.
install -Dm644 /dev/stdin "${SCREENSHOT_DESKTOP_ENTRY}" <<EOF
[Desktop Entry]
Type=Application
Name=Codex Computer Use
Exec="${SKY_MCP}"
NoDisplay=true
OnlyShowIn=KDE;
X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
EOF
if command -v kbuildsycoca6 >/dev/null 2>&1; then
  kbuildsycoca6
fi

if codex mcp get computer-use >/dev/null 2>&1; then
  codex mcp remove computer-use
fi
if codex mcp get computer-use-linux >/dev/null 2>&1; then
  codex mcp remove computer-use-linux
fi
if codex mcp get computer_use_linux >/dev/null 2>&1; then
  codex mcp remove computer_use_linux
fi
MCP_ENV=(
  --env "SKY_WAYLAND_BIN=${SKY_WAYLAND}"
  --env "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR}"
  --env "DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS}"
  --env "WAYLAND_DISPLAY=${WAYLAND_DISPLAY}"
)
for variable in DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE; do
  if [[ -n "${!variable:-}" ]]; then
    MCP_ENV+=(--env "${variable}=${!variable}")
  fi
done
codex mcp add computer_use_linux "${MCP_ENV[@]}" -- "${SKY_MCP}"

echo "Registered Computer Use with Codex."
