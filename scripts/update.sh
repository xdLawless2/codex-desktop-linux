#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UPSTREAM_URL="https://persistent.oaistatic.com/codex-app-prod/Codex.dmg"
DMG_PATH="${ROOT_DIR}/Codex.dmg"
TEMP_DMG="$(mktemp "${ROOT_DIR}/.Codex.dmg.XXXXXX")"
TEMP_HEADERS="$(mktemp "${ROOT_DIR}/.Codex.headers.XXXXXX")"
BACKUP_DIR="$(mktemp -d "${ROOT_DIR}/.update-backup.XXXXXX")"
PAYLOAD_BACKED_UP=0
UPDATE_SUCCEEDED=0

payload_paths=(
  "Codex.dmg"
  "app_asar"
  "app_resources"
  "assets/icons/linux"
  "dist"
)

restore_payload() {
  local relative_path
  local current_path
  local backup_path

  for relative_path in "${payload_paths[@]}"; do
    current_path="${ROOT_DIR}/${relative_path}"
    backup_path="${BACKUP_DIR}/${relative_path}"
    rm -rf "${current_path}"
    if [[ -e "${backup_path}" ]]; then
      mkdir -p "$(dirname "${current_path}")"
      mv "${backup_path}" "${current_path}"
    fi
  done
}

cleanup() {
  local status=$?
  set +e
  if [[ "${status}" -ne 0 && "${PAYLOAD_BACKED_UP}" == "1" ]]; then
    echo "Update failed; restoring the previous generated payload." >&2
    restore_payload
  fi
  rm -f "${TEMP_DMG}" "${TEMP_HEADERS}"
  if [[ "${UPDATE_SUCCEEDED}" == "1" || "${PAYLOAD_BACKED_UP}" == "0" ]]; then
    rm -rf "${BACKUP_DIR}"
  fi
  exit "${status}"
}
trap cleanup EXIT

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

for command in codex curl node npm cargo 7z sha256sum; do
  need_cmd "${command}"
done

echo "== Updating Codex Desktop for Linux =="

echo "[1/7] Closing running Codex Desktop processes..."
pkill -TERM -f '/codex-desktop\.bin' 2>/dev/null || true
pkill -TERM -f "${ROOT_DIR}/node_modules/.bin/electron.*${ROOT_DIR}/app_asar" 2>/dev/null || true
sleep 1

echo "[2/7] Downloading and pinning the latest official desktop image..."
curl -fL --retry 3 -D "${TEMP_HEADERS}" "${UPSTREAM_URL}" -o "${TEMP_DMG}"
UPSTREAM_VERSION="$(bash "${ROOT_DIR}/scripts/get-codex-version.sh" "${TEMP_DMG}")"
UPSTREAM_SHA256="$(sha256sum "${TEMP_DMG}" | awk '{print $1}')"
UPSTREAM_ETAG="$(
  awk 'BEGIN { IGNORECASE=1 } /^etag:/ { gsub("\r", "", $2); value=$2 } END { print value }' \
    "${TEMP_HEADERS}"
)"
if [[ -z "${UPSTREAM_ETAG}" ]]; then
  echo "The upstream response did not include an ETag." >&2
  exit 1
fi
printf 'Candidate: version=%s etag=%s sha256=%s\n' \
  "${UPSTREAM_VERSION}" "${UPSTREAM_ETAG}" "${UPSTREAM_SHA256}"

echo "[3/7] Updating the Linux Codex backend..."
codex update
CODEX_CLI_VERSION="$(codex --version | awk '{print $2}')"
if [[ -z "${CODEX_CLI_VERSION}" ]]; then
  echo "Could not determine the installed Codex CLI version." >&2
  exit 1
fi
CODEX_CLI_SOURCE_PATH="$(readlink -f "$(command -v codex)")"
CODEX_CODE_MODE_HOST_SOURCE_PATH="$(dirname "${CODEX_CLI_SOURCE_PATH}")/codex-code-mode-host"
if [[ ! -x "${CODEX_CODE_MODE_HOST_SOURCE_PATH}" ]]; then
  echo "Missing codex-code-mode-host beside ${CODEX_CLI_SOURCE_PATH}." >&2
  exit 1
fi
export CODEX_CLI_SOURCE_PATH CODEX_CODE_MODE_HOST_SOURCE_PATH

echo "[4/7] Preserving the previous generated payload..."
for relative_path in "${payload_paths[@]}"; do
  current_path="${ROOT_DIR}/${relative_path}"
  if [[ -e "${current_path}" ]]; then
    backup_path="${BACKUP_DIR}/${relative_path}"
    mkdir -p "$(dirname "${backup_path}")"
    mv "${current_path}" "${backup_path}"
  fi
done
PAYLOAD_BACKED_UP=1

echo "[5/7] Extracting and rebuilding the candidate..."
SKIP_APP_INSTALL=1 bash "${ROOT_DIR}/scripts/setup.sh" "${TEMP_DMG}"

echo "[6/7] Building DEB and AppImage packages..."
npm --prefix "${ROOT_DIR}" run build:linux

echo "[7/7] Promoting the verified candidate and recording metadata..."
mv "${TEMP_DMG}" "${DMG_PATH}"
printf '%s\n' "${UPSTREAM_VERSION}" > "${ROOT_DIR}/upstream-version.txt"
printf '%s\n' "${UPSTREAM_ETAG}" > "${ROOT_DIR}/upstream-etag.txt"
printf '%s\n' "${UPSTREAM_SHA256}" > "${ROOT_DIR}/upstream-sha256.txt"
printf '%s\n' "${CODEX_CLI_VERSION}" > "${ROOT_DIR}/codex-cli-version.txt"
UPDATE_SUCCEEDED=1
rm -rf "${BACKUP_DIR}"

echo
echo "Updated successfully to ${UPSTREAM_VERSION}."
echo "Launch: codex-desktop"
echo "Packages: ${ROOT_DIR}/dist/"
