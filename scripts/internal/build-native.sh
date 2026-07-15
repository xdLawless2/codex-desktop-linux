#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
APP_DIR="${APP_DIR:-${ROOT_DIR}/app_asar}"
BUILD_DIR="${BUILD_DIR:-${ROOT_DIR}/build_native}"
NATIVE_ARCH="${NATIVE_ARCH:-${npm_config_arch:-$(node -p "process.arch")}}"

if [[ ! -d "${APP_DIR}" ]]; then
  echo "Missing ${APP_DIR}. Run scripts/setup.sh first." >&2
  exit 1
fi

if [[ ! -d "${ROOT_DIR}/node_modules" ]]; then
  echo "Missing local node_modules. Run: npm install" >&2
  exit 1
fi

ELECTRON_VERSION="${ELECTRON_VERSION:-}"
if [[ -z "${ELECTRON_VERSION}" ]]; then
  ELECTRON_VERSION="$(node -e "
const app = require(process.argv[1]);
const installedVersion = require(process.argv[2]).version;
const requiredVersion = app.devDependencies.electron;
if (installedVersion !== requiredVersion) {
  throw new Error(\`Electron version mismatch: upstream requires \${requiredVersion}, installed \${installedVersion}\`);
}
console.log(installedVersion);
" "${APP_DIR}/package.json" "${ROOT_DIR}/node_modules/electron/package.json")"
fi

BETTER_SQLITE3_VERSION="$(node -p "require('${ROOT_DIR}/node_modules/better-sqlite3/package.json').version")"
NODE_PTY_VERSION="$(node -p "require('${APP_DIR}/node_modules/node-pty/package.json').version")"

mkdir -p "${BUILD_DIR}"
if [[ ! -f "${BUILD_DIR}/package.json" ]]; then
  (
    cd "${BUILD_DIR}"
    npm init -y >/dev/null
  )
fi

echo "Installing native build dependencies into ${BUILD_DIR}..."
(
  cd "${BUILD_DIR}"
  npm install \
    "electron@${ELECTRON_VERSION}" \
    "better-sqlite3@${BETTER_SQLITE3_VERSION}" \
    "node-pty@${NODE_PTY_VERSION}" \
    "@electron/rebuild"
)

echo "Rebuilding better-sqlite3 and node-pty for Electron ${ELECTRON_VERSION} (${NATIVE_ARCH})..."
(
  cd "${BUILD_DIR}"
  npx electron-rebuild -v "${ELECTRON_VERSION}" -a "${NATIVE_ARCH}" -f --build-from-source -w better-sqlite3,node-pty
)

echo "Copying rebuilt native binaries into app_asar..."
mkdir -p "${APP_DIR}/node_modules/better-sqlite3/build/Release"
cp -f "${BUILD_DIR}/node_modules/better-sqlite3/build/Release/better_sqlite3.node" \
  "${APP_DIR}/node_modules/better-sqlite3/build/Release/better_sqlite3.node"
cp -f "${BUILD_DIR}/node_modules/node-pty/build/Release/pty.node" \
  "${APP_DIR}/node_modules/node-pty/build/Release/pty.node"
if [[ -f "${BUILD_DIR}/node_modules/node-pty/build/Release/spawn-helper" ]]; then
  cp -f "${BUILD_DIR}/node_modules/node-pty/build/Release/spawn-helper" \
    "${APP_DIR}/node_modules/node-pty/build/Release/spawn-helper"
  chmod 0755 "${APP_DIR}/node_modules/node-pty/build/Release/spawn-helper"
else
  rm -f "${APP_DIR}/node_modules/node-pty/build/Release/spawn-helper"
fi
rm -f \
  "${APP_DIR}/node_modules/better-sqlite3/.codex-native-module-build.json" \
  "${APP_DIR}/node_modules/node-pty/.codex-native-module-build.json"

echo "Done rebuilding native modules."
