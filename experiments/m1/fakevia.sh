#!/bin/sh
# M1 측정용 가짜 via: hc가 넘기는 인자를 기록하고, 마지막 인자(원격 줄)를 이 PC에서 그대로 실행한다.
LOG="${HERDVIA_M1_LOG:-/tmp/herdvia-m1.log}"
MODE_FILE="${HERDVIA_M1_MODE:-/tmp/herdvia-m1.mode}"
ts() { python3 -c 'import time;print("%.3f"%time.time())'; }
start=$(ts)
{
  printf '=== %s START pid=%s ppid=%s argc=%s tty_in=%s\n' "$start" "$$" "$PPID" "$#" "$([ -t 0 ] && echo y || echo n)"
  i=0; for a in "$@"; do i=$((i+1)); printf '  arg%s=[%s]\n' "$i" "$a"; done
} >> "$LOG"
for last in "$@"; do :; done
if [ -f "$MODE_FILE" ]; then
  code=$(cat "$MODE_FILE")
  printf '=== %s FORCED pid=%s exit=%s\n' "$(ts)" "$$" "$code" >> "$LOG"
  echo "herdvia-m1: forced failure $code" >&2
  exit "$code"
fi
sh -c "$last"
code=$?
printf '=== %s END pid=%s exit=%s started=%s\n' "$(ts)" "$$" "$code" "$start" >> "$LOG"
exit $code
