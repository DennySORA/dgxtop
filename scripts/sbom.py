#!/usr/bin/env python3
"""Emit a CycloneDX 1.5 SBOM (JSON) for dgxtop from the committed Cargo.lock.

Usage: scripts/sbom.py Cargo.lock vX.Y.Z [metadata.json] > dgxtop-vX.Y.Z.cdx.json

Only local files are read: no network access, no cargo invocation. Registry
packages carry their lock-file SHA-256 checksum; workspace members carry none.
The optional `cargo metadata --format-version 1 --locked` output adds each
package's declared SPDX license expression (the lock file has no licenses).
"""
import json
import sys
import tomllib
import uuid
from pathlib import Path

WORKSPACE_PREFIX = "dgxtop"


def declared_licenses(metadata: dict) -> dict:
    """(name, version) -> SPDX expression declared by each registry package."""
    licenses = {}
    for package in metadata.get("packages", []):
        expression = package.get("license")
        if package.get("source") and expression:
            # Old crates write "MIT/Apache-2.0"; SPDX spells it with OR.
            licenses[(package["name"], package["version"])] = expression.replace("/", " OR ")
    return licenses


def component(package: dict, licenses: dict) -> dict:
    name = package["name"]
    version = package["version"]
    entry = {
        "type": "library",
        "bom-ref": f"pkg:cargo/{name}@{version}",
        "name": name,
        "version": version,
        "purl": f"pkg:cargo/{name}@{version}",
    }
    checksum = package.get("checksum")
    if checksum:
        entry["hashes"] = [{"alg": "SHA-256", "content": checksum}]
    expression = licenses.get((name, version))
    if expression:
        entry["licenses"] = [{"expression": expression}]
    if name == "libsqlite3-sys":
        # The bundled amalgamation is what actually runs; the runtime gate checks >= 3.51.3.
        entry["properties"] = [{"name": "dgxtop:sqlite-bundled", "value": "true"}]
    return entry


def build(lock_path: Path, tag: str, metadata_path: Path | None = None) -> dict:
    lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    licenses = {}
    if metadata_path is not None:
        licenses = declared_licenses(json.loads(metadata_path.read_text(encoding="utf-8")))
    packages = sorted(lock.get("package", []), key=lambda item: (item["name"], item["version"]))
    third_party = [package for package in packages if not package["name"].startswith(WORKSPACE_PREFIX)]
    version = tag.removeprefix("v")
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, f'dgxtop/{tag}')}",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "bom-ref": f"pkg:cargo/dgxtop@{version}",
                "name": "dgxtop",
                "version": version,
                "licenses": [{"license": {"id": "Apache-2.0"}}],
            }
        },
        "components": [component(package, licenses) for package in third_party],
    }


def main() -> int:
    if len(sys.argv) not in (3, 4):
        print(__doc__, file=sys.stderr)
        return 2
    metadata = Path(sys.argv[3]) if len(sys.argv) == 4 else None
    json.dump(build(Path(sys.argv[1]), sys.argv[2], metadata), sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
