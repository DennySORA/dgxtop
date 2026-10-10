"""Unit tests for scripts/sbom.py (run: python3 -m unittest discover -s scripts -p 'test_*.py')."""
import json
import tempfile
import unittest
from pathlib import Path

import sbom

LOCK = """
version = 4

[[package]]
name = "dgxtop-core"
version = "0.2.0"

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "9a8e94ea7f378bd32cbbd37198a4a91436180c5bb472411e48b5ec2e2124ae9e"

[[package]]
name = "libsqlite3-sys"
version = "0.37.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "95b4103cffefa72eb8428cb6b47d6627161e51c2739fc5e3b734584157bc642a"
"""

METADATA = {
    "packages": [
        {"name": "serde", "version": "1.0.228", "source": "registry+x", "license": "MIT/Apache-2.0"},
        {"name": "libsqlite3-sys", "version": "0.37.0", "source": "registry+x", "license": None},
        {"name": "dgxtop-core", "version": "0.2.0", "source": None, "license": "Apache-2.0"},
    ]
}


class SbomTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        root = Path(self.directory.name)
        self.lock = root / "Cargo.lock"
        self.lock.write_text(LOCK, encoding="utf-8")
        self.metadata = root / "metadata.json"
        self.metadata.write_text(json.dumps(METADATA), encoding="utf-8")

    def tearDown(self) -> None:
        self.directory.cleanup()

    def components(self, metadata: Path | None) -> dict:
        document = sbom.build(self.lock, "v0.2.0", metadata)
        return {component["name"]: component for component in document["components"]}

    def test_registry_packages_carry_checksums_and_workspace_members_are_excluded(self) -> None:
        components = self.components(None)
        self.assertEqual(sorted(components), ["libsqlite3-sys", "serde"])
        self.assertEqual(components["serde"]["hashes"][0]["alg"], "SHA-256")
        self.assertEqual(components["serde"]["purl"], "pkg:cargo/serde@1.0.228")

    def test_declared_licenses_are_added_as_spdx_expressions(self) -> None:
        components = self.components(self.metadata)
        self.assertEqual(components["serde"]["licenses"], [{"expression": "MIT OR Apache-2.0"}])

    def test_missing_license_data_is_left_out_instead_of_guessed(self) -> None:
        self.assertNotIn("licenses", self.components(None)["serde"])
        self.assertNotIn("licenses", self.components(self.metadata)["libsqlite3-sys"])

    def test_bundled_sqlite_is_marked(self) -> None:
        properties = self.components(None)["libsqlite3-sys"]["properties"]
        self.assertIn({"name": "dgxtop:sqlite-bundled", "value": "true"}, properties)

    def test_the_document_is_deterministic_for_a_tag(self) -> None:
        first = sbom.build(self.lock, "v0.2.0", self.metadata)
        second = sbom.build(self.lock, "v0.2.0", self.metadata)
        self.assertEqual(first, second)
        self.assertEqual(first["metadata"]["component"]["version"], "0.2.0")


if __name__ == "__main__":
    unittest.main()
