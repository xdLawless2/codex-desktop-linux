#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
APP_DIR="${APP_DIR:-${ROOT_DIR}/app_asar}"
BUILD_DIR="${BUILD_DIR:-${ROOT_DIR}/build_native}"
NATIVE_ARCH="${NATIVE_ARCH:-${npm_config_arch:-$(node -p "process.arch")}}"
case "${NATIVE_ARCH}" in
  x64|amd64|x86_64) NATIVE_ARCH="x64" ;;
  arm64|aarch64) NATIVE_ARCH="arm64" ;;
  *)
    echo "Unsupported native target architecture: ${NATIVE_ARCH}" >&2
    exit 1
    ;;
esac

if ! command -v file >/dev/null 2>&1; then
  echo "Missing required command: file" >&2
  exit 1
fi

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
UPSTREAM_BETTER_SQLITE3_VERSION="$(node -p "require('${APP_DIR}/node_modules/better-sqlite3/package.json').version")"
NODE_PTY_VERSION="$(node -p "require('${APP_DIR}/node_modules/node-pty/package.json').version")"
node - "${UPSTREAM_BETTER_SQLITE3_VERSION}" "${BETTER_SQLITE3_VERSION}" <<'NODE'
const upstream = process.argv[2].split('.').map(Number);
const installed = process.argv[3].split('.').map(Number);
const compare = (left, right) => {
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const difference = (left[index] ?? 0) - (right[index] ?? 0);
    if (difference !== 0) return difference;
  }
  return 0;
};
if (upstream.some(Number.isNaN) || installed.some(Number.isNaN)) {
  throw new Error('Expected numeric better-sqlite3 versions');
}
if (upstream[0] !== installed[0] || compare(installed, upstream) < 0) {
  throw new Error(
    `Incompatible better-sqlite3 versions: upstream has ${process.argv[2]}, rebuild uses ${process.argv[3]}`,
  );
}
NODE
if [[ "${BETTER_SQLITE3_VERSION}" != "${UPSTREAM_BETTER_SQLITE3_VERSION}" ]]; then
  echo "Using better-sqlite3 ${BETTER_SQLITE3_VERSION} native sources for upstream ${UPSTREAM_BETTER_SQLITE3_VERSION} JavaScript wrapper."
fi
if [[ -z "${NODE_PTY_VERSION}" ]]; then
  echo "Could not determine the upstream node-pty version." >&2
  exit 1
fi

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

assert_native_arch() {
  local binary_path="$1"
  local label="$2"
  local description
  local binary_arch

  description="$(file -b "${binary_path}")"
  if [[ "${description}" != *ELF* ]]; then
    echo "${label} is not a Linux ELF binary: ${description}" >&2
    exit 1
  fi
  case "${description}" in
    *x86-64*) binary_arch="x64" ;;
    *aarch64*|*ARM64*) binary_arch="arm64" ;;
    *)
      echo "Unsupported ${label} architecture: ${description}" >&2
      exit 1
      ;;
  esac
  if [[ "${binary_arch}" != "${NATIVE_ARCH}" ]]; then
    echo "Architecture mismatch: ${label} is ${binary_arch}, target is ${NATIVE_ARCH}" >&2
    exit 1
  fi
}

assert_native_arch \
  "${APP_DIR}/node_modules/better-sqlite3/build/Release/better_sqlite3.node" \
  "better-sqlite3 native module"
assert_native_arch \
  "${APP_DIR}/node_modules/node-pty/build/Release/pty.node" \
  "node-pty native module"

echo "Done rebuilding native modules."
