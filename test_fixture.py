import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts.create_fixture_repo import create_fixture, git


class FixtureRepositoryTest(unittest.TestCase):
    def test_creates_expected_three_commit_stacks(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = create_fixture(Path(directory) / "fixture")

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
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "not-a-fixture"
            root.mkdir()

            with self.assertRaisesRegex(RuntimeError, "fixture marker is missing"):
                create_fixture(root, force=True)

    def test_beta_two_conflicts_when_inserted_after_alpha_two(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = create_fixture(Path(directory) / "fixture")
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
                text=True,
                capture_output=True,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(
                git(repo, "diff", "--name-only", "--diff-filter=U"),
                "shared.txt",
            )


if __name__ == "__main__":
    unittest.main()
