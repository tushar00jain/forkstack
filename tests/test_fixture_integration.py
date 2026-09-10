"""Real-system integration tests for the disposable fixture generator."""

import subprocess
import shutil
import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path

from scripts.create_fixture_repo import create_fixture, git


INTEGRATION_OUTPUT = Path(__file__).resolve().parents[1] / "target" / "integration-fixtures"


@contextmanager
def integration_directory(prefix):
    INTEGRATION_OUTPUT.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix=prefix, dir=INTEGRATION_OUTPUT))
    try:
        yield directory
    finally:
        shutil.rmtree(directory, ignore_errors=True)


class FixtureRepositoryIntegrationTest(unittest.TestCase):
    def test_creates_expected_three_commit_stacks(self):
        with integration_directory("fixture-graph-") as directory:
            repo = create_fixture(directory / "fixture")

            self.assertEqual(git(repo, "rev-list", "--count", "main"), "6")
            self.assertEqual(
                git(
                    repo,
                    "rev-list",
                    "--count",
                    "origin/fs-base/alpha/1..fs-head/alpha/3",
                ),
                "3",
            )
            self.assertEqual(
                git(repo, "rev-list", "--count", "main..fs-head/beta/3"),
                "3",
            )
            self.assertEqual(
                git(
                    repo,
                    "rev-list",
                    "--count",
                    "fs-head/alpha/3..fs-head/gamma/3",
                ),
                "3",
            )
            self.assertEqual(
                git(repo, "rev-parse", "fs-head/alpha/2"),
                git(repo, "rev-parse", "origin/fs-head/alpha/2"),
            )

    def test_force_only_removes_a_generated_fixture(self):
        with integration_directory("fixture-safety-") as directory:
            root = directory / "not-a-fixture"
            root.mkdir()

            with self.assertRaisesRegex(RuntimeError, "fixture marker is missing"):
                create_fixture(root, force=True)

    def test_beta_two_conflicts_when_inserted_after_alpha_two(self):
        with integration_directory("fixture-conflict-") as directory:
            repo = create_fixture(directory / "fixture")
            git(repo, "switch", "--detach", "fs-head/beta/3")

            result = subprocess.run(
                [
                    "git",
                    "rebase",
                    "--update-refs",
                    "--onto",
                    "fs-head/alpha/2",
                    "fs-head/beta/1",
                ],
                cwd=repo,
                capture_output=True,
                text=True,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(
                git(repo, "diff", "--name-only", "--diff-filter=U"),
                "shared.txt",
            )


if __name__ == "__main__":
    unittest.main()
