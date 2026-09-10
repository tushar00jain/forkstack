"""Forkstack command-line entry point."""

import argparse

from forkstack.commands import log, submit


def build_parser():
    parser = argparse.ArgumentParser(
        description="Create a stack of one-commit pull requests inside your own fork.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    subparsers = parser.add_subparsers(dest="command")
    submit.add_parser(subparsers)
    log.add_parser(subparsers)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    if not hasattr(args, "func"):
        parser.print_help()
        return
    args.func(args)
