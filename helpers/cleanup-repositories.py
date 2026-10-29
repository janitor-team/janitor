#!/usr/bin/python3
"""Cleanup owned repositories that are no longer needed for merge proposals.

This is necessary in particular because some hosting sites
(e.g. default GitLab) have restrictions on the number of repositories
that a single user can own (in the case of GitLab, 1000).
"""

import logging
import sys

import breezy
import breezy.bzr
import breezy.git  # noqa: F401
import breezy.plugins
import breezy.plugins.github  # noqa: F401
import breezy.plugins.gitlab  # noqa: F401
import breezy.plugins.launchpad  # noqa: F401
from breezy.forge import ForgeLoginRequired, UnsupportedForge, iter_forge_instances


def projects_to_remove(instance):
    in_use = set()
    for mp in instance.iter_my_proposals():
        if not mp.is_closed() and not mp.is_merged():
            in_use.add(mp.get_source_project())
    for project in instance.iter_my_forks():
        if project in in_use:
            continue
        yield project


def main(argv=None):
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("--dry-run", action="store_true", help="Dry run.")
    args = parser.parse_args()

    logging.basicConfig(format="%(message)s")

    for instance in iter_forge_instances():
        try:
            for project in projects_to_remove(instance):
                logging.info(f"Deleting {project} from {instance!r}")
                if not args.dry_run:
                    instance.delete_project(project)
        except UnsupportedForge as e:
            logging.warning("Ignoring unsupported instance %s: %s", instance, e)
        except ForgeLoginRequired as e:
            logging.warning("Ignoring instance %s: %s", instance, e)


if __name__ == "__main__":
    sys.exit(main())
