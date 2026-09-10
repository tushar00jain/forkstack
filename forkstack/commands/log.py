"""The ``forkstack log`` command."""

import os
import signal
import subprocess
import sys
import tempfile


REFRESH_KEY = "R"
REFRESH_STATUS = ord(REFRESH_KEY)
LESSKEY = f"#command\n{REFRESH_KEY} quit {REFRESH_KEY}\n"

# -S keeps graph columns aligned, -+F keeps the pager open for refresh, and
# -X leaves the graph visible when the pager exits.
LESS = [
    "less",
    "-R",
    "-S",
    "-+F",
    "-X",
    f"-Ps?e(END)  .[{REFRESH_KEY}] refresh   [q] quit",
]
CLEAR = "\033[H\033[2J"


def page(cmd, repo, env):
    """Run cmd under less. True if the user asked for another walk."""
    walk = subprocess.Popen(
        cmd, cwd=repo, stdout=subprocess.PIPE, start_new_session=True
    )
    previous = signal.signal(signal.SIGINT, signal.SIG_IGN)
    try:
        status = subprocess.run(LESS, stdin=walk.stdout, env=env).returncode
    finally:
        signal.signal(signal.SIGINT, previous)
        walk.stdout.close()
        walk.wait()
    return status == REFRESH_STATUS


def cmd_log(args):
    cmd = ["git", "log", "--graph", "--oneline", "--decorate"]
    remotes = [r for spec in args.remote or [] for r in spec.split(",") if r]
    if args.no_remotes:
        remotes = []
    elif not remotes:
        remotes = ["origin"]

    refs = ["refs/heads/*", "HEAD"]
    refs += [f"refs/remotes/{r}/*" for r in remotes]
    cmd += ["--branches", "HEAD"]
    cmd += [f"--glob=refs/remotes/{r}/*" for r in remotes]
    if args.tags:
        refs.append("refs/tags/*")
        cmd.append("--tags")
    cmd += [f"--decorate-refs={ref}" for ref in refs]
    if args.max_count:
        cmd.append(f"-{args.max_count}")
    cmd += args.git_args
    if not sys.stdout.isatty():
        subprocess.run(cmd, cwd=args.repo, check=False)
        return

    cmd.insert(2, "--color=always")
    with tempfile.NamedTemporaryFile("w", suffix=".lesskey") as keys:
        keys.write(LESSKEY)
        keys.flush()
        env = {**os.environ, "LESSKEYIN": keys.name}
        try:
            while page(cmd, args.repo, env):
                print(CLEAR, end="", flush=True)
        except KeyboardInterrupt:
            pass


def add_parser(subparsers):
    parser = subparsers.add_parser(
        "log",
        help="git log --graph of the stacks and branches",
        description="Graph of the repository's commits and where the branches point. "
        "In the pager, R re-runs the walk and q quits.",
    )
    parser.add_argument(
        "--repo", default=".", help="repository directory (default: cwd)"
    )
    parser.add_argument(
        "--remote",
        action="append",
        metavar="NAME",
        help="show only this remote's refs (repeatable, or comma-separated, "
        "default: origin); local branches and HEAD are always shown",
    )
    parser.add_argument(
        "--no-remotes", action="store_true", help="show no remote refs at all"
    )
    parser.add_argument(
        "--tags",
        action="store_true",
        help="also walk and decorate tags (hidden by default: a repository that "
        "tags for CI buries the stack otherwise)",
    )
    parser.add_argument(
        "-n", "--max-count", type=int, metavar="N", help="limit to N commits"
    )
    parser.add_argument(
        "git_args", nargs="*", metavar="GIT_ARG", help="extra arguments for git log"
    )
    parser.set_defaults(func=cmd_log)
