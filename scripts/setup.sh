#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DMG_PATH="${1:-${ROOT_DIR}/Codex.dmg}"
SKIP_APP_INSTALL="${SKIP_APP_INSTALL:-0}"

print_usage() {
  cat <<EOF
Usage:
  bash scripts/setup.sh /path/to/Codex.dmg
  bash scripts/setup.sh

Notes:
  - If no path is provided, script uses: ${ROOT_DIR}/Codex.dmg
  - Set SKIP_APP_INSTALL=1 to skip Linux app integration
EOF
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    return 1
  fi
}

echo "== Codex Desktop Linux setup =="

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  print_usage
  exit 0
fi

need_cmd node
need_cmd npm

if [[ ! -f "${DMG_PATH}" ]]; then
  echo "DMG not found: ${DMG_PATH}" >&2
  print_usage >&2
  exit 1
fi

if [[ "${SKIP_APP_INSTALL}" != "1" ]] && ! command -v codex >/dev/null 2>&1; then
  echo "The 'codex' CLI is not found on PATH." >&2
  echo "Install Codex CLI first, then rerun this script." >&2
  echo "Or run with SKIP_APP_INSTALL=1 for packaging-only flow." >&2
  exit 1
fi
if [[ "${SKIP_APP_INSTALL}" != "1" ]]; then
  export CODEX_CLI_SOURCE_PATH
  CODEX_CLI_SOURCE_PATH="$(readlink -f "$(command -v codex)")"
  export CODEX_CODE_MODE_HOST_SOURCE_PATH
  CODEX_CODE_MODE_HOST_SOURCE_PATH="$(dirname "${CODEX_CLI_SOURCE_PATH}")/codex-code-mode-host"
  if [[ ! -x "${CODEX_CODE_MODE_HOST_SOURCE_PATH}" ]]; then
    echo "Missing codex-code-mode-host beside ${CODEX_CLI_SOURCE_PATH}" >&2
    exit 1
  fi
fi

echo "[1/5] Installing npm dependencies..."
(
  cd "${ROOT_DIR}"
  npm ci --include=dev
  "${ROOT_DIR}/node_modules/node/bin/node" "${ROOT_DIR}/node_modules/electron/install.js"
)

echo "[2/5] Extracting app payload from DMG..."
bash "${ROOT_DIR}/scripts/internal/extract-dmg.sh" "${DMG_PATH}"
node "${ROOT_DIR}/build/linux-ui-patch.js" "${ROOT_DIR}/app_asar"

echo "[3/5] Rebuilding native modules..."
bash "${ROOT_DIR}/scripts/internal/build-native.sh"

echo "[3b/5] Building Wayland Computer Use helper..."
bash "${ROOT_DIR}/scripts/internal/build-computer-use.sh"

echo "[4/6] Running smoke check..."
"${ROOT_DIR}/node_modules/.bin/electron" --version >/dev/null

if [[ "${SKIP_APP_INSTALL}" == "1" ]]; then
  echo "[5/6] Skipped Linux app integration (SKIP_APP_INSTALL=1)."
else
  echo "[5/6] Installing Linux app command..."
  mkdir -p "${HOME}/.local/bin"
  {
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
    printf 'ROOT_DIR=%q\n' "${ROOT_DIR}"
    cat <<EOF
APP_DIR="\${ROOT_DIR}/app_asar"
if [[ ! -d "\${APP_DIR}" ]]; then
  echo "Missing \${APP_DIR}. Re-run: bash \${ROOT_DIR}/scripts/setup.sh /path/to/Codex.dmg" >&2
  exit 1
fi
if [[ -z "\${CODEX_CLI_PATH:-}" ]]; then
  if ! CODEX_CLI_PATH="\$(command -v codex)"; then
    echo "CODEX_CLI_PATH is not set and codex is not on PATH." >&2
    echo "Install Codex CLI first, or set CODEX_CLI_PATH=/path/to/codex." >&2
    exit 1
  fi
fi
ELECTRON_BIN="\${ROOT_DIR}/node_modules/.bin/electron"
if [[ ! -x "\${ELECTRON_BIN}" ]]; then
  echo "Missing project Electron runtime. Run npm install in \${ROOT_DIR}." >&2
  exit 1
fi
export ELECTRON_FORCE_IS_PACKAGED=1
export NODE_ENV=production
unset ELECTRON_RUN_AS_NODE
export CODEX_CLI_PATH
CODEX_ELECTRON_RESOURCES_PATH="\${ROOT_DIR}/app_resources"
CODEX_ELECTRON_BUNDLED_PLUGINS_RESOURCES_PATH="\${CODEX_ELECTRON_RESOURCES_PATH}"
CODEX_NODE_REPL_PATH="\${CODEX_ELECTRON_RESOURCES_PATH}/cua_node/bin/node_repl"
CODEX_BROWSER_USE_NODE_PATH="\${ROOT_DIR}/app_resources/cua_node/bin/node"
if [[ ! -f "\${CODEX_ELECTRON_BUNDLED_PLUGINS_RESOURCES_PATH}/plugins/openai-bundled/.agents/plugins/marketplace.json" || ! -x "\${CODEX_NODE_REPL_PATH}" || ! -x "\${CODEX_BROWSER_USE_NODE_PATH}" ]]; then
  echo "Missing Linux Browser Use runtime. Re-run setup." >&2
  exit 1
fi
export CODEX_ELECTRON_RESOURCES_PATH
export CODEX_ELECTRON_BUNDLED_PLUGINS_RESOURCES_PATH
export CODEX_NODE_REPL_PATH
export CODEX_BROWSER_USE_NODE_PATH
export CODEX_BROWSER_USE_DEFAULT_VIEWPORT_SIZE="\${CODEX_BROWSER_USE_DEFAULT_VIEWPORT_SIZE:-1280x800}"
EXTRA_ELECTRON_ARGS=(--class=codex-desktop)
for arg in "\$@"; do
  if [[ "\${arg}" == "--no-sandbox" || "\${arg}" == "--disable-gpu-sandbox" ]]; then
    echo "Refusing to launch Codex Desktop without Chromium sandboxing." >&2
    exit 1
  fi
done
# Wayland / X11 detection
if [[ "\${CODEX_USE_X11:-0}" == "1" ]]; then
  EXTRA_ELECTRON_ARGS+=(--ozone-platform=x11)
elif [[ "\${CODEX_USE_WAYLAND:-0}" == "1" ]]; then
  EXTRA_ELECTRON_ARGS+=(--ozone-platform=wayland --enable-features=WaylandWindowDecorations)
elif [[ -n "\${WAYLAND_DISPLAY:-}" ]]; then
  EXTRA_ELECTRON_ARGS+=(--ozone-platform=wayland --enable-features=WaylandWindowDecorations)
else
  EXTRA_ELECTRON_ARGS+=(--ozone-platform=x11)
fi
# Vulkan (only disable if explicitly requested)
if [[ "\${CODEX_DISABLE_VULKAN:-0}" == "1" ]]; then
  EXTRA_ELECTRON_ARGS+=(--disable-features=Vulkan)
fi
EXTRA_ELECTRON_ARGS+=(--password-store="\${CODEX_PASSWORD_STORE:-basic}")
# Stale SingletonLock cleanup
CONFIG_DIR="\${XDG_CONFIG_HOME:-\${HOME}/.config}"
for singleton_dir in "\${CONFIG_DIR}/Codex" "\${CONFIG_DIR}/Codex Desktop"; do
  singleton_lock="\${singleton_dir}/SingletonLock"
  if [[ -L "\${singleton_lock}" ]]; then
    lock_target="\$(readlink "\${singleton_lock}")"
    lock_pid="\${lock_target##*-}"
    if [[ -n "\${lock_pid}" && "\${lock_pid}" =~ ^[0-9]+$ ]] && ! kill -0 "\${lock_pid}" 2>/dev/null; then
      rm -f "\${singleton_lock}"
    fi
  fi
done
# --doctor flag
if [[ "\${1:-}" == "--doctor" ]]; then
  echo "=== Codex Desktop Doctor ==="
  echo ""
  echo "Display Server:"
  if [[ -n "\${WAYLAND_DISPLAY:-}" ]]; then
    echo "  Wayland: WAYLAND_DISPLAY=\${WAYLAND_DISPLAY}"
  elif [[ "\${XDG_SESSION_TYPE:-}" == "wayland" ]]; then
    echo "  Wayland (via XDG_SESSION_TYPE)"
  else
    echo "  X11"
  fi
  echo ""
  echo "GPU:"
  if [[ "\${CODEX_DISABLE_GPU:-0}" == "1" ]]; then
    echo "  GPU acceleration: DISABLED"
  else
    echo "  GPU acceleration: enabled"
  fi
  echo ""
  echo "CLI Path:"
  echo "  CODEX_CLI_PATH=\${CODEX_CLI_PATH}"
  echo ""
  echo "Platform:"
  echo "  \$(uname -a)"
  echo "  Arch: \$(uname -m)"
  echo ""
  echo "Electron:"
  echo "  ELECTRON_BIN=\${ELECTRON_BIN}"
  echo "  Version: \$("\${ELECTRON_BIN}" --version)"
  echo ""
  echo "Sandbox:"
  CHROME_SANDBOX_BIN="\${ROOT_DIR}/node_modules/electron/dist/chrome-sandbox"
  if [[ -f "\${CHROME_SANDBOX_BIN}" ]]; then
    sandbox_uid="\$(stat -c '%u' "\${CHROME_SANDBOX_BIN}")"
    sandbox_mode="\$(stat -c '%a' "\${CHROME_SANDBOX_BIN}")"
    echo "  chrome-sandbox uid=\${sandbox_uid} mode=\${sandbox_mode}"
    if [[ "\${sandbox_uid}" == "0" && "\${sandbox_mode}" == "4755" ]]; then
      echo "  Status: OK (setuid root)"
    else
      echo "  Status: NOT setuid root (sandbox disabled or not functional)"
    fi
  else
    echo "  chrome-sandbox: not found"
  fi
  exit 0
fi
exec "\${ELECTRON_BIN}" "\${EXTRA_ELECTRON_ARGS[@]}" "\${APP_DIR}" "\$@"
EOF
  } > "${HOME}/.local/bin/codex-desktop"
  chmod +x "${HOME}/.local/bin/codex-desktop"

  echo "[6/6] Installing Linux desktop application..."
  mkdir -p "${HOME}/.local/share/applications"
  rm -f "${HOME}/.local/share/applications/codex.desktop"
  cat > "${HOME}/.local/share/applications/codex-desktop.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Codex
Comment=Codex Desktop (Linux app)
Exec=${HOME}/.local/bin/codex-desktop %U
Terminal=false
Icon=codex
Categories=Development;
StartupNotify=true
StartupWMClass=codex-desktop
MimeType=x-scheme-handler/codex;
X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
EOF

  for icon_path in "${ROOT_DIR}"/assets/icons/linux/*x*.png; do
    icon_size="$(basename "${icon_path}" .png)"
    install -Dm644 \
      "${icon_path}" \
      "${HOME}/.local/share/icons/hicolor/${icon_size}/apps/codex.png"
  done
  gtk-update-icon-cache -f -t "${HOME}/.local/share/icons/hicolor"

  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "${HOME}/.local/share/applications"
  fi
  if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6
  fi

  echo "[+] Registering Wayland Computer Use MCP server..."
  bash "${ROOT_DIR}/scripts/register-computer-use.sh" "${ROOT_DIR}/app_resources/computer-use"
  install -Dm644 \
    "${ROOT_DIR}/skills/computer-use-linux/SKILL.md" \
    "${HOME}/.codex/skills/computer-use-linux/SKILL.md"
fi

echo
echo "Linux app setup complete."
echo "Run now: ${HOME}/.local/bin/codex-desktop"
echo "Or launch from app menu: Codex"
