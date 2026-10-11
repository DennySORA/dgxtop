"""Unit tests for scripts/record_evidence.sh (run: python3 -m unittest discover -s scripts -p 'test_*.py')."""
import hashlib
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "record_evidence.sh"
UUID = "GPU-0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"


class RecordEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        (self.root / "scripts").mkdir()
        shutil.copy2(SCRIPT, self.root / "scripts" / "record_evidence.sh")
        self.evidence = self.root / "docs" / "validation" / "evidence"

    def tearDown(self) -> None:
        self.directory.cleanup()

    def record(self, name: str, *command: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(self.root / "scripts" / "record_evidence.sh"), name, *command],
            capture_output=True,
            text=True,
            cwd=self.root,
        )

    def test_gpu_uuids_and_serial_numbers_are_redacted(self) -> None:
        output = f'{UUID}\nkey: "serial", value: "1650123456789"\n"serial": "ABC123"\n'
        result = self.record("probe", "printf", output)
        self.assertEqual(result.returncode, 0, result.stderr)
        log = (self.evidence / "probe.log").read_text()
        self.assertNotIn(UUID, log)
        self.assertNotIn("1650123456789", log)
        self.assertNotIn("ABC123", log)
        self.assertIn("GPU-<redacted>", log)
        self.assertIn('key: "serial", value: "<redacted>"', log)
        self.assertIn('"serial": "<redacted>"', log)

    def test_the_command_exit_code_is_recorded_and_returned(self) -> None:
        result = self.record("failing", "bash", "-c", "echo partial output; exit 3")
        self.assertEqual(result.returncode, 3)
        log = (self.evidence / "failing.log").read_text()
        self.assertIn("partial output", log)
        self.assertIn("# exit_code: 3", log)

    def test_the_printed_hash_is_the_hash_of_the_stored_log(self) -> None:
        result = self.record("hashed", "echo", "hello")
        digest = hashlib.sha256((self.evidence / "hashed.log").read_bytes()).hexdigest()
        self.assertIn(f"sha256={digest}", result.stdout)

    def test_only_the_log_is_written(self) -> None:
        self.record("tidy", "echo", "hello")
        self.assertEqual(sorted(path.name for path in self.evidence.iterdir()), ["tidy.log"])


if __name__ == "__main__":
    unittest.main()
