#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UPSTREAM_URL="https://persistent.oaistatic.com/codex-app-prod/Codex.dmg"
DMG_PATH="${ROOT_DIR}/Codex.dmg"
TEMP_DMG="$(mktemp "${ROOT_DIR}/.Codex.dmg.XXXXXX")"

cleanup() {
  rm -f "${TEMP_DMG}"
}
trap cleanup EXIT

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

for command in codex curl node npm cargo 7z icns2png; do
  need_cmd "${command}"
done

echo "== Updating Codex Desktop for Linux =="

echo "[1/6] Closing running Codex Desktop processes..."
pkill -TERM -f '/codex-desktop\.bin' 2>/dev/null || true
pkill -TERM -f "${ROOT_DIR}/node_modules/.bin/electron.*${ROOT_DIR}/app_asar" 2>/dev/null || true
sleep 1

echo "[2/6] Updating the Linux Codex backend..."
codex update
CODEX_CLI_VERSION="$(codex --version | awk '{print $2}')"
if [[ -z "${CODEX_CLI_VERSION}" ]]; then
  echo "Could not determine the installed Codex CLI version." >&2
  exit 1
fi
printf '%s\n' "${CODEX_CLI_VERSION}" > "${ROOT_DIR}/codex-cli-version.txt"

echo "[3/6] Downloading the latest official desktop image..."
curl -fL --retry 3 "${UPSTREAM_URL}" -o "${TEMP_DMG}"
mv -f "${TEMP_DMG}" "${DMG_PATH}"

echo "[4/6] Extracting, rebuilding, and installing..."
bash "${ROOT_DIR}/scripts/setup.sh" "${DMG_PATH}"

echo "[5/6] Recording the upstream release..."
UPSTREAM_VERSION="$(bash "${ROOT_DIR}/scripts/get-codex-version.sh" "${DMG_PATH}")"
UPSTREAM_SHA256="$(sha256sum "${DMG_PATH}" | awk '{print $1}')"
UPSTREAM_ETAG="$(
  curl -fsSI "${UPSTREAM_URL}" |
    awk 'BEGIN { IGNORECASE=1 } /^etag:/ { gsub("\r", "", $2); print $2; exit }'
)"
if [[ -z "${UPSTREAM_ETAG}" ]]; then
  echo "The upstream response did not include an ETag." >&2
  exit 1
fi
printf '%s\n' "${UPSTREAM_VERSION}" > "${ROOT_DIR}/upstream-version.txt"
printf '%s\n' "${UPSTREAM_ETAG}" > "${ROOT_DIR}/upstream-etag.txt"
printf '%s\n' "${UPSTREAM_SHA256}" > "${ROOT_DIR}/upstream-sha256.txt"

echo "[6/6] Building DEB and AppImage packages..."
npm --prefix "${ROOT_DIR}" run build:linux

echo
echo "Updated successfully to ${UPSTREAM_VERSION}."
echo "Launch: codex-desktop"
echo "Packages: ${ROOT_DIR}/dist/"
