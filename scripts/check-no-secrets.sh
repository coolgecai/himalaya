#!/usr/bin/env bash
set -euo pipefail

root="${1:-.}"
status=0

while IFS= read -r path; do
  case "$path" in
    *.env|*/.env|creds.txt|*/creds.txt|*.pem|*.key|*.p12|*.pfx)
      echo "secret-like tracked path is not allowed: $path" >&2
      status=1
      ;;
    target/*|*/target/*|node_modules/*|*/node_modules/*|vscode-extension/out/*|*/vscode-extension/out/*|*.vsix|.Himalaya/*|*/.Himalaya/*|.Himalayad-agents/*|*/.Himalayad-agents/*|.clawd-agents/*|*/.clawd-agents/*|.claude/*|*/.claude/*|.port_sessions/*|*/.port_sessions/*|.sandbox-home/*|*/.sandbox-home/*|.sandbox-tmp/*|*/.sandbox-tmp/*)
      echo "generated artifact tracked path is not allowed: $path" >&2
      status=1
      ;;
  esac
done < <(git -C "$root" ls-files)

exit "$status"
