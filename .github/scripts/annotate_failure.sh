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
  # linker errors, each with the line after it); fall back to the raw
  # tail. Colour codes are stripped first (cargo colours "error" under
  # CI, so an anchored match on the raw text missed every compiler
  # error). GitHub keeps at most 10 error annotations per step.
  plain="$(sed -e $'s/\x1b\\[[0-9;]*[A-Za-z]//g' "$log")"
  lines="$(printf '%s\n' "$plain" \
    | grep -E -A1 'panicked|assertion|left:|right:|^error|error\[|Error:|FAILED|undefined reference|cannot find|could not' \
    | grep -v -E '^--$|build failed, waiting' | head -n 9)"
  if [ -z "$lines" ]; then
    lines="$(printf '%s\n' "$plain" | tail -n 8)"
  fi
  while IFS= read -r line; do
    line="${line//'%'/'%25'}"
    line="${line//$'\r'/}"
    echo "::error title=${title}::${line}"
  done <<< "$lines"
fi
rm -f "$log"
exit "$rc"
