#!/usr/bin/env bash
set -euo pipefail

installer=${1:?Windows installer path is required}
test -f "$installer"

runner_temp=$(cygpath -u "${RUNNER_TEMP:?}")
install_dir=$runner_temp/koharu-startup-smoke
mkdir -p "$install_dir"
install_windows=$(cygpath -w "$install_dir")
MSYS2_ARG_CONV_EXCL='*' "$installer" /S "/D=$install_windows"

required=(
  "$install_dir/koharu.exe"
  "$install_dir/koharu-torch.dll"
  "$install_dir/libcef.dll"
  "$install_dir/chrome_elf.dll"
  "$install_dir/resources.pak"
  "$install_dir/resources/ui/index.html"
)
for path in "${required[@]}"; do
  if [[ ! -f "$path" ]]; then
    printf 'Installed payload is missing %s\n' "$path" >&2
    exit 1
  fi
done
if ! compgen -G "$install_dir/locales/*.pak" >/dev/null; then
  printf 'Installed payload has no CEF locale packs in %s\n' "$install_dir/locales" >&2
  exit 1
fi

log=$runner_temp/koharu-startup.log
"$install_dir/koharu.exe" >"$log" 2>&1 &
pid=$!
cleanup() {
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

for _ in {1..15}; do
  if ! kill -0 "$pid" 2>/dev/null; then
    status=0
    wait "$pid" || status=$?
    printf 'Installed Koharu exited during startup with status %s\n' "$status" >&2
    sed -n '1,240p' "$log" >&2
    exit 1
  fi
  sleep 1
done

printf 'Installed Koharu remained running with the packaged CEF layout.\n'
