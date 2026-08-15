#!/usr/bin/env bash
set -euo pipefail

dispatch_missing=false
if [[ ${1:-} == --dispatch-missing ]]; then
  dispatch_missing=true
  shift
fi
if (( $# != 0 )); then
  printf 'Usage: %s [--dispatch-missing]\n' "$0" >&2
  exit 2
fi

: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
release_from_source() {
  sed -n 's/^const RELEASE: &str = "\([^"]*\)";/\1/p' "$1"
}

llama_release=$(release_from_source \
  "$repository_root/crates/koharu-runtime/src/runtime/packages/llama.rs")
diffusion_release=$(release_from_source \
  "$repository_root/crates/koharu-runtime/src/runtime/packages/diffusion.rs")
if [[ -z $llama_release || -z $diffusion_release ]]; then
  printf 'Unable to read the pinned runtime release names from the Rust sources.\n' >&2
  exit 1
fi

tags=("$llama_release" "$diffusion_release")
prefixes=("llama" "stable-diffusion")
workflows=("llama-cpp.yml" "stable-diffusion-cpp.yml")
refs=(
  "${llama_release#llama.cpp-}"
  "${diffusion_release#stable-diffusion.cpp-}"
)
run_titles=(
  "Runtime dependency - llama.cpp ${refs[0]}"
  "Runtime dependency - stable-diffusion.cpp ${refs[1]}"
)
dispatches=(0 0)

release_ready() {
  local tag=$1
  local prefix=$2
  local asset_names
  if ! asset_names=$(gh release view "$tag" \
    --repo "$GITHUB_REPOSITORY" \
    --json assets \
    --jq '.assets[].name' 2>/dev/null); then
    return 1
  fi
  local backend
  for backend in cuda hip vulkan; do
    local expected="$prefix-$backend-windows-2022.tar.gz"
    case $'\n'"$asset_names"$'\n' in
      *$'\n'"$expected"$'\n'*) ;;
      *) return 1 ;;
    esac
  done
}

workflow_active() {
  local workflow=$1
  local run_title=$2
  local runs
  local status
  local title
  if ! runs=$(gh run list \
    --repo "$GITHUB_REPOSITORY" \
    --workflow "$workflow" \
    --branch main \
    --event workflow_dispatch \
    --limit 20 \
    --json status,displayTitle \
    --jq '.[] | [.status, .displayTitle] | @tsv'); then
    printf 'Unable to inspect workflow %s.\n' "$workflow" >&2
    exit 1
  fi
  while IFS=$'\t' read -r status title; do
    if [[ $title == "$run_title" && $status != completed ]]; then
      return 0
    fi
  done <<< "$runs"
  return 1
}

start_if_needed() {
  local index=$1
  if release_ready "${tags[index]}" "${prefixes[index]}"; then
    return
  fi
  if ! $dispatch_missing; then
    printf 'Windows runtime release %s is incomplete.\n' "${tags[index]}" >&2
    exit 1
  fi
  if workflow_active "${workflows[index]}" "${run_titles[index]}"; then
    printf 'Waiting for active workflow %s for pinned ref %s.\n' \
      "${workflows[index]}" "${refs[index]}"
    return
  fi
  printf 'Dispatching %s for pinned ref %s.\n' \
    "${workflows[index]}" "${refs[index]}"
  gh workflow run "${workflows[index]}" \
    --repo "$GITHUB_REPOSITORY" \
    --ref main \
    -f "ref=${refs[index]}"
  dispatches[index]=$((dispatches[index] + 1))
}

start_if_needed 0
start_if_needed 1
if (( dispatches[0] > 0 || dispatches[1] > 0 )); then
  # Workflow dispatch is asynchronous; give the new runs time to become
  # visible before deciding that another dispatch is necessary.
  sleep 15
fi

deadline=$((SECONDS + ${RUNTIME_WAIT_SECONDS:-14400}))
while (( SECONDS < deadline )); do
  ready=0
  for index in 0 1; do
    if release_ready "${tags[index]}" "${prefixes[index]}"; then
      ready=$((ready + 1))
      continue
    fi
    if ! workflow_active "${workflows[index]}" "${run_titles[index]}"; then
      if (( dispatches[index] >= 2 )); then
        printf '%s completed twice without publishing all Windows assets.\n' \
          "${workflows[index]}" >&2
        exit 1
      fi
      printf 'Retrying %s for pinned ref %s.\n' \
        "${workflows[index]}" "${refs[index]}"
      gh workflow run "${workflows[index]}" \
        --repo "$GITHUB_REPOSITORY" \
        --ref main \
        -f "ref=${refs[index]}"
      dispatches[index]=$((dispatches[index] + 1))
    fi
  done
  if (( ready == 2 )); then
    printf 'All pinned Windows ML runtime assets are available.\n'
    exit 0
  fi
  sleep 30
done

printf 'Timed out waiting for pinned Windows ML runtime releases.\n' >&2
exit 1
