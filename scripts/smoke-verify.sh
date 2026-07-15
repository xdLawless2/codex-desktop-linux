#!/usr/bin/env bash
set -euo pipefail

APP_CMD="${1:-codex-desktop}"
TIMEOUT_SECONDS="${SMOKE_TIMEOUT_SECONDS:-20}"
LOG_FILE="${SMOKE_LOG_FILE:-/tmp/codex-desktop-smoke.log}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

info() {
  echo "== $* =="
}

info "Resolving commands"
command -v "${APP_CMD}" >/dev/null 2>&1 || fail "${APP_CMD} is not on PATH"
APP_PATH="$(command -v "${APP_CMD}")"
APP_REALPATH="$(readlink -f "${APP_PATH}" 2>/dev/null || printf '%s' "${APP_PATH}")"
echo "app_path=${APP_PATH}"
echo "app_realpath=${APP_REALPATH}"

if command -v codex >/dev/null 2>&1; then
  CODEX_REALPATH="$(readlink -f "$(command -v codex)" 2>/dev/null || command -v codex)"
  echo "codex_path=$(command -v codex)"
  echo "codex_realpath=${CODEX_REALPATH}"
  codex --version || true
else
  echo "codex_path=not-found"
fi

info "Checking desktop entries"
shopt -s nullglob
for desktop_file in /usr/share/applications/*codex* ~/.local/share/applications/*codex*; do
  [[ -f "${desktop_file}" ]] || continue
  echo "--- ${desktop_file}"
  grep -E '^(Name|Exec|StartupWMClass)=' "${desktop_file}" || true
done

info "Launching smoke test"
rm -f "${LOG_FILE}"
set +e
timeout "${TIMEOUT_SECONDS}s" env ELECTRON_ENABLE_LOGGING=1 "${APP_CMD}" >"${LOG_FILE}" 2>&1
status=$?
set -e

echo "exit_status=${status}"
echo "log_file=${LOG_FILE}"

tail -120 "${LOG_FILE}" || true

if grep -Eiq 'app server has not been initiali[sz]ed|CODEX_CLI_PATH is not set|could not find the Codex CLI|Missing bundled Linux Codex CLI|Cannot find module|Trace/breakpoint trap|Segmentation fault' "${LOG_FILE}"; then
  fail "smoke log contains bootstrap/crash error"
fi

if [[ "${status}" -ne 124 ]]; then
  fail "app did not remain running until smoke timeout"
fi

info "PASS"
