#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK_DIR="${ROOT_DIR}/work_dmg"
APP_ASAR_DIR="${ROOT_DIR}/app_asar"
APP_RESOURCES_DIR="${ROOT_DIR}/app_resources"
ICON_DIR="${ROOT_DIR}/assets/icons/linux"
DMG_PATH="${1:-${ROOT_DIR}/Codex.dmg}"
CODEX_CLI_SOURCE_PATH="${CODEX_CLI_SOURCE_PATH:-}"
CODEX_CODE_MODE_HOST_SOURCE_PATH="${CODEX_CODE_MODE_HOST_SOURCE_PATH:-}"

if [[ ! -f "${DMG_PATH}" ]]; then
  echo "DMG not found: ${DMG_PATH}" >&2
  exit 1
fi

extract_archive() {
  local archive_path="$1"
  local output_dir="$2"
  local log_path="$3"

  set +e
  "${SEVEN_Z_BIN}" x -y -o"${output_dir}" "${archive_path}" >"${log_path}" 2>&1
  local rc=$?
  set -e
  return "${rc}"
}

if ! command -v 7z >/dev/null 2>&1; then
  echo "Missing required command: 7z (install the current 7zip package)." >&2
  exit 1
fi
SEVEN_Z_BIN="$(command -v 7z)"

rm -rf "${WORK_DIR}" "${APP_ASAR_DIR}" "${APP_RESOURCES_DIR}" "${ICON_DIR}"
mkdir -p "${WORK_DIR}" "${APP_ASAR_DIR}" "${APP_RESOURCES_DIR}/bin" "${ICON_DIR}"

echo "[1/3] Extracting DMG..."
EXTRACT_LOG="${WORK_DIR}/7z-extract.log"
EXTRACT_RC=0
extract_archive "${DMG_PATH}" "${WORK_DIR}" "${EXTRACT_LOG}" || EXTRACT_RC=$?
if [[ "${EXTRACT_RC}" -ne 0 ]]; then
  if grep -q "Dangerous link path was ignored" "${EXTRACT_LOG}"; then
    echo "7z warning: ignored unsafe symlink entries in DMG, continuing."
  elif command -v dmg2img >/dev/null 2>&1; then
    echo "Direct DMG extraction failed, retrying via dmg2img..."
    IMG_PATH="${WORK_DIR}/Codex.img"
    dmg2img "${DMG_PATH}" "${IMG_PATH}" >/dev/null
    EXTRACT_RC=0
    extract_archive "${IMG_PATH}" "${WORK_DIR}" "${EXTRACT_LOG}" || EXTRACT_RC=$?
    if [[ "${EXTRACT_RC}" -ne 0 ]]; then
      cat "${EXTRACT_LOG}" >&2
      exit "${EXTRACT_RC}"
    fi
  else
    cat "${EXTRACT_LOG}" >&2
    exit "${EXTRACT_RC}"
  fi
fi

echo "[2/3] Locating app.asar..."
ASAR_PATH="$(find "${WORK_DIR}" -type f -path "*/Contents/Resources/app.asar" | head -n 1 || true)"
if [[ -z "${ASAR_PATH}" ]]; then
  ASAR_PATH="$(find "${WORK_DIR}" -type f -name "app.asar" | head -n 1 || true)"
fi
if [[ -z "${ASAR_PATH}" ]]; then
  cat "${EXTRACT_LOG}" >&2 || true
  echo "Could not find app.asar after extraction." >&2
  exit 1
fi

RESOURCES_DIR="$(dirname "${ASAR_PATH}")"
INFO_PLIST="$(dirname "${RESOURCES_DIR}")/Info.plist"
if [[ ! -f "${INFO_PLIST}" ]]; then
  echo "Could not find the upstream application Info.plist." >&2
  exit 1
fi
ICNS_NAME="$(python3 - "${INFO_PLIST}" <<'PY'
import plistlib
import sys
from pathlib import Path

with Path(sys.argv[1]).open("rb") as handle:
    name = plistlib.load(handle).get("CFBundleIconFile")
if not isinstance(name, str) or not name or Path(name).name != name:
    raise SystemExit("Info.plist has no safe CFBundleIconFile value")
print(name if name.endswith(".icns") else f"{name}.icns")
PY
)"
ICNS_PATH="${RESOURCES_DIR}/${ICNS_NAME}"
if [[ ! -f "${ICNS_PATH}" ]]; then
  echo "Upstream application icon does not exist: ${ICNS_PATH}" >&2
  exit 1
fi
ICNS_PATH="${ICNS_PATH}" ICON_DIR="${ICON_DIR}" python3 - <<'PY'
import os
import struct
from pathlib import Path

icns_path = Path(os.environ["ICNS_PATH"])
icon_dir = Path(os.environ["ICON_DIR"])
data = icns_path.read_bytes()
if len(data) < 8 or data[:4] != b"icns":
    raise SystemExit("upstream application icon is not a valid ICNS container")
declared_length = struct.unpack(">I", data[4:8])[0]
if declared_length != len(data):
    raise SystemExit("ICNS container length does not match its header")

position = 8
sizes = set()
while position < len(data):
    if position + 8 > len(data):
        raise SystemExit("truncated ICNS entry header")
    entry_length = struct.unpack(">I", data[position + 4 : position + 8])[0]
    if entry_length < 8 or position + entry_length > len(data):
        raise SystemExit("invalid ICNS entry length")
    payload = data[position + 8 : position + entry_length]
    if len(payload) >= 24 and payload[:8] == b"\x89PNG\r\n\x1a\n":
        width, height = struct.unpack(">II", payload[16:24])
        if width == height and width > 0:
            (icon_dir / f"{width}x{height}.png").write_bytes(payload)
            sizes.add(width)
    position += entry_length

if not sizes:
    raise SystemExit("ICNS container contains no square PNG icons")
PY
cp -f "${RESOURCES_DIR}/codex" "${APP_RESOURCES_DIR}/bin/codex"
chmod +x "${APP_RESOURCES_DIR}/bin/codex"

mkdir -p "${APP_RESOURCES_DIR}/plugins/openai-bundled/plugins"
for plugin in browser chrome deep-research sites visualize; do
  cp -a \
    "${RESOURCES_DIR}/plugins/openai-bundled/plugins/${plugin}" \
    "${APP_RESOURCES_DIR}/plugins/openai-bundled/plugins/${plugin}"
done
cp -a "${RESOURCES_DIR}/skills" "${APP_RESOURCES_DIR}/skills"
cp -f "${RESOURCES_DIR}/codex-notification.wav" "${APP_RESOURCES_DIR}/codex-notification.wav"
cp -f "${RESOURCES_DIR}/THIRD_PARTY_NOTICES.txt" "${APP_RESOURCES_DIR}/THIRD_PARTY_NOTICES.txt"

echo "[3/3] Extracting app.asar -> ${APP_ASAR_DIR}"
npx --no-install asar extract "${ASAR_PATH}" "${APP_ASAR_DIR}"

if [[ ! -f "${APP_RESOURCES_DIR}/bin/codex" ]]; then
  echo "Could not locate bundled Codex CLI in DMG resources." >&2
  exit 1
fi

# Optional Linux override for CI packaging. Useful because DMG bundles
# macOS binaries, while Linux packages require a Linux codex binary.
if [[ -n "${CODEX_CLI_SOURCE_PATH}" ]]; then
  if [[ ! -f "${CODEX_CLI_SOURCE_PATH}" ]]; then
    echo "CODEX_CLI_SOURCE_PATH does not exist: ${CODEX_CLI_SOURCE_PATH}" >&2
    exit 1
  fi
  cp -f "${CODEX_CLI_SOURCE_PATH}" "${APP_RESOURCES_DIR}/bin/codex"
  chmod +x "${APP_RESOURCES_DIR}/bin/codex"
fi
if [[ -n "${CODEX_CODE_MODE_HOST_SOURCE_PATH}" ]]; then
  if [[ ! -f "${CODEX_CODE_MODE_HOST_SOURCE_PATH}" ]]; then
    echo "CODEX_CODE_MODE_HOST_SOURCE_PATH does not exist: ${CODEX_CODE_MODE_HOST_SOURCE_PATH}" >&2
    exit 1
  fi
  cp -f \
    "${CODEX_CODE_MODE_HOST_SOURCE_PATH}" \
    "${APP_RESOURCES_DIR}/bin/codex-code-mode-host"
  chmod +x "${APP_RESOURCES_DIR}/bin/codex-code-mode-host"
fi

echo "Done."
echo "app_asar: ${APP_ASAR_DIR}"
if [[ -d "${APP_RESOURCES_DIR}/bin" ]]; then
  echo "app_resources/bin: ${APP_RESOURCES_DIR}/bin"
fi
