#!/usr/bin/env bash
# Usage: bash scripts/run-mirror.sh MIRROR_ROOT [mirror options...]
set -uo pipefail
if [[ $# -eq 0 ]]; then
    printf 'Usage: %s MIRROR_ROOT [mirror options...]\n' "$0" >&2
    exit 2
fi
snatchrs_root="$1"
shift
snatchrs_bin="${SNATCHRS_BIN:-snatchrs}"
mkdir -p "$snatchrs_root" || exit 1
printf 'running\n' > "$snatchrs_root/run.status" || exit 1
"$snatchrs_bin" mirror --mirror "$snatchrs_root" "$@"
snatchrs_exit=$?
printf 'exit=%s\n' "$snatchrs_exit" > "$snatchrs_root/run.status"
printf '\nsnatchrs exited with status %s; report: %s/mirror-report.json\n' "$snatchrs_exit" "$snatchrs_root"
exit "$snatchrs_exit"
