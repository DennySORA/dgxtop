#!/usr/bin/env python3
"""Check docs/validation/VALIDATION.md against its evidence logs (AT40).

Usage: scripts/check_validation.py [VALIDATION.md] [EVIDENCE_DIR]

Rules (spec/dgxtop-v2/ACCEPTANCE.md: PASS needs evidence; written code is not PASS):
- the status table lists AT01..AT40 exactly once each, nothing else, and only the
  statuses PASS, FAIL, NOT_RUN and BLOCKED;
- every evidence log in the evidence table exists, is non-empty, has the SHA-256 and
  the exit code the table states;
- a PASS or FAIL row cites at least one evidence log; a PASS row cites only logs
  that exited 0;
- every AT has a `### ATnn` section, and every test named in the `Tests` column of a
  PASS section appears as a passing test in one of the logs its row cites.

Only local files are read. Exit status 0 means every rule held.
"""
import hashlib
import re
import sys
from pathlib import Path

STATUSES = {"PASS", "FAIL", "NOT_RUN", "BLOCKED"}
EXPECTED_IDS = [f"AT{number:02d}" for number in range(1, 41)]
BACKTICKED = re.compile(r"`([^`]+)`")
# libtest prints "... ok"; the supervisor host suite's own harness adds "(N ms)".
RUST_PASS = re.compile(r"^test (\S+) \.\.\. ok(?: \(\d+ ms\))?$")
PYTHON_PASS = re.compile(r"^(test\w*) \([\w.]+\) \.\.\. ok$")
EXIT_LINE = re.compile(r"^# exit_code: (-?\d+)$", re.MULTILINE)


def table_rows(text: str, heading: str) -> list[list[str]]:
    """Cells of the data rows of the first table under `## heading`."""
    lines = text.splitlines()
    try:
        start = lines.index(f"## {heading}")
    except ValueError:
        return []
    rows = []
    for line in lines[start + 1:]:
        if line.startswith("## "):
            break
        if not line.startswith("|"):
            if rows:
                break
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if all(set(cell) <= set("-: ") for cell in cells):
            continue
        rows.append(cells)
    return rows[1:] if rows else []


def backticked(cell: str) -> list[str]:
    return BACKTICKED.findall(cell)


def test_names(identifier: str) -> list[str]:
    """Function names a cited test id stands for (`a::b::{c, d}` -> c, d)."""
    braces = re.fullmatch(r"(.*)::\{(.*)\}", identifier)
    if braces:
        return [name.strip() for name in braces.group(2).split(",") if name.strip()]
    return [re.split(r"::|\.", identifier)[-1]]


def passing_tests(log_text: str) -> set[str]:
    names = set()
    for line in log_text.splitlines():
        match = RUST_PASS.match(line) or PYTHON_PASS.match(line)
        if match:
            names.add(match.group(1).split("::")[-1])
    return names


def sections(text: str) -> dict[str, str]:
    """Body of every `### ATnn ...` section."""
    found = {}
    parts = re.split(r"^### (AT\d\d)\b.*$", text, flags=re.MULTILINE)
    for index in range(1, len(parts), 2):
        found[parts[index]] = parts[index + 1].split("\n## ")[0]
    return found


def cited_tests(section: str) -> list[str]:
    """Backticked identifiers in the `Tests` column of a section's tables."""
    identifiers = []
    columns = None
    for line in section.splitlines():
        if not line.startswith("|"):
            columns = None
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if columns is None:
            columns = cells
            continue
        if all(set(cell) <= set("-: ") for cell in cells) or "Tests" not in columns:
            continue
        cell = cells[columns.index("Tests")] if len(cells) > columns.index("Tests") else ""
        identifiers.extend(item for item in backticked(cell) if "::" in item)
    return identifiers


def check(document: Path, evidence_dir: Path) -> list[str]:
    text = document.read_text(encoding="utf-8")
    problems = []
    logs = {}
    for cells in table_rows(text, "Evidence logs"):
        names = backticked(cells[0])
        if len(cells) < 4 or not names:
            problems.append(f"malformed evidence row: {' | '.join(cells)}")
            continue
        name, stated_exit, stated_hash = names[0], cells[-2], "".join(backticked(cells[-1]))
        path = evidence_dir / f"{name}.log"
        if not path.is_file() or path.stat().st_size == 0:
            problems.append(f"{name}: evidence log missing or empty ({path})")
            continue
        content = path.read_bytes()
        digest = hashlib.sha256(content).hexdigest()
        if digest != stated_hash:
            problems.append(f"{name}: SHA-256 {digest} does not match the stated {stated_hash}")
        log_text = content.decode("utf-8", errors="replace")
        exits = EXIT_LINE.findall(log_text)
        if not exits or exits[-1] != stated_exit:
            problems.append(f"{name}: recorded exit code {exits[-1:] or 'none'} is not the stated {stated_exit}")
        logs[name] = (exits[-1] if exits else None, passing_tests(log_text))
    rows = table_rows(text, "Acceptance status")
    seen = {}
    for cells in rows:
        identifier = "".join(re.findall(r"AT\d\d", cells[0])[:1])
        if not identifier:
            problems.append(f"status row without an AT id: {' | '.join(cells)}")
            continue
        if identifier in seen:
            problems.append(f"{identifier}: listed more than once")
        if len(cells) < 4:
            problems.append(f"{identifier}: malformed status row")
            continue
        seen[identifier] = (cells[2], backticked(cells[3]))
    for identifier in EXPECTED_IDS:
        if identifier not in seen:
            problems.append(f"{identifier}: missing from the status table")
    for identifier in sorted(set(seen) - set(EXPECTED_IDS)):
        problems.append(f"{identifier}: not an acceptance case of the specification")
    bodies = sections(text)
    for identifier, (status, cited) in sorted(seen.items()):
        if status not in STATUSES:
            problems.append(f"{identifier}: status {status!r} is not one of {sorted(STATUSES)}")
            continue
        if identifier not in bodies:
            problems.append(f"{identifier}: no '### {identifier}' section")
        for name in cited:
            if name not in logs:
                problems.append(f"{identifier}: cites {name}, which is not in the evidence table")
        if status in {"PASS", "FAIL"} and not cited:
            problems.append(f"{identifier}: {status} without an evidence log")
        if status != "PASS":
            continue
        failing = [name for name in cited if name in logs and logs[name][0] != "0"]
        if failing:
            problems.append(f"{identifier}: PASS cites logs that did not exit 0: {failing}")
        passed = set().union(*(logs[name][1] for name in cited if name in logs))
        for test in cited_tests(bodies.get(identifier, "")):
            missing = [name for name in test_names(test) if name not in passed]
            if missing:
                problems.append(f"{identifier}: {test} has no passing run in the cited logs ({', '.join(missing)})")
    return problems


def main(arguments: list[str]) -> int:
    root = Path(__file__).resolve().parents[1]
    document = Path(arguments[0]) if arguments else root / "docs/validation/VALIDATION.md"
    evidence_dir = Path(arguments[1]) if len(arguments) > 1 else document.parent / "evidence"
    problems = check(document, evidence_dir)
    for problem in problems:
        print(f"check_validation: {problem}", file=sys.stderr)
    if not problems:
        print(f"check_validation: {document} is consistent with {evidence_dir}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
