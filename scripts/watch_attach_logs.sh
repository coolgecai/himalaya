#!/usr/bin/env bash
set -euo pipefail

# Background watcher for Dev Host logs to capture attachment-related lines
LOG_DIR="/tmp/himalaya-devhost-userdata-enabled/logs/20260430T161941/window1"
OUT_TMP="/tmp/himalaya-attach-monitor.log"
OUT_REPO="/mnt/d486b21f-5399-45c5-b21d-0f7921dc862d/Himalaya-main/vscode-extension/attach-monitor.log"
PATTERN='Attachment|--file|fsPath|\\battach\\b|attached|Attachment not found|Attachment is not readable|Skipped sensitive attachment|assistantStart|--attachment|attachment:|file://'

mkdir -p "$(dirname "$OUT_REPO")"

echo "[watch_attach_logs] starting; watching: $LOG_DIR -> $OUT_TMP and $OUT_REPO"
# ensure output files exist for immediate debugging
touch "$OUT_TMP" "$OUT_REPO"
echo "[watch_attach_logs] ensured output files: $OUT_TMP $OUT_REPO"

# wait until the log directory exists
while [ ! -d "$LOG_DIR" ]; do
  echo "[watch_attach_logs] waiting for log dir $LOG_DIR ..."
  sleep 1
done

# wait for at least one file to tail
while [ "$(find "$LOG_DIR" -type f | wc -l)" -eq 0 ]; do
  sleep 1
done

# Tail all current files under the log dir and filter
# Use stdbuf to force line buffering
find "$LOG_DIR" -type f -print0 | xargs -0 -r tail -n 0 -F | stdbuf -oL grep --line-buffered -E "$PATTERN" | while IFS= read -r line; do
  ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  echo "[$ts] $line" | tee -a "$OUT_TMP" "$OUT_REPO"
done
