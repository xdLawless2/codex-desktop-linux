#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DMG_PATH="${1:-${ROOT_DIR}/Codex.dmg}"
WORK_DIR="${ROOT_DIR}/work_version_check"

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

need_cmd node

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

rm -rf "${WORK_DIR}"
mkdir -p "${WORK_DIR}"

EXTRACT_LOG="${WORK_DIR}/7z-extract.log"
EXTRACT_RC=0
extract_archive "${DMG_PATH}" "${WORK_DIR}" "${EXTRACT_LOG}" || EXTRACT_RC=$?
if [[ "${EXTRACT_RC}" -ne 0 ]]; then
  if grep -q "Dangerous link path was ignored" "${EXTRACT_LOG}"; then
    echo "7z warning: ignored unsafe symlink entries in DMG, continuing." >&2
  elif command -v dmg2img >/dev/null 2>&1; then
    echo "Direct DMG extraction failed, retrying via dmg2img..." >&2
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

ASAR_PATH="$(find "${WORK_DIR}" -type f -path "*/Contents/Resources/app.asar" | head -n 1 || true)"
if [[ -z "${ASAR_PATH}" ]]; then
  ASAR_PATH="$(find "${WORK_DIR}" -type f -name "app.asar" | head -n 1 || true)"
fi
if [[ -z "${ASAR_PATH}" ]]; then
  cat "${EXTRACT_LOG}" >&2 || true
  echo "Could not find app.asar in extracted DMG payload." >&2
  exit 1
fi

CODEX_VERSION="$(
  node - "${ASAR_PATH}" <<'NODE'
const fs = require("fs");
const asar = require("@electron/asar");

const asarPath = process.argv[2];
const pkgRaw = asar.extractFile(asarPath, "package.json");
const pkg = JSON.parse(Buffer.isBuffer(pkgRaw) ? pkgRaw.toString("utf8") : pkgRaw);

if (!pkg.version) {
  process.exit(2);
}
process.stdout.write(String(pkg.version));
NODE
)"

if [[ -z "${CODEX_VERSION}" ]]; then
  echo "Failed to read Codex version from app.asar/package.json" >&2
  exit 1
fi

echo "${CODEX_VERSION}"
