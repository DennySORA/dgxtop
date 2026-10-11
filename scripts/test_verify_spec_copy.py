"""Unit tests for scripts/verify_spec_copy.py (run: python3 -m unittest discover -s scripts -p 'test_*.py')."""
import hashlib
import json
import shutil
import tempfile
import unittest
import zipfile
from pathlib import Path

import verify_spec_copy

FILES = {"SPEC.md": b"# spec\n", "contracts/command.schema.json": b"{}\n"}
NOTHING_LEFT_OUT: frozenset[str] = frozenset()


class VerifySpecCopyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.spec = self.root / "spec"
        self.archive = self.root / "package.zip"
        self.build(FILES)

    def tearDown(self) -> None:
        self.directory.cleanup()

    def build(self, package: dict[str, bytes], left_out: frozenset[str] = NOTHING_LEFT_OUT) -> None:
        """Writes the package with its MANIFEST as a ZIP, and the copy without `left_out`."""
        manifest = {
            "file_count_including_manifest": len(package) + 1,
            "files": {
                name: {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}
                for name, content in package.items()
            },
        }
        self.contents = dict(package)
        self.contents["MANIFEST.json"] = json.dumps(manifest).encode()
        shutil.rmtree(self.spec, ignore_errors=True)
        for name, content in self.contents.items():
            if name in left_out:
                continue
            path = self.spec / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        with zipfile.ZipFile(self.archive, "w") as archive:
            for name, content in self.contents.items():
                archive.writestr(f"dgxtop-spec-v2/{name}", content)

    def problems(self, removed: frozenset[str] = NOTHING_LEFT_OUT) -> list[str]:
        problems, _ = verify_spec_copy.archive_problems(self.archive, self.spec, removed)
        return problems + verify_spec_copy.manifest_problems(self.spec, removed)

    def test_an_identical_copy_passes(self) -> None:
        self.assertEqual(self.problems(), [])
        self.assertEqual(verify_spec_copy.main([str(self.archive), str(self.spec)], NOTHING_LEFT_OUT), 0)

    def test_an_edited_copy_fails_both_the_package_and_the_manifest_check(self) -> None:
        (self.spec / "SPEC.md").write_bytes(b"# relaxed spec\n")
        problems = "\n".join(self.problems())
        self.assertIn("differs from the package: SPEC.md", problems)
        self.assertIn("SPEC.md: SHA-256 differs from MANIFEST", problems)
        self.assertEqual(verify_spec_copy.main([str(self.archive), str(self.spec)], NOTHING_LEFT_OUT), 1)

    def test_extra_and_missing_files_are_reported(self) -> None:
        (self.spec / "NOTES.md").write_bytes(b"extra\n")
        (self.spec / "contracts/command.schema.json").unlink()
        problems = "\n".join(self.problems())
        self.assertIn("not in the package: NOTES.md", problems)
        self.assertIn("missing from the copy: contracts/command.schema.json", problems)
        self.assertIn("MANIFEST lists a missing file: contracts/command.schema.json", problems)

    def test_a_wrong_manifest_count_is_reported(self) -> None:
        manifest = json.loads(self.contents["MANIFEST.json"])
        manifest["file_count_including_manifest"] = 9
        (self.spec / "MANIFEST.json").write_text(json.dumps(manifest))
        self.assertIn("MANIFEST counts 9 files", "\n".join(verify_spec_copy.manifest_problems(self.spec, NOTHING_LEFT_OUT)))

    def test_member_paths_outside_the_package_root_are_rejected(self) -> None:
        with zipfile.ZipFile(self.archive, "a") as archive:
            archive.writestr("elsewhere/evil.md", b"x")
        self.assertIn("unexpected member path: elsewhere/evil.md", "\n".join(self.problems()))

    def test_files_left_out_by_d59_may_be_absent_from_the_copy(self) -> None:
        left_out = frozenset({"AGENTS.md", "validation/model-tests.log"})
        self.build({**FILES, "AGENTS.md": b"# rules\n", "validation/model-tests.log": b"ok\n"}, left_out)
        self.assertEqual(self.problems(left_out), [])
        self.assertEqual(verify_spec_copy.main([str(self.archive), str(self.spec)], left_out), 0)

    def test_a_left_out_file_that_is_still_in_the_copy_fails_both_checks(self) -> None:
        left_out = frozenset({"AGENTS.md"})
        self.build({**FILES, "AGENTS.md": b"# rules\n"})
        problems = self.problems(left_out)
        self.assertEqual(problems.count("removed from the copy (D-59) but present: AGENTS.md"), 2)
        self.assertEqual(verify_spec_copy.main([str(self.archive), str(self.spec)], left_out), 1)

    def test_a_left_out_file_the_package_does_not_have_is_reported(self) -> None:
        problems = self.problems(frozenset({"NOTES.md"}))
        self.assertEqual(problems, ["listed as removed but not in MANIFEST: NOTES.md"])

    def test_leaving_a_file_out_does_not_excuse_an_edited_one(self) -> None:
        left_out = frozenset({"AGENTS.md"})
        self.build({**FILES, "AGENTS.md": b"# rules\n"}, left_out)
        (self.spec / "SPEC.md").write_bytes(b"# relaxed spec\n")
        problems = "\n".join(self.problems(left_out))
        self.assertIn("differs from the package: SPEC.md", problems)
        self.assertIn("SPEC.md: SHA-256 differs from MANIFEST", problems)

    def test_the_repository_copy_matches_its_manifest_except_the_left_out_files(self) -> None:
        spec = Path(__file__).resolve().parents[1] / "spec" / "dgxtop-v2"
        self.assertEqual(verify_spec_copy.manifest_problems(spec), [])


if __name__ == "__main__":
    unittest.main()
