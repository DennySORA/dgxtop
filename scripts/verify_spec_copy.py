#!/usr/bin/env python3
"""Verify the in-repository spec copy against the delivered package (D-02, AT40).

Usage: scripts/verify_spec_copy.py dgxtop-spec-v2.zip [spec/dgxtop-v2]

The package is kept in history, not in the tree (D-58):
git show 0a348c0:dgxtop-spec-v2.zip > target/dgxtop-spec-v2.zip

Checks every ZIP member's CRC, that each member file is byte-identical to the
copy under the spec directory, that the copy has no extra files, and that every
file listed in the copy's MANIFEST.json has the recorded size and SHA-256. The
copy leaves out the package files in REMOVED_FROM_COPY (D-59); each of them must
be part of the package and absent from the copy.
Only local files are read. Exit status 0 means every check held.
"""
import hashlib
import json
import sys
import zipfile
from pathlib import Path

ROOT_PREFIX = "dgxtop-spec-v2/"

# Package files the repository copy leaves out (D-59): the agent instructions,
# the generated HTML edition and the package's own validator and reference
# models. They stay in the delivered archive.
REMOVED_FROM_COPY = frozenset({
    "AGENTS.md",
    "CHANGELOG.md",
    "CLAUDE.md",
    "IMPLEMENTATION_PROMPT.md",
    "VALIDATION.md",
    "baseline/EXISTING_RULES.md",
    "index.html",
    "validation/browser-results.json",
    "validation/browser_check.py",
    "validation/build_html.py",
    "validation/model-tests.log",
    "validation/reference_model.py",
    "validation/requirements-browser.txt",
    "validation/runtime_model.py",
    "validation/seal_package.py",
    "validation/test_contracts.py",
    "validation/test_reference_model.py",
    "validation/test_runtime.py",
    "validation/validate_package.py",
})


def manifest_problems(spec_dir: Path, removed: frozenset[str] = REMOVED_FROM_COPY) -> list[str]:
    """MANIFEST.json maps each file to its byte count and SHA-256; it does not
    list itself, but its file count includes it."""
    manifest = json.loads((spec_dir / "MANIFEST.json").read_text(encoding="utf-8"))
    files = manifest.get("files", {})
    problems = []
    if manifest.get("file_count_including_manifest") != len(files) + 1:
        problems.append(f"MANIFEST counts {manifest.get('file_count_including_manifest')} files but lists {len(files)} + itself")
    for unknown in sorted(removed - set(files)):
        problems.append(f"listed as removed but not in MANIFEST: {unknown}")
    for relative, entry in sorted(files.items()):
        path = spec_dir / relative
        if relative in removed:
            if path.exists():
                problems.append(f"removed from the copy (D-59) but present: {relative}")
            continue
        if not path.is_file():
            problems.append(f"MANIFEST lists a missing file: {relative}")
            continue
        content = path.read_bytes()
        if len(content) != entry.get("bytes"):
            problems.append(f"{relative}: {len(content)} bytes, MANIFEST says {entry.get('bytes')}")
        if hashlib.sha256(content).hexdigest() != entry.get("sha256"):
            problems.append(f"{relative}: SHA-256 differs from MANIFEST")
    return problems


def archive_problems(
    archive_path: Path, spec_dir: Path, removed: frozenset[str] = REMOVED_FROM_COPY
) -> tuple[list[str], int]:
    problems = []
    with zipfile.ZipFile(archive_path) as archive:
        broken = archive.testzip()
        if broken is not None:
            problems.append(f"CRC mismatch in {broken}")
        members = [info for info in archive.infolist() if not info.is_dir()]
        expected = set()
        for info in members:
            if not info.filename.startswith(ROOT_PREFIX) or ".." in Path(info.filename).parts:
                problems.append(f"unexpected member path: {info.filename}")
                continue
            relative = info.filename[len(ROOT_PREFIX):]
            expected.add(relative)
            copy = spec_dir / relative
            if relative in removed:
                if copy.exists():
                    problems.append(f"removed from the copy (D-59) but present: {relative}")
            elif not copy.is_file():
                problems.append(f"missing from the copy: {relative}")
            elif copy.read_bytes() != archive.read(info):
                problems.append(f"differs from the package: {relative}")
    present = {str(path.relative_to(spec_dir)) for path in spec_dir.rglob("*") if path.is_file()}
    for extra in sorted(present - expected):
        problems.append(f"not in the package: {extra}")
    return problems, len(members)


def main(arguments: list[str], removed: frozenset[str] = REMOVED_FROM_COPY) -> int:
    if not arguments:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    archive_path = Path(arguments[0])
    spec_dir = Path(arguments[1]) if len(arguments) > 1 else Path("spec/dgxtop-v2")
    problems, count = archive_problems(archive_path, spec_dir, removed)
    problems += manifest_problems(spec_dir, removed)
    for problem in problems:
        print(f"verify_spec_copy: {problem}", file=sys.stderr)
    if not problems:
        left_out = f" except the {len(removed)} files D-59 leaves out" if removed else ""
        print(f"verify_spec_copy: {count} members, CRC ok, byte-identical to {spec_dir}{left_out}, MANIFEST hashes ok")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
