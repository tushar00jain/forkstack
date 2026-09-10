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


if __name__ == "__main__":
    unittest.main()
