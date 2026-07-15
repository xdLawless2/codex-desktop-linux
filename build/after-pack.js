const fs = require('fs');
const path = require('path');
const { patchLinuxUi } = require('./linux-ui-patch');

module.exports = async function afterPack(context) {
  if (context.electronPlatformName !== 'linux') {
    return;
  }

  const appOutDir = context.appOutDir;
  const executableName = context.packager.platformSpecificBuildOptions.executableName || 'codex-desktop';
  const executablePath = path.join(appOutDir, executableName);
  const binaryPath = `${executablePath}.bin`;

  if (!fs.existsSync(executablePath)) {
    throw new Error(`Expected Electron executable not found: ${executablePath}`);
  }

  if (!fs.existsSync(binaryPath)) {
    fs.renameSync(executablePath, binaryPath);
  }

  const wrapper = `#!/usr/bin/env bash
set -euo pipefail

APP_DIR="$(cd "$(dirname "$(readlink -f "\${BASH_SOURCE[0]}")")" && pwd)"
ELECTRON_BIN="\${APP_DIR}/__EXECUTABLE_NAME__.bin"

# --- Config home resolution ---
XDG_CONFIG_HOME="\${XDG_CONFIG_HOME:-\${HOME}/.config}"

# --- SingletonLock stale cleanup ---
cleanup_singleton_lock() {
  local config_home="\${XDG_CONFIG_HOME}"
  local lock_found=""
  for candidate in "\${config_home}/Codex/SingletonLock" "\${config_home}/Codex Desktop/SingletonLock"; do
    if [[ -L "\${candidate}" ]]; then
      lock_found="\${candidate}"
      break
    fi
  done
  if [[ -n "\${lock_found}" ]]; then
    local target
    target="$(readlink "\${lock_found}")" || return 0
    local pid
    pid="\${target##*-}"
    if [[ "\${pid}" =~ ^[0-9]+$ ]]; then
      if ! kill -0 "\${pid}" 2>/dev/null; then
        rm -f "\${lock_found}"
      fi
    fi
  fi
}

# --- Doctor diagnostic ---
run_doctor() {
  echo "=== Codex Desktop Doctor Report ==="
  echo ""

  # Display server
  echo "--- Display Server ---"
  if [[ -n "\${WAYLAND_DISPLAY:-}" ]]; then
    echo "  Wayland detected: WAYLAND_DISPLAY=\${WAYLAND_DISPLAY}"
  else
    echo "  Wayland: not detected (WAYLAND_DISPLAY unset)"
  fi
  if [[ -n "\${XDG_SESSION_TYPE:-}" ]]; then
    echo "  XDG_SESSION_TYPE=\${XDG_SESSION_TYPE}"
  else
    echo "  XDG_SESSION_TYPE: unset"
  fi
  echo "  CODEX_USE_X11=\${CODEX_USE_X11:-unset}"
  echo "  CODEX_USE_WAYLAND=\${CODEX_USE_WAYLAND:-unset}"
  echo ""

  # GPU info
  echo "--- GPU ---"
  echo "  CODEX_DISABLE_GPU=\${CODEX_DISABLE_GPU:-unset}"
  echo ""

  # Sandbox
  echo "--- Sandbox ---"
  local sandbox="\${APP_DIR}/chrome-sandbox"
  if [[ -e "\${sandbox}" ]]; then
    local sandbox_perms
    sandbox_perms="$(stat -c '%a' "\${sandbox}" 2>/dev/null || echo 'unknown')"
    echo "  chrome-sandbox path: \${sandbox}"
    echo "  chrome-sandbox permissions: \${sandbox_perms}"
    echo "  chrome-sandbox owner: $(stat -c '%U:%G' "\${sandbox}" 2>/dev/null || echo 'unknown')"
  else
    echo "  chrome-sandbox: not found at \${sandbox}"
  fi
  echo ""

  # CLI resolution
  echo "--- CLI ---"
  echo "  Bundled CLI: \${APP_DIR}/resources/codex"
  echo ""

  # Platform
  echo "--- Platform ---"
  echo "  OS: $(uname -s)"
  echo "  Arch: $(uname -m)"
  echo "  Kernel: $(uname -r)"
  echo ""

  # Electron
  echo "--- Electron ---"
  echo "  Binary: \${ELECTRON_BIN}"
  if [[ -x "\${ELECTRON_BIN}" ]]; then
    echo "  Status: executable"
  else
    echo "  Status: MISSING or not executable"
  fi
  echo ""
  echo "=== End of Report ==="
}

# --- Handle --doctor flag ---
for arg in "$@"; do
  if [[ "\${arg}" == "--doctor" ]]; then
    run_doctor
    exit 0
  fi
done

CODEX_CLI_PATH="\${APP_DIR}/resources/codex"
if [[ ! -x "\${CODEX_CLI_PATH}" ]]; then
  echo "Missing bundled Linux Codex CLI: \${CODEX_CLI_PATH}" >&2
  exit 127
fi

export CODEX_CLI_PATH
export NODE_ENV="\${NODE_ENV:-production}"
export ELECTRON_FORCE_IS_PACKAGED="\${ELECTRON_FORCE_IS_PACKAGED:-1}"

extra_args=()

for arg in "$@"; do
  if [[ "\${arg}" == "--no-sandbox" || "\${arg}" == "--disable-gpu-sandbox" ]]; then
    echo "Refusing to launch Codex Desktop without Chromium sandboxing." >&2
    exit 1
  fi
done

# --- Display server / Wayland ---
if [[ "\${CODEX_USE_X11:-}" == "1" ]]; then
  extra_args+=(--ozone-platform=x11)
elif [[ "\${CODEX_USE_WAYLAND:-}" == "1" ]]; then
  extra_args+=(--ozone-platform=wayland --enable-features=WaylandWindowDecorations)
elif [[ -n "\${WAYLAND_DISPLAY:-}" ]]; then
  extra_args+=(--ozone-platform=wayland --enable-features=WaylandWindowDecorations)
else
  extra_args+=(--ozone-platform=x11)
fi
# --- Vulkan: only disable if explicitly requested ---
if [[ "\${CODEX_DISABLE_VULKAN:-}" == "1" ]]; then
  extra_args+=(--disable-features=Vulkan)
fi

# --- Password store ---
if [[ -n "\${CODEX_PASSWORD_STORE:-}" ]]; then
  extra_args+=(--password-store="\${CODEX_PASSWORD_STORE}")
else
  extra_args+=(--password-store=basic)
fi

# --- Register the Wayland Computer Use MCP server (idempotent) ---
CU_DIR="\${APP_DIR}/resources/computer-use"
if [[ -x "\${CU_DIR}/sky_mcp" ]]; then
  if [[ -f "\${CU_DIR}/SKILL.md" ]]; then
    install -Dm644 \
      "\${CU_DIR}/SKILL.md" \
      "\${HOME}/.codex/skills/computer-use-linux/SKILL.md"
  fi
  if ! command -v busctl >/dev/null 2>&1; then
    echo "busctl is required to enable Linux accessibility." >&2
    exit 1
  fi
  for variable in XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS WAYLAND_DISPLAY; do
    if [[ -z "\${!variable:-}" ]]; then
      echo "\${variable} is required to register Wayland Computer Use." >&2
      exit 1
    fi
  done
  busctl --user set-property \
    org.a11y.Bus /org/a11y/bus org.a11y.Status ScreenReaderEnabled b true
  busctl --user set-property \
    org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true
  install -Dm644 /dev/stdin \
    "\${HOME}/.local/share/applications/org.openai.CodexComputerUse.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Codex Computer Use
Exec="\${CU_DIR}/sky_mcp"
NoDisplay=true
OnlyShowIn=KDE;
X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
EOF
  if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6
  fi
  if "\${CODEX_CLI_PATH}" mcp get computer-use >/dev/null 2>&1; then
    "\${CODEX_CLI_PATH}" mcp remove computer-use >/dev/null
  fi
  if "\${CODEX_CLI_PATH}" mcp get computer-use-linux >/dev/null 2>&1; then
    "\${CODEX_CLI_PATH}" mcp remove computer-use-linux >/dev/null
  fi
  if "\${CODEX_CLI_PATH}" mcp get computer_use_linux >/dev/null 2>&1; then
    "\${CODEX_CLI_PATH}" mcp remove computer_use_linux >/dev/null
  fi
  mcp_env=(
    --env "SKY_WAYLAND_BIN=\${CU_DIR}/sky_wayland"
    --env "XDG_RUNTIME_DIR=\${XDG_RUNTIME_DIR}"
    --env "DBUS_SESSION_BUS_ADDRESS=\${DBUS_SESSION_BUS_ADDRESS}"
    --env "WAYLAND_DISPLAY=\${WAYLAND_DISPLAY}"
  )
  for variable in DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE; do
    if [[ -n "\${!variable:-}" ]]; then
      mcp_env+=(--env "\${variable}=\${!variable}")
    fi
  done
  "\${CODEX_CLI_PATH}" mcp add computer_use_linux \
    "\${mcp_env[@]}" -- "\${CU_DIR}/sky_mcp" >/dev/null
fi

# --- Clean stale SingletonLock before launch ---
cleanup_singleton_lock

exec "\${ELECTRON_BIN}" "\${extra_args[@]}" "$@"
`.replace(/__EXECUTABLE_NAME__/g, executableName);

  fs.writeFileSync(executablePath, wrapper, { mode: 0o755 });
  fs.chmodSync(binaryPath, 0o755);

  patchLinuxUi(path.join(appOutDir, 'resources', 'app'));

  // Normalize permissions: dirs 755, files 644, executables 755
};
