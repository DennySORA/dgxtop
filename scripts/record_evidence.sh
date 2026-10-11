#!/usr/bin/env bash
# Runs one verification command and stores a self-describing evidence log:
#   scripts/record_evidence.sh <evidence-name> <command> [args...]
# Writes docs/validation/evidence/<evidence-name>.log containing the command,
# UTC time, git commit, host summary, full output and the exit code, then
# prints the log path, exit code and SHA-256. The command's exit code is
# returned unchanged (a failing check is recorded as failing, never hidden).
set -uo pipefail

if [ $# -lt 2 ]; then
    echo "usage: $0 <evidence-name> <command> [args...]" >&2
    exit 2
fi

name="$1"
shift
case "$name" in
    *[!A-Za-z0-9._-]*) echo "evidence name must match [A-Za-z0-9._-]+" >&2; exit 2 ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"
directory="${root}/docs/validation/evidence"
mkdir -p "$directory"
log="${directory}/${name}.log"

{
    echo "# evidence: ${name}"
    echo "# command: $*"
    echo "# started_utc: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "# git_commit: $(git -C "$root" rev-parse HEAD 2>/dev/null || echo unknown)$(git -C "$root" diff --quiet 2>/dev/null || echo ' (working tree modified)')"
    echo "# host: $(uname -srm)"
    echo "# cpu: $(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ //')"
    echo "# rustc: $(rustc --version 2>/dev/null || echo unknown)"
    echo "# ---- output ----"
} > "$log"

(cd "$root" && "$@") >> "$log" 2>&1
status=$?

{
    echo "# ---- end ----"
    echo "# finished_utc: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "# exit_code: ${status}"
    echo "# redacted: GPU UUIDs and hardware serial numbers"
} >> "$log"

# Hardware identifiers are not secrets but identify the owner's machine; they
# are masked before the evidence is hashed and stored. sed writes a new file
# because `sed -i` takes a backup suffix on BSD sed (macOS), which swallowed
# `-E` there. A log that could not be redacted is deleted, never kept.
redacted="${log}.redacted"
if ! sed -E \
    -e 's/GPU-[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}/GPU-<redacted>/g' \
    -e 's/(key: "serial", value: ")[^"]*"/\1<redacted>"/g' \
    -e 's/("serial" *: *")[^"]*"/\1<redacted>"/g' \
    "$log" > "$redacted"; then
    rm -f -- "$log" "$redacted"
    echo "redaction failed; ${log#"${root}/"} was deleted" >&2
    exit 1
fi
mv -f -- "$redacted" "$log"

echo "evidence=${log#"${root}/"} exit=${status} sha256=$(sha256sum "$log" | cut -d' ' -f1)"
exit "$status"
