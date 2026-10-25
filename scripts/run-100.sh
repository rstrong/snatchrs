#!/usr/bin/env bash
# Run in a terminal or tmux; leave the final summary in terminal history.
set -uo pipefail
snatchrs_bin="${SNATCHRS_BIN:-$HOME/.local/bin/snatchrs}"
snatchrs_root="${SNATCHRS_MIRROR:-/mirror/pypi}"
snatchrs_workers="${1:-8}"
mkdir -p "$snatchrs_root"
printf 'running\n' > "$snatchrs_root/run.status"
"$snatchrs_bin" mirror --mirror "$snatchrs_root" --sample 100 --seed 20261025 --concurrency "$snatchrs_workers"
snatchrs_exit=$?
printf 'exit=%s\n' "$snatchrs_exit" > "$snatchrs_root/run.status"
printf '\nsnatchrs exited with status %s; report: %s/mirror-report.json\n' "$snatchrs_exit" "$snatchrs_root"
exit "$snatchrs_exit"
