#!/usr/bin/env bash
set -euo pipefail

installer=${1:?Windows installer path is required}
test -f "$installer"

runner_temp=$(cygpath -u "${RUNNER_TEMP:?}")
install_dir=$runner_temp/koharu-startup-smoke
mkdir -p "$install_dir"
install_windows=$(cygpath -w "$install_dir")
MSYS2_ARG_CONV_EXCL='*' "$installer" /S "/D=$install_windows"

local_data=$(cygpath -u "${LOCALAPPDATA:?}")
marker_name="upgrade-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-0}.marker"
legacy_marker="$install_dir/store/$marker_name"
persistent_marker="$local_data/KoharuData/store/$marker_name"
mkdir -p "$(dirname "$legacy_marker")"
printf 'models survive installer upgrades\n' >"$legacy_marker"

required=(
  "$install_dir/koharu.exe"
  "$install_dir/koharu-torch.dll"
)
for path in "${required[@]}"; do
  if [[ ! -f "$path" ]]; then
    printf 'Installed payload is missing %s\n' "$path" >&2
    exit 1
  fi
done

pid=
cleanup() {
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  rm -f "$legacy_marker" "$persistent_marker"
}
trap cleanup EXIT

verify_torch_runtime() {
  local report=$1
  local report_windows
  report_windows=$(cygpath -w "$report")
  rm -f "$report"
  if ! MSYS2_ARG_CONV_EXCL='*' "$install_dir/koharu.exe" \
    --verify-torch-runtime "$report_windows"; then
    printf 'Installed Koharu could not initialize its Torch runtime.\n' >&2
    if [[ -f "$report" ]]; then
      sed -n '1,240p' "$report" >&2
    else
      printf 'Koharu did not produce the expected runtime report at %s\n' "$report" >&2
    fi
    exit 1
  fi
  if [[ ! -f "$report" ]] || [[ $(head -n 1 "$report") != ready ]]; then
    printf 'Installed Koharu produced an invalid Torch runtime report.\n' >&2
    [[ ! -f "$report" ]] || sed -n '1,240p' "$report" >&2
    exit 1
  fi
}

launch_and_wait() {
  local log=$1
  "$install_dir/koharu.exe" >"$log" 2>&1 &
  pid=$!
  for _ in {1..15}; do
    if ! kill -0 "$pid" 2>/dev/null; then
      local status=0
      wait "$pid" || status=$?
      pid=
      printf 'Installed Koharu exited during startup with status %s\n' "$status" >&2
      sed -n '1,240p' "$log" >&2
      exit 1
    fi
    sleep 1
  done
}

stop_app() {
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid"
    wait "$pid" 2>/dev/null || true
  fi
  pid=
}

verify_torch_runtime "$runner_temp/koharu-torch-first.txt"
launch_and_wait "$runner_temp/koharu-startup-first.log"
if [[ ! -f "$persistent_marker" ]]; then
  printf 'Koharu did not migrate the legacy model store to %s\n' "$persistent_marker" >&2
  exit 1
fi
if [[ -f "$legacy_marker" ]]; then
  printf 'Koharu left the migrated model marker in its installation directory\n' >&2
  exit 1
fi

stop_app
MSYS2_ARG_CONV_EXCL='*' "$installer" /S "/D=$install_windows"
if [[ ! -f "$persistent_marker" ]]; then
  printf 'Reinstalling Koharu removed the persistent model store\n' >&2
  exit 1
fi
verify_torch_runtime "$runner_temp/koharu-torch-reinstalled.txt"
launch_and_wait "$runner_temp/koharu-startup-reinstalled.log"

printf 'Installed Koharu loaded Torch, remained running, and preserved its model store across reinstall.\n'
