import argparse
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import forkstack


def git(repo, *args, input=None):
    return subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        text=True,
        input=input,
        capture_output=True,
    ).stdout.strip()


class ForkstackTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.repo = root / "repo"
        self.remote = root / "remote.git"
        git(root, "init", "--bare", str(self.remote))
        git(root, "init", "-b", "main", str(self.repo))
        git(self.repo, "config", "user.name", "Fork Stack")
        git(self.repo, "config", "user.email", "forkstack@example.com")
        git(self.repo, "remote", "add", "origin", str(self.remote))

        self.base = self.commit("base.txt", "base\n", "base")
        self.first = self.commit("first.txt", "first\n", "first change")
        git(self.repo, "branch", "first", self.first)
        self.second = self.commit("second.txt", "second\n", "second change")
        git(self.repo, "branch", "stack", self.second)
        git(self.repo, "push", "origin", f"{self.base}:refs/heads/main")
        git(self.repo, "push", "origin", f"{self.first}:refs/heads/draft/1")
        git(self.repo, "push", "origin", f"{self.second}:refs/heads/draft/2")
        git(
            self.repo,
            "fetch",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        )

    def tearDown(self):
        self.temp.cleanup()

    def commit(self, name, contents, message):
        (self.repo / name).write_text(contents)
        git(self.repo, "add", name)
        git(self.repo, "commit", "-m", message)
        return git(self.repo, "rev-parse", "HEAD")

    def test_identity_follows_change_when_reordered(self):
        revs = git(
            self.repo, "rev-list", "--reverse", "origin/main..stack"
        ).splitlines()
        plan = forkstack.assign_branches(
            str(self.repo), revs, "origin", "draft"
        )
        self.assertEqual([step.branch for step in plan], ["draft/1", "draft/2"])

        git(self.repo, "switch", "stack")
        forkstack.rewrite_with_identities(str(self.repo), plan)
        first, second = [step.rev for step in plan]

        git(self.repo, "switch", "--detach", self.base)
        git(self.repo, "cherry-pick", second)
        reordered_second = git(self.repo, "rev-parse", "HEAD")
        git(self.repo, "cherry-pick", first)
        reordered_first = git(self.repo, "rev-parse", "HEAD")

        reordered = forkstack.assign_branches(
            str(self.repo),
            [reordered_second, reordered_first],
            "origin",
            "draft",
        )
        self.assertEqual(
            [step.branch for step in reordered], ["draft/2", "draft/1"]
        )
        self.assertFalse(any(step.identity_added for step in reordered))

    def test_new_commit_uses_next_unused_identity(self):
        revs = git(
            self.repo, "rev-list", "--reverse", "origin/main..stack"
        ).splitlines()
        plan = forkstack.assign_branches(
            str(self.repo), revs, "origin", "draft"
        )
        git(self.repo, "switch", "stack")
        forkstack.rewrite_with_identities(str(self.repo), plan)
        self.commit("third.txt", "third\n", "third change")

        revs = git(
            self.repo, "rev-list", "--reverse", "origin/main..HEAD"
        ).splitlines()
        updated = forkstack.assign_branches(
            str(self.repo), revs, "origin", "draft"
        )
        self.assertEqual(
            [step.branch for step in updated],
            ["draft/1", "draft/2", "draft/3"],
        )

    def test_mixed_identities_are_preserved(self):
        revs = git(
            self.repo, "rev-list", "--reverse", "origin/main..stack"
        ).splitlines()
        plan = forkstack.assign_branches(
            str(self.repo), revs, "origin", "draft"
        )
        git(self.repo, "switch", "stack")
        forkstack.rewrite_with_identities(str(self.repo), plan)
        self.commit(
            "imported.txt",
            "imported\n",
            "imported change\n\nfs-branch: other/6",
        )
        self.commit("new.txt", "new\n", "new change")

        revs = git(
            self.repo, "rev-list", "--reverse", "origin/main..HEAD"
        ).splitlines()
        updated = forkstack.assign_branches(
            str(self.repo), revs, "origin", "draft"
        )

        self.assertEqual(
            [step.branch for step in updated],
            ["draft/1", "draft/2", "other/6", "draft/3"],
        )
        self.assertEqual(
            [step.identity_added for step in updated],
            [False, False, False, True],
        )

    def test_untagged_commit_requires_prefix(self):
        revs = git(
            self.repo, "rev-list", "--reverse", "origin/main..stack"
        ).splitlines()

        with self.assertRaisesRegex(RuntimeError, "untagged commits require --prefix"):
            forkstack.assign_branches(str(self.repo), revs, "origin", None)

    def test_refuses_to_overwrite_divergent_local_head_branch(self):
        git(
            self.repo,
            "push",
            "origin",
            f"{self.first}:refs/heads/fs-head/draft/1",
        )
        git(
            self.repo,
            "fetch",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        )
        git(self.repo, "branch", "fs-head/draft/1", self.second)
        plan = [
            forkstack.StackCommit(
                rev=self.first,
                branch="draft/1",
                subject="first change",
                body="",
                message="first change",
            )
        ]

        with self.assertRaisesRegex(RuntimeError, "divergent local branch"):
            forkstack.sync_local_head_branches(
                str(self.repo), plan, "origin"
            )

    def test_execute_creates_fresh_prs_then_restacks_them(self):
        prs = {}
        pushes = []
        actual_run = forkstack.run

        def fake_run(cmd, cwd=None, capture=True, input=None, env=None):
            if cmd[:3] == ["gh", "pr", "list"]:
                branch = cmd[cmd.index("--head") + 1]
                return json.dumps([prs[branch]] if branch in prs else [])
            if cmd[:3] == ["gh", "pr", "edit"]:
                number = int(cmd[3])
                pr = next(value for value in prs.values() if value["number"] == number)
                for option, key in (
                    ("--base", "baseRefName"),
                    ("--title", "title"),
                    ("--body", "body"),
                ):
                    if option in cmd:
                        pr[key] = cmd[cmd.index(option) + 1]
                return ""
            if cmd[:3] == ["gh", "pr", "create"]:
                branch = cmd[cmd.index("--head") + 1]
                number = 100 + len(prs) + 1
                prs[branch] = {
                    "number": number,
                    "baseRefName": cmd[cmd.index("--base") + 1],
                    "title": cmd[cmd.index("--title") + 1],
                    "body": cmd[cmd.index("--body") + 1],
                }
                return f"https://github.com/example/repo/pull/{number}"
            if cmd[:2] == ["git", "push"]:
                pushes.append(list(cmd))
            return actual_run(
                cmd, cwd=cwd, capture=capture, input=input, env=env
            )

        args = argparse.Namespace(
            repo=str(self.repo),
            remote="origin",
            base="main",
            prefix="draft",
            draft=True,
            execute=True,
        )
        git(self.repo, "switch", "stack")
        with mock.patch.object(forkstack, "run", side_effect=fake_run):
            forkstack.cmd_submit(args)

        seeded = git(
            self.repo, "rev-list", "--reverse", "origin/main..stack"
        ).splitlines()
        self.assertEqual(
            [
                forkstack.identity_from_message(
                    git(self.repo, "show", "-s", "--format=%B", rev)
                )
                for rev in seeded
            ],
            ["draft/1", "draft/2"],
        )
        self.assertEqual(
            prs["fs-head/draft/1"]["baseRefName"],
            "fs-base/draft/1",
        )
        self.assertEqual(
            prs["fs-head/draft/2"]["baseRefName"],
            "fs-base/draft/2",
        )
        first, second = seeded
        self.assertEqual(
            git(self.repo, "rev-parse", "fs-head/draft/1"), first
        )
        self.assertEqual(
            git(self.repo, "rev-parse", "fs-head/draft/2"), second
        )
        self.assertEqual(
            git(self.repo, "config", "branch.fs-head/draft/1.remote"),
            "origin",
        )
        self.assertEqual(
            git(self.repo, "config", "branch.fs-head/draft/1.merge"),
            "refs/heads/fs-head/draft/1",
        )
        self.assertEqual(git(self.repo, "rev-parse", "origin/draft/1"), self.first)
        self.assertEqual(git(self.repo, "rev-parse", "origin/draft/2"), self.second)

        git(self.repo, "switch", "--detach", self.base)
        git(self.repo, "cherry-pick", second)
        reordered_second = git(self.repo, "rev-parse", "HEAD")
        git(self.repo, "cherry-pick", first)
        reordered_first = git(self.repo, "rev-parse", "HEAD")

        pushes.clear()
        with mock.patch.object(forkstack, "run", side_effect=fake_run):
            forkstack.cmd_submit(args)
        git(
            self.repo,
            "fetch",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        )

        first_head = git(
            self.repo, "rev-parse", "origin/fs-head/draft/1"
        )
        second_head = git(
            self.repo, "rev-parse", "origin/fs-head/draft/2"
        )
        first_base = git(
            self.repo, "rev-parse", "origin/fs-base/draft/1"
        )
        second_base = git(
            self.repo, "rev-parse", "origin/fs-base/draft/2"
        )
        self.assertEqual(first_head, reordered_first)
        self.assertEqual(second_head, reordered_second)
        self.assertEqual(first_base, reordered_second)
        self.assertEqual(second_base, self.base)
        self.assertEqual(
            git(self.repo, "rev-parse", "fs-head/draft/1"), reordered_first
        )
        self.assertEqual(
            git(self.repo, "rev-parse", "fs-head/draft/2"), reordered_second
        )
        self.assertEqual(
            git(self.repo, "diff", "--binary", first_base, first_head),
            git(
                self.repo,
                "diff",
                "--binary",
                reordered_second,
                reordered_first,
            ),
        )
        self.assertEqual(
            git(self.repo, "diff", "--binary", second_base, second_head),
            git(self.repo, "diff", "--binary", self.base, reordered_second),
        )
        self.assertEqual(len(pushes), 1)
        self.assertIn("refs/heads/fs-base/draft/1", " ".join(pushes[0]))
        self.assertIn("refs/heads/fs-base/draft/2", " ".join(pushes[0]))
        self.assertIn("refs/heads/fs-head/draft/1", " ".join(pushes[0]))
        self.assertIn("refs/heads/fs-head/draft/2", " ".join(pushes[0]))
        self.assertTrue(all("--atomic" in push for push in pushes))
        self.assertTrue(all("--force-with-lease" in push for push in pushes))


if __name__ == "__main__":
    unittest.main()
