"""Unit tests for scripts/format_run_log.py (run: python3 -m unittest discover -s scripts -p 'test_*.py')."""
import unittest

import check_validation
from format_run_log import format_lines

RAW = [
    "release gate\tRun cargo test\t2026-10-11T04:05:12.1234567Z running 2 tests\n",
    "release gate\tRun cargo test\t2026-10-11T04:05:12.2Z test cli::installer::wsl1_hosts_are_refused ... ok\n",
    "release gate\tRun cargo test\t2026-10-11T04:05:12.3Z test hardware::hardware_nvml_reads ... \x1b[33mignored\x1b[0m\n",
    "build x86_64\tPackage\t2026-10-11T04:06:00.0Z \x1b[36;1mtar -czf dgxtop.tar.gz dgxtop\x1b[0m\n",
]


class FormatRunLogTests(unittest.TestCase):
    def test_each_job_and_step_gets_one_heading_and_lines_lose_timestamps_and_colour(self) -> None:
        self.assertEqual(
            list(format_lines(RAW)),
            [
                "## release gate / Run cargo test",
                "running 2 tests",
                "test cli::installer::wsl1_hosts_are_refused ... ok",
                "test hardware::hardware_nvml_reads ... ignored",
                "## build x86_64 / Package",
                "tar -czf dgxtop.tar.gz dgxtop",
            ],
        )

    def test_check_validation_reads_the_passing_tests(self) -> None:
        text = "\n".join(format_lines(RAW))
        self.assertEqual(check_validation.passing_tests(text), {"wsl1_hosts_are_refused"})

    def test_lines_without_the_github_prefix_are_kept(self) -> None:
        self.assertEqual(list(format_lines(["plain line\n", ""])), ["plain line", ""])

    def test_an_empty_log_stays_empty(self) -> None:
        self.assertEqual(list(format_lines([])), [])


if __name__ == "__main__":
    unittest.main()
