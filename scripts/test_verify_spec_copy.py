"""Unit tests for scripts/verify_spec_copy.py (run: python3 -m unittest discover -s scripts -p 'test_*.py')."""
import hashlib
import json
import tempfile
import unittest
import zipfile
from pathlib import Path

import verify_spec_copy

FILES = {"SPEC.md": b"# spec\n", "contracts/command.schema.json": b"{}\n"}


class VerifySpecCopyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.spec = self.root / "spec"
        manifest = {
            "file_count_including_manifest": len(FILES) + 1,
            "files": {
                name: {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}
                for name, content in FILES.items()
            },
        }
        self.contents = dict(FILES)
        self.contents["MANIFEST.json"] = json.dumps(manifest).encode()
        for name, content in self.contents.items():
            path = self.spec / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        self.archive = self.root / "package.zip"
        with zipfile.ZipFile(self.archive, "w") as archive:
            for name, content in self.contents.items():
                archive.writestr(f"dgxtop-spec-v2/{name}", content)

    def tearDown(self) -> None:
        self.directory.cleanup()

    def problems(self) -> list[str]:
        problems, _ = verify_spec_copy.archive_problems(self.archive, self.spec)
        return problems + verify_spec_copy.manifest_problems(self.spec)

    def test_an_identical_copy_passes(self) -> None:
        self.assertEqual(self.problems(), [])
        self.assertEqual(verify_spec_copy.main([str(self.archive), str(self.spec)]), 0)

    def test_an_edited_copy_fails_both_the_package_and_the_manifest_check(self) -> None:
        (self.spec / "SPEC.md").write_bytes(b"# relaxed spec\n")
        problems = "\n".join(self.problems())
        self.assertIn("differs from the package: SPEC.md", problems)
        self.assertIn("SPEC.md: SHA-256 differs from MANIFEST", problems)
        self.assertEqual(verify_spec_copy.main([str(self.archive), str(self.spec)]), 1)

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
        self.assertIn("MANIFEST counts 9 files", "\n".join(verify_spec_copy.manifest_problems(self.spec)))

    def test_member_paths_outside_the_package_root_are_rejected(self) -> None:
        with zipfile.ZipFile(self.archive, "a") as archive:
            archive.writestr("elsewhere/evil.md", b"x")
        self.assertIn("unexpected member path: elsewhere/evil.md", "\n".join(self.problems()))


if __name__ == "__main__":
    unittest.main()
