"""Unit tests for scripts/check_validation.py (run: python3 -m unittest discover -s scripts -p 'test_*.py')."""
import hashlib
import tempfile
import unittest
from pathlib import Path

import check_validation

GATE_LOG = """# evidence: gate-test
# command: cargo test --workspace --all-features --locked
# ---- output ----
test scheduler::tests::clock_jump_schedules_only_the_next_future_slot ... ok
test arbitration::tests::pin_never_hides_a_conflict ... ok
test arbitration::tests::outcome_is_independent_of_candidate_order ... ok
test_rejects_synthetic_label (test_tools.ToolsTests.test_rejects_synthetic_label) ... ok
test hardware_nvml_smoke ... ignored
# ---- end ----
# exit_code: 0
"""

FAILED_LOG = """# evidence: bench-ref
# ---- output ----
coverage_percent 99.0 >= 99.9 Fail
# ---- end ----
# exit_code: 1
"""


def digest(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


class CheckValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.evidence = self.root / "evidence"
        self.evidence.mkdir()
        self.logs = {"gate-test": GATE_LOG, "bench-ref": FAILED_LOG}
        self.rows = {identifier: ("NOT_RUN", "") for identifier in check_validation.EXPECTED_IDS}
        self.rows["AT03"] = ("PASS", "`gate-test`")
        self.rows["AT28"] = ("FAIL", "`bench-ref`")
        self.tests = {
            "AT03": "`dgxtop-collectors::arbitration::tests::{pin_never_hides_a_conflict, outcome_is_independent_of_candidate_order}`, "
            "`scripts/test_tools.py::ToolsTests::test_rejects_synthetic_label`",
            "AT22": "`dgxtop-manager::scheduler::tests::clock_jump_schedules_only_the_next_future_slot`",
        }

    def tearDown(self) -> None:
        self.directory.cleanup()

    def write(self, hashes: dict | None = None, exits: dict | None = None) -> Path:
        for name, text in self.logs.items():
            (self.evidence / f"{name}.log").write_text(text)
        hashes = hashes or {}
        exits = exits or {}
        lines = ["# Validation", "", "## Evidence logs", "", "| Evidence | Command | Exit | SHA-256 |", "|---|---|---|---|"]
        for name, text in self.logs.items():
            exit_code = exits.get(name, "0" if name == "gate-test" else "1")
            lines.append(f"| `{name}` | `cmd` | {exit_code} | `{hashes.get(name, digest(text))}` |")
        lines += ["", "## Acceptance status", "", "| AT | Requirement | Status | Evidence | Summary |", "|---|---|---|---|---|"]
        for identifier, (status, cited) in self.rows.items():
            lines.append(f"| [{identifier}](#{identifier.lower()}) | R | {status} | {cited} | s |")
        lines += ["", "## Details", ""]
        for identifier in self.rows:
            lines += [f"### {identifier} — title", "", "| Clause | Tests | Note |", "|---|---|---|"]
            lines.append(f"| S1 | {self.tests.get(identifier, '')} | see `Type::method` |")
            lines.append("")
        document = self.root / "VALIDATION.md"
        document.write_text("\n".join(lines) + "\n")
        return document

    def problems(self, **arguments) -> list[str]:
        return check_validation.check(self.write(**arguments), self.evidence)

    def test_a_consistent_report_passes(self) -> None:
        self.assertEqual(self.problems(), [])
        self.assertEqual(check_validation.main([str(self.write()), str(self.evidence)]), 0)

    def test_missing_duplicate_and_orphan_ids_are_rejected(self) -> None:
        del self.rows["AT40"]
        self.rows["AT99"] = ("NOT_RUN", "")
        problems = "\n".join(self.problems())
        self.assertIn("AT40: missing from the status table", problems)
        self.assertIn("AT99: not an acceptance case", problems)
        document = self.write()
        text = document.read_text().replace("| [AT02](#at02)", "| [AT01](#at01)")
        document.write_text(text)
        self.assertIn("AT01: listed more than once", "\n".join(check_validation.check(document, self.evidence)))

    def test_unknown_status_and_pass_without_evidence_are_rejected(self) -> None:
        self.rows["AT01"] = ("PARTIAL", "")
        self.rows["AT02"] = ("PASS", "")
        problems = "\n".join(self.problems())
        self.assertIn("AT01: status 'PARTIAL'", problems)
        self.assertIn("AT02: PASS without an evidence log", problems)

    def test_wrong_hash_wrong_exit_and_missing_logs_are_rejected(self) -> None:
        problems = "\n".join(self.problems(hashes={"gate-test": "0" * 64}, exits={"bench-ref": "0"}))
        self.assertIn("gate-test: SHA-256", problems)
        self.assertIn("bench-ref: recorded exit code ['1'] is not the stated 0", problems)
        self.rows["AT05"] = ("BLOCKED", "`hardware-suites`")
        self.assertIn("AT05: cites hardware-suites", "\n".join(self.problems()))
        (self.evidence / "gate-test.log").write_text("")
        document = self.root / "VALIDATION.md"
        self.assertIn("gate-test: evidence log missing or empty", "\n".join(check_validation.check(document, self.evidence)))

    def test_pass_must_not_cite_a_failed_log(self) -> None:
        self.rows["AT03"] = ("PASS", "`gate-test`, `bench-ref`")
        self.assertIn("AT03: PASS cites logs that did not exit 0: ['bench-ref']", "\n".join(self.problems()))

    def test_pass_needs_every_cited_test_to_have_passed(self) -> None:
        self.tests["AT03"] += ", `dgxtop-collectors::arbitration::tests::renamed_test`"
        self.rows["AT22"] = ("PASS", "`gate-test`")
        self.tests["AT22"] += ", `dgxtop-adapters/adapters_b_nvml::hardware_nvml_smoke`"
        problems = "\n".join(self.problems())
        self.assertIn("AT03: dgxtop-collectors::arbitration::tests::renamed_test has no passing run", problems)
        self.assertIn("(hardware_nvml_smoke)", problems, "an ignored test is not a passing run")
        self.assertNotIn("Type::method", problems, "only the Tests column names tests")

    def test_custom_harness_lines_with_a_duration_count_as_passing(self) -> None:
        log = "\n".join(
            [
                "test healthy_round_trip_with_a_child_process ... ok (408 ms)",
                "test hung_host_is_restarted ... FAILED (1113 ms)",
                "test suffix_with_more_text ... ok (12 ms) later",
            ]
        )
        passing = check_validation.passing_tests(log)
        self.assertIn("healthy_round_trip_with_a_child_process", passing)
        self.assertNotIn("hung_host_is_restarted", passing, "a failed run is not a pass")
        self.assertNotIn("suffix_with_more_text", passing, "only the duration suffix is accepted")

    def workspace(self) -> Path:
        """A two-crate workspace: a library with modules, a crate with test targets."""
        root = self.root / "workspace"
        library = root / "crates/core"
        (library / "src/budget").mkdir(parents=True)
        (library / "Cargo.toml").write_text('[package]\nname = "dgxtop-core"\n')
        (library / "src/lib.rs").write_text("pub mod budget;\npub mod clock;\n#[cfg(test)]\nmod tests {}\n")
        (library / "src/budget/mod.rs").write_text("")
        (library / "src/clock.rs").write_text("")
        root_crate = root / "crates/dgxtop"
        (root_crate / "tests").mkdir(parents=True)
        (root_crate / "src").mkdir()
        (root_crate / "src/lib.rs").write_text("")
        (root_crate / "Cargo.toml").write_text(
            '[package]\nname = "dgxtop"\nautotests = false\n\n[[test]]\nname = "store_hot_commit"\npath = "x.rs"\n'
        )
        (root_crate / "tests/ignored.rs").write_text("")
        store = root / "crates/store"
        (store / "tests").mkdir(parents=True)
        (store / "Cargo.toml").write_text('[package]\nname = "dgxtop-store"\n')
        (store / "tests/store_persist_config.rs").write_text("")
        return root

    def test_cited_paths_resolve_in_their_crates(self) -> None:
        crates = check_validation.workspace_crates(self.workspace())
        resolves = [
            "dgxtop-core::budget::tests::a",
            "dgxtop-core::clock::tests::{a, b}",
            "dgxtop-core::tests::a",
            "dgxtop/store_hot_commit::a",
            "dgxtop-store/store_persist_config::a",
            "action_disposable::at10::a",
            "scripts/test_tools.py::ToolsTests::test_a",
        ]
        for identifier in resolves:
            self.assertIsNone(check_validation.unresolved_path(identifier, crates), identifier)
        self.assertEqual(
            check_validation.unresolved_path("dgxtop-store/store_hot_commit::a", crates),
            "dgxtop-store has no test target store_hot_commit",
        )
        self.assertEqual(
            check_validation.unresolved_path("dgxtop/ignored::a", crates),
            "dgxtop has no test target ignored",
            "autotests = false: only declared targets",
        )
        self.assertEqual(
            check_validation.unresolved_path("dgxtop-core::query::tests::a", crates),
            "dgxtop-core has no module query",
        )

    def test_a_moved_test_is_reported_in_any_section(self) -> None:
        self.tests["AT17"] = "`dgxtop-core::query::tests::moved`"
        document = self.write()
        problems = "\n".join(check_validation.check(document, self.evidence, self.workspace()))
        self.assertIn("AT17: dgxtop-core::query::tests::moved does not resolve (dgxtop-core has no module query)", problems)
        self.assertNotIn("does not resolve", "\n".join(check_validation.check(document, self.evidence)))

    def test_every_case_needs_a_section(self) -> None:
        document = self.write()
        document.write_text(document.read_text().replace("### AT17 — title", "### Something else"))
        self.assertIn("AT17: no '### AT17' section", "\n".join(check_validation.check(document, self.evidence)))


if __name__ == "__main__":
    unittest.main()
