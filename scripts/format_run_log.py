#!/usr/bin/env python3
"""Turn `gh run view <id> --log` output into an evidence log (AT20, AT37).

Usage: gh run view RUN_ID --log | scripts/format_run_log.py

GitHub prefixes every line with its job, step and timestamp, and keeps the
colour codes. This prints one `## job / step` heading whenever the job or the
step changes, then the lines themselves without timestamp or colour, so
scripts/check_validation.py can read libtest's `test ... ok` lines. Nothing
is dropped. Reads stdin only.
"""
import re
import sys

TIMESTAMP = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z ?")
COLOUR = re.compile(r"\x1b\[[0-9;]*m")


def format_lines(lines):
    """Yield the evidence lines for the raw `gh run view --log` lines."""
    current = None
    for raw in lines:
        parts = raw.rstrip("\n").split("\t", 2)
        if len(parts) < 3:
            yield COLOUR.sub("", raw.rstrip("\n"))
            continue
        job, step, text = parts
        if (job, step) != current:
            current = (job, step)
            yield f"## {job} / {step}"
        yield COLOUR.sub("", TIMESTAMP.sub("", text))


def main() -> int:
    for line in format_lines(sys.stdin):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
