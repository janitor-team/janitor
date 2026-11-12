#!/usr/bin/python3
# Copyright (C) 2020 Jelmer Vernooij <jelmer@jelmer.uk>
#
# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.

"""Artifacts."""

import asyncio

from ._common import artifacts  # type: ignore

ArtifactManager = artifacts.ArtifactManager
GCSArtifactManager = artifacts.GCSArtifactManager
LocalArtifactManager = artifacts.LocalArtifactManager
ServiceUnavailable = artifacts.ServiceUnavailable
ArtifactsMissing = artifacts.ArtifactsMissing
get_artifact_manager = artifacts.get_artifact_manager
list_ids = artifacts.list_ids
upload_backup_artifacts = artifacts.upload_backup_artifacts
store_artifacts_with_backup = artifacts.store_artifacts_with_backup

DEFAULT_GCS_TIMEOUT = 60


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command")
    list_parser = subparsers.add_parser("list")
    list_parser.add_argument("location", type=str)
    args = parser.parse_args()
    if args.command == "list":
        manager = get_artifact_manager(args.location)
        asyncio.run(list_ids(manager))
