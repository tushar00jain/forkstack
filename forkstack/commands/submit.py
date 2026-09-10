"""The ``forkstack submit`` command."""

import argparse
import dataclasses
import json
import os
import re
import subprocess
import sys


def run(cmd, cwd=None, capture=True, input=None, env=None):
    result = subprocess.run(
        cmd,
        cwd=cwd,
        check=True,
        text=True,
        capture_output=capture,
        input=input,
        env=env,
    )
    return (result.stdout or "").strip()


def git(args, repo, **kwargs):
    return run(["git", *args], cwd=repo, **kwargs)


def parse_owner_repo(url):
    m = re.search(r"[:/]([^/:]+)/([^/]+?)(?:\.git)?$", url.strip())
    return f"{m.group(1)}/{m.group(2)}" if m else None


def commit_field(repo, rev, fmt):
    return git(["log", "-1", f"--format={fmt}", rev], repo)


IDENTITY_TRAILER = "fs-branch"
IDENTITY_RE = re.compile(rf"^{IDENTITY_TRAILER}:\s*(\S+)\s*$", re.MULTILINE)


@dataclasses.dataclass
class StackCommit:
    rev: str
    branch: str
    subject: str
    body: str
    message: str
    identity_added: bool = False
    number: int | None = None
    pr: dict | None = None

    @property
    def head_branch(self):
        return f"fs-head/{self.branch}"

    @property
    def base_branch(self):
        return f"fs-base/{self.branch}"


def identity_from_message(message):
    matches = IDENTITY_RE.findall(message)
    if len(matches) > 1:
        raise RuntimeError(f"multiple {IDENTITY_TRAILER} trailers in one commit")
    return matches[0] if matches else None


def add_identity(message, branch):
    if identity_from_message(message):
        return message
    return f"{message.rstrip()}\n\n{IDENTITY_TRAILER}: {branch}\n"


def body_without_identity(body):
    return IDENTITY_RE.sub("", body).rstrip()


def remote_identities(repo, remote, prefix):
    out = git(
        [
            "for-each-ref",
            "--format=%(refname:strip=4)",
            f"refs/remotes/{remote}/fs-head/{prefix}/",
        ],
        repo,
    )
    return set(out.splitlines()) if out else set()


def assign_branches(repo, revs, remote, prefix):
    records = []
    claimed = set()
    previous = None
    for rev in revs:
        parents = commit_field(repo, rev, "%P").split()
        if len(parents) != 1:
            raise RuntimeError(
                f"commit {rev[:12]} has {len(parents)} parents; "
                "forkstack requires a linear stack"
            )
        if previous is not None and parents[0] != previous:
            raise RuntimeError("forkstack requires a linear stack")
        previous = rev
        message = commit_field(repo, rev, "%B")
        branch = identity_from_message(message)
        if branch:
            if branch in claimed:
                raise RuntimeError(f"duplicate {IDENTITY_TRAILER}: {branch}")
            try:
                git(["check-ref-format", "--branch", branch], repo)
            except subprocess.CalledProcessError as error:
                raise RuntimeError(f"invalid {IDENTITY_TRAILER}: {branch}") from error
            claimed.add(branch)
        records.append((rev, message, branch))

    if prefix is None and any(branch is None for _rev, _message, branch in records):
        raise RuntimeError("untagged commits require --prefix")

    known = claimed
    next_number = 1
    if prefix is not None:
        known = remote_identities(repo, remote, prefix) | claimed
        branch_re = re.compile(rf"^{re.escape(prefix)}/([0-9]+)$")
        numbered = [
            int(match.group(1))
            for branch in known
            if (match := branch_re.match(branch))
        ]
        next_number = max(numbered, default=0) + 1

    plan = []
    for position, (rev, message, branch) in enumerate(records, start=1):
        identity_added = branch is None
        if branch is None:
            positional = f"{prefix}/{position}"
            if positional in known and positional not in claimed:
                branch = positional
            else:
                while f"{prefix}/{next_number}" in known:
                    next_number += 1
                branch = f"{prefix}/{next_number}"
                next_number += 1
            claimed.add(branch)
            known.add(branch)
        plan.append(
            StackCommit(
                rev=rev,
                branch=branch,
                subject=commit_field(repo, rev, "%s"),
                body=body_without_identity(commit_field(repo, rev, "%b")),
                message=message,
                identity_added=identity_added,
            )
        )
    return plan


def rewrite_with_identities(repo, plan):
    old_head = git(["rev-parse", "HEAD"], repo)
    new_parent = None
    rewritten = {}
    for step in plan:
        old_rev = step.rev
        old_parent = git(["rev-parse", f"{old_rev}^"], repo)
        parent = new_parent or old_parent
        message = add_identity(step.message, step.branch)
        if parent == old_parent and message == step.message:
            new_rev = old_rev
        else:
            fields = commit_field(
                repo,
                old_rev,
                "%T%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI",
            ).split("\0")
            (
                tree,
                author_name,
                author_email,
                author_date,
                committer_name,
                committer_email,
                committer_date,
            ) = fields
            env = {
                **os.environ,
                "GIT_AUTHOR_NAME": author_name,
                "GIT_AUTHOR_EMAIL": author_email,
                "GIT_AUTHOR_DATE": author_date,
                "GIT_COMMITTER_NAME": committer_name,
                "GIT_COMMITTER_EMAIL": committer_email,
                "GIT_COMMITTER_DATE": committer_date,
            }
            new_rev = git(
                ["commit-tree", tree, "-p", parent],
                repo,
                input=f"{message.rstrip()}\n",
                env=env,
            )
        step.rev = new_rev
        step.message = message
        step.body = body_without_identity("\n".join(message.splitlines()[1:]))
        new_parent = new_rev
        rewritten[old_rev] = new_rev

    # Preserve local branch markers inside the stack, not just the checked-out
    # top branch.
    for old_rev, new_rev in rewritten.items():
        if old_rev == new_rev:
            continue
        refs = git(
            [
                "for-each-ref",
                "--format=%(refname)",
                "--points-at",
                old_rev,
                "refs/heads/",
            ],
            repo,
        ).splitlines()
        for ref in refs:
            git(["update-ref", ref, new_rev, old_rev], repo)

    if git(["rev-parse", "HEAD"], repo) == old_head and new_parent != old_head:
        git(["update-ref", "HEAD", new_parent, old_head], repo)
    return plan


def optional_ref(repo, ref):
    try:
        return git(["rev-parse", "--verify", ref], repo)
    except subprocess.CalledProcessError:
        return None


def checked_out_branches(repo):
    branches = set()
    for line in git(["worktree", "list", "--porcelain"], repo).splitlines():
        if line.startswith("branch "):
            branches.add(line.removeprefix("branch "))
    return branches


def sync_local_head_branches(repo, plan, remote):
    """Keep local fs-head branches aligned with their logical stack changes."""
    checked_out = checked_out_branches(repo)
    updates = []

    for step in plan:
        local_ref = f"refs/heads/{step.head_branch}"
        remote_ref = f"refs/remotes/{remote}/{step.head_branch}"
        local_rev = optional_ref(repo, local_ref)

        if local_rev == step.rev:
            continue
        if local_ref in checked_out:
            raise RuntimeError(
                f"cannot update {step.head_branch!r}: it is checked out in a worktree"
            )
        if local_rev is None:
            updates.append(f"create {local_ref} {step.rev}")
            continue

        remote_rev = optional_ref(repo, remote_ref)
        if remote_rev != local_rev:
            raise RuntimeError(
                f"refusing to overwrite divergent local branch {step.head_branch!r}; "
                f"it does not match {remote}/{step.head_branch}"
            )
        updates.append(f"update {local_ref} {step.rev} {local_rev}")

    if updates:
        git(["update-ref", "--stdin"], repo, input="\n".join(updates) + "\n")

    for step in plan:
        git(["config", f"branch.{step.head_branch}.remote", remote], repo)
        git(
            [
                "config",
                f"branch.{step.head_branch}.merge",
                f"refs/heads/{step.head_branch}",
            ],
            repo,
        )


def push_refs(repo, remote, updates, label):
    if not updates:
        return
    print(f"pushing {label}")
    run(
        [
            "git",
            "push",
            "--atomic",
            "--force-with-lease",
            remote,
            *[f"{rev}:refs/heads/{branch}" for branch, rev in updates.items()],
        ],
        cwd=repo,
        capture=False,
    )


def stack_table(entries, current):
    lines = ["", "---", "Stack (top to bottom):"]
    for branch, number, subject in reversed(entries):
        mark = "->" if branch == current else ""
        ref = f"#{number}" if number else branch
        lines.append(f"- {mark} {ref} {subject}")
    lines += ["---", ""]
    return "\n".join(lines)


def existing_pr(fork, branch, repo):
    out = run(
        [
            "gh",
            "pr",
            "list",
            "--repo",
            fork,
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "number,baseRefName,title,body",
        ],
        cwd=repo,
    )
    prs = json.loads(out or "[]")
    return prs[0] if prs else None


def cmd_submit(args):
    repo = args.repo
    fork = parse_owner_repo(git(["remote", "get-url", args.remote], repo))
    if not fork:
        sys.exit(f"could not read owner/name from the {args.remote!r} remote URL")

    if args.execute:
        print(f"fetching {args.remote}")
        run(["git", "fetch", args.remote], cwd=repo, capture=False)

    base_ref = f"{args.remote}/{args.base}"
    revs = git(["rev-list", "--reverse", f"{base_ref}..HEAD"], repo).split()
    if not revs:
        sys.exit(f"no commits in {base_ref}..HEAD")

    plan = assign_branches(repo, revs, args.remote, args.prefix)

    print(f"fork:   {fork}")
    print(f"stack:  {base_ref}..HEAD  ({len(plan)} commits)\n")
    for step in plan:
        marker = "  (new identity)" if step.identity_added else ""
        print(
            f"  {step.rev[:12]}  {step.branch} ({step.head_branch}) "
            f"<- base {step.base_branch}{marker}"
        )
        print(f"                {step.subject}")

    if not args.execute:
        print("\nDry run. The execute pass would:\n")
        for step in plan:
            if step.identity_added:
                print(
                    f"  add {IDENTITY_TRAILER}: {step.branch} to "
                    f"{step.rev[:12]}"
                )
            print(
                f"  create/update local {step.head_branch} tracking "
                f"{args.remote}/{step.head_branch}"
            )
            print(f"  atomically update {step.base_branch} and {step.head_branch}")
        print("\nRe-run with --execute to do it.")
        return

    if any(step.identity_added for step in plan):
        print(f"recording stable {IDENTITY_TRAILER} trailers")
        rewrite_with_identities(repo, plan)

    print("updating local PR head branches")
    sync_local_head_branches(repo, plan, args.remote)

    for step in plan:
        step.pr = existing_pr(fork, step.head_branch, repo)
        if step.pr:
            step.number = step.pr["number"]
            if step.pr["baseRefName"] != step.base_branch:
                raise RuntimeError(
                    f"PR #{step.number} targets {step.pr['baseRefName']!r}; "
                    f"expected {step.base_branch!r}"
                )

    updates = {}
    root = git(["rev-parse", base_ref], repo)
    for index, step in enumerate(plan):
        updates[step.base_branch] = root if index == 0 else plan[index - 1].rev
        updates[step.head_branch] = step.rev

    push_refs(repo, args.remote, updates, "PR base and head refs")

    for step in plan:
        if step.number:
            print(f"{step.branch}: reusing #{step.number}")
        else:
            cmd = [
                "gh",
                "pr",
                "create",
                "--repo",
                fork,
                "--base",
                step.base_branch,
                "--head",
                step.head_branch,
                "--title",
                step.subject,
                "--body",
                step.body,
            ]
            if args.draft:
                cmd.append("--draft")
            url = run(cmd, cwd=repo)
            print(f"{step.branch}: created {url}")
            match = re.search(r"/pull/([0-9]+)/?$", url)
            if not match:
                raise RuntimeError(
                    f"could not read pull request number from {url!r}"
                )
            step.number = int(match.group(1))

    entries = [(s.branch, s.number, s.subject) for s in plan]
    for step in plan:
        if step.number:
            run(
                [
                    "gh",
                    "pr",
                    "edit",
                    str(step.number),
                    "--repo",
                    fork,
                    "--title",
                    step.subject,
                    "--body",
                    stack_table(entries, step.branch) + step.body,
                ],
                cwd=repo,
            )
    print(f"\n{len(plan)} pull requests in {fork}.")


def add_parser(subparsers):
    parser = subparsers.add_parser(
        "submit",
        help="push branches and open the pull requests",
        description="Create a stack of one-commit pull requests inside your own fork.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "New changes are assigned <prefix>/<n> identities. Those identities stay\n"
            "with the changes when the stack is reordered. If the stack gets\n"
            "shorter, old branches and PRs are left behind; close them yourself.\n"
        ),
    )
    parser.add_argument(
        "--repo", default=".", help="repository directory (default: cwd)"
    )
    parser.add_argument("--remote", default="origin", help="remote for your fork")
    parser.add_argument(
        "--base",
        default="main",
        help="branch in the fork the stack sits on (default: main)",
    )
    parser.add_argument(
        "--prefix",
        help="identity prefix, required only when the stack has untagged commits",
    )
    parser.add_argument("--draft", action="store_true", default=True)
    parser.add_argument(
        "--no-draft",
        dest="draft",
        action="store_false",
        help="open PRs ready for review",
    )
    parser.add_argument(
        "--execute",
        action="store_true",
        help="actually push branches and create PRs (default: print the plan)",
    )
    parser.set_defaults(func=cmd_submit)
