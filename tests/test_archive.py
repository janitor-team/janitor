#!/usr/bin/python
# Copyright (C) 2022 Jelmer Vernooij <jelmer@jelmer.uk>
#
# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 2 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program; if not, write to the Free Software
# Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA


import hashlib
import os
from tempfile import TemporaryDirectory

import gpg
import pytest
from debian.deb822 import Release

from janitor.config import read_string as read_config_string
from janitor.debian.archive import (
    GeneratorManager,
    HashedFileWriter,
    PackageInfoProvider,
    create_app,
    write_suite_files,
)


async def create_client(aiohttp_client, config=None):
    if config is None:
        config = read_config_string("")
    return await aiohttp_client(
        await create_app(None, config, "/tmp", None, gpg_context=None)
    )


async def test_health(aiohttp_client):
    client = await create_client(aiohttp_client)

    resp = await client.get("/health")
    assert resp.status == 200
    text = await resp.text()
    assert text == "ok"


async def test_ready(aiohttp_client):
    client = await create_client(aiohttp_client)

    resp = await client.get("/ready")
    assert resp.status == 200
    text = await resp.text()
    assert text == ""


def test_hash_file_writer():
    with TemporaryDirectory() as td:
        r = Release()
        with HashedFileWriter(r, td, "foo/bar") as w:
            w.write(b"chunk1")
            w.write(b"chunk2")
            w.done()
        md5hex = hashlib.md5(b"chunk1chunk2").hexdigest()
        with open(os.path.join(td, "foo", "by-hash", "MD5Sum", md5hex), "rb") as f:
            assert f.read() == b"chunk1chunk2"
        with open(os.path.join(td, "foo", "bar"), "rb") as f:
            assert f.read() == b"chunk1chunk2"
        assert r["MD5Sum"] == [{"md5sum": md5hex, "name": "foo/bar", "size": 12}]


async def _no_entries(*args, **kwargs):
    return
    yield  # pragma: no cover


async def test_write_suite_files_leaves_no_partial_gpg_files_on_signing_failure():
    """A failed signature must not leave a Release.gpg or InRelease behind.

    apt treats a present-but-unsigned Release.gpg as an attempted-and-failed
    verification (not "no verification requested"), so an empty file there
    is worse than none.
    """
    with TemporaryDirectory() as gnupghome, TemporaryDirectory() as base_path:
        os.chmod(gnupghome, 0o700)
        gpg_context = gpg.Context(armor=True, home_dir=gnupghome)

        with pytest.raises(gpg.errors.GpgError):
            await write_suite_files(
                base_path,
                get_packages=_no_entries,
                get_sources=_no_entries,
                suite_name="test",
                archive_description="Test",
                components=["main"],
                arches=["amd64"],
                origin="test",
                gpg_context=gpg_context,
            )

        assert os.path.exists(os.path.join(base_path, "Release"))
        assert not os.path.exists(os.path.join(base_path, "Release.gpg"))
        assert not os.path.exists(os.path.join(base_path, "InRelease"))


ON_DEMAND_CONFIG = """\
origin: "janitor.example.org"
distribution {
  name: "unstable"
  archive_mirror_uri: "http://deb.debian.org/debian"
  component: "main"
  chroot: "unstable-amd64-sbuild"
  vendor: "debian"
}
campaign {
  name: "some-campaign"
  branch_name: "some-campaign"
  debian_build {
    base_distribution: "unstable"
    build_distribution: "some-campaign"
  }
}
"""


PACKAGES = b"Package: pkg\nVersion: 1.0-1\nArchitecture: amd64\n\n"
SOURCES = b"Package: pkg\nVersion: 1.0-1\n\n"


class OnePackageInfoProvider(PackageInfoProvider):
    async def packages_for_run(self, run_id, suite_name, package, arch):
        yield PACKAGES

    async def sources_for_run(self, run_id, suite_name, package):
        yield SOURCES


async def _add_run(con, run_id):
    await con.execute(
        "INSERT INTO codebase (name, branch_url, url, subpath, vcs_type) "
        "VALUES ('pkg', 'https://example.com/pkg', 'https://example.com/pkg', "
        "'', 'git')"
    )
    await con.execute(
        "INSERT INTO change_set (id, campaign) "
        "VALUES ('some-change-set', 'some-campaign')"
    )
    await con.execute(
        "INSERT INTO run (id, suite, codebase, result_code, finish_time, "
        "logfilenames, change_set) "
        "VALUES ($1, 'some-campaign', 'pkg', 'success', now(), '{}', "
        "'some-change-set')",
        run_id,
    )
    await con.execute(
        "INSERT INTO debian_build (run_id, version, distribution, source) "
        "VALUES ($1, '1.0-1', 'some-campaign', 'pkg')",
        run_id,
    )


async def _on_demand_client(aiohttp_client, db, dists_dir):
    config = read_config_string(ON_DEMAND_CONFIG)
    generator_manager = GeneratorManager(
        dists_dir, db, config, OnePackageInfoProvider(), None
    )
    return await aiohttp_client(
        await create_app(generator_manager, config, dists_dir, db, gpg_context=None)
    )


async def test_on_demand_dists_run(aiohttp_client, db, con, tmp_path):
    """/dists/run/{run_id} serves a Release and the packages built by that run."""
    run_id = "some-run-id"
    await _add_run(con, run_id)
    client = await _on_demand_client(aiohttp_client, db, str(tmp_path))

    resp = await client.get(f"/dists/run/{run_id}/Release")
    assert resp.status == 200, await resp.text()
    body = await resp.read()
    assert (tmp_path / "run" / run_id / "Release").read_bytes() == body
    release = Release(body)
    assert release["Suite"] == f"run/{run_id}"
    assert release["Label"] == f"Run {run_id}"
    assert release["Origin"] == "janitor.example.org"
    assert release["Components"] == "main"

    resp = await client.get(f"/dists/run/{run_id}/main/binary-amd64/Packages")
    assert (resp.status, await resp.read()) == (200, PACKAGES)
    resp = await client.get(f"/dists/run/{run_id}/main/source/Sources")
    assert (resp.status, await resp.read()) == (200, SOURCES)


async def test_on_demand_dists_unknown_run(aiohttp_client, db, tmp_path):
    """An unknown run id is a 404, not a crash further down."""
    client = await _on_demand_client(aiohttp_client, db, str(tmp_path))

    resp = await client.get("/dists/run/no-such-run/Release")
    assert (resp.status, await resp.text()) == (404, "no such run: no-such-run")
