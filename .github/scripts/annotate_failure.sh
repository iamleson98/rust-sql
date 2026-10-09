#!/usr/bin/env bash
# Run a command; when it fails, surface the tail of its output as GitHub
# error annotations. Job logs and step summaries need an authenticated
# reader, while annotations are served by the public check-runs API — a
# red step then says WHY without anyone downloading logs.
#
# Usage: annotate_failure.sh <title> <command> [args...]
set -uo pipefail
title="$1"
shift
log="$(mktemp)"
"$@" 2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
if [ "$rc" -ne 0 ]; then
  # Prefer the diagnostic lines (panics, assertion messages, compiler and
  # linker errors); fall back to the raw tail. GitHub keeps at most 10
  # error annotations per step.
  lines="$(grep -E 'panicked|assertion|left:|right:|^error|error\[|Error:|FAILED|failed|undefined reference|cannot find' "$log" | tail -n 8)"
  if [ -z "$lines" ]; then
    lines="$(tail -n 8 "$log")"
  fi
  while IFS= read -r line; do
    line="${line//'%'/'%25'}"
    line="${line//$'\r'/}"
    echo "::error title=${title}::${line}"
  done <<< "$lines"
fi
rm -f "$log"
exit "$rc"
