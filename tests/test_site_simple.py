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

import tempfile
from pathlib import Path

import asyncpg
import pytest_asyncio
import testing.postgresql
from aiohttp import web
from fakeredis.aioredis import FakeRedis

from janitor.config import read_string as read_config_string
from janitor.site.simple import create_app

_SCHEMA_DIR = Path(__file__).resolve().parent.parent / "schema"


@pytest_asyncio.fixture()
async def database_location():
    with testing.postgresql.Postgresql() as postgresql:
        conn = await asyncpg.connect(postgresql.url())
        try:
            await conn.execute((_SCHEMA_DIR / "state.sql").read_text())
            await conn.execute((_SCHEMA_DIR / "debian" / "debian.sql").read_text())
        finally:
            await conn.close()

        yield postgresql.url()


def create_config(database_location=None):
    return read_config_string(f"""
campaign {{
  name: "lintian-fixes"
}}
artifact_location: "{tempfile.mkdtemp()}"
{f'database_location: "{database_location}"' if database_location else ""}
""")


def _fingerprints_of(exported):
    """Fingerprints of the keys in an exported gpg keyring blob."""
    import gpg

    if not exported:
        return []
    with tempfile.TemporaryDirectory() as home:
        with gpg.Context(home_dir=home) as ctx:
            result = ctx.key_import(exported)
            return [i.fpr for i in getattr(result, "imports", ())]


async def test_create_app():
    await create_app(config=create_config(), redis=FakeRedis())


async def test_codebase_query_redirect(aiohttp_client, database_location):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get(
        "/lintian-fixes/c", params={"codebase": "foo"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/lintian-fixes/c/foo/"


async def test_codebase_query_redirect_legacy_package_param(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get(
        "/lintian-fixes/c", params={"package": "foo"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/lintian-fixes/c/foo/"


async def test_codebase_query_redirect_prefers_codebase_over_package(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get(
        "/lintian-fixes/c",
        params={"codebase": "new", "package": "old"},
        allow_redirects=False,
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/lintian-fixes/c/new/"


async def test_codebase_query_redirect_missing_param_returns_404(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get("/lintian-fixes/c", allow_redirects=False)
    assert resp.status == 404


async def test_codebase_query_redirect_empty_param_returns_404(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get(
        "/lintian-fixes/c", params={"codebase": ""}, allow_redirects=False
    )
    assert resp.status == 404


async def test_webhook_github_push_reschedules_without_crashing(
    aiohttp_client, database_location
):
    # A GitHub push webhook for a known codebase reschedules it successfully.
    conn = await asyncpg.connect(database_location)
    try:
        await conn.execute(
            "INSERT INTO codebase (name, branch_url, url, vcs_type) "
            "VALUES ($1, $2, $2, $3)",
            "example",
            "https://github.com/jelmer/example",
            "git",
        )
    finally:
        await conn.close()

    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.post(
        "/webhook",
        headers={"X-GitHub-Event": "push"},
        json={
            "ref": "refs/heads/main",
            "after": "0" * 40,
            "repository": {
                "clone_url": "https://github.com/jelmer/example",
                "html_url": "https://github.com/jelmer/example",
                "git_url": "git://github.com/jelmer/example.git",
                "ssh_url": "git@github.com:jelmer/example.git",
                "default_branch": "main",
            },
        },
    )
    assert resp.status == 200, await resp.text()
    body = await resp.json()
    assert "https://github.com/jelmer/example" in body["urls"]


async def test_candidates_with_multiple_unscored_does_not_500(
    aiohttp_client, database_location
):
    # The candidates page loads fine with two or more unscored candidates.
    conn = await asyncpg.connect(database_location)
    try:
        await conn.execute(
            "INSERT INTO codebase (name, branch_url, url, vcs_type) VALUES "
            "($1, $2, $2, $3), ($4, $5, $5, $3)",
            "foo",
            "https://example.com/foo.git",
            "git",
            "bar",
            "https://example.com/bar.git",
        )
        await conn.execute(
            "INSERT INTO candidate (codebase, suite, command) VALUES "
            "($1, $2, $3), ($4, $2, $3)",
            "foo",
            "lintian-fixes",
            "true",
            "bar",
        )
    finally:
        await conn.close()

    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get("/lintian-fixes/candidates")
    assert resp.status == 200


async def test_merge_proposal_without_url_returns_400(
    aiohttp_client, database_location
):
    _private_app, app = await create_app(
        config=create_config(database_location), redis=FakeRedis()
    )
    client = await aiohttp_client(app)
    resp = await client.get("/lintian-fixes/merge-proposal")
    assert resp.status == 400
    assert await resp.text() == "no url specified"


# A real throwaway ed25519 public key, generated for these tests only.
IMPORTABLE_KEY = """-----BEGIN PGP PUBLIC KEY BLOCK-----

mDMEar0oJhYJKwYBBAHaRw8BAQdAWau9EcHXu85QZP6cSXl9uDQKWcUj6Tgc5RmJ
43UAUYu0Lkphbml0b3IgQXJjaGl2ZSBUZXN0IDxhcmNoaXZlQGV4YW1wbGUuaW52
YWxpZD6IkAQTFgoAOBYhBN0jaZX8b8oPr+Lq8hAB/UU1IsftBQJqvSgmAhsBBQsJ
CAcCBhUKCQgLAgQWAgMBAh4BAheAAAoJEBAB/UU1Isft02IBAMrmlhteLroJuhsx
4iq/LqRXcMt5een+7jJ4EJWQPOoGAP4qjF1yjfQ0uXEHv2FK3rzd19WzeyH2TJJM
fQ0QDRZHAg==
=6omt
-----END PGP PUBLIC KEY BLOCK-----
"""

IMPORTABLE_KEY_FPR = "DD236995FC6FCA0FAFE2EAF21001FD453522C7ED"

# A second one, for the case of a janitor part way through a key rotation.
ROTATED_KEY = """-----BEGIN PGP PUBLIC KEY BLOCK-----

mDMEar0skRYJKwYBBAHaRw8BAQdAdjP/XCUejUxYvBfe37nEIrJAvTae82vu6fK6
2VEPyk60MEphbml0b3IgUm90YXRpb24gVGVzdCA8cm90YXRpb25AZXhhbXBsZS5p
bnZhbGlkPoiQBBMWCgA4FiEEPLD9OJtBhxgNbxuyYNypMR4xQyMFAmq9LJECGwEF
CwkIBwIGFQoJCAsCBBYCAwECHgECF4AACgkQYNypMR4xQyOVMwEA/xWVBmH7Ik5Y
+fK14Gbeo/Jxl5LAr59BaRRRW+cJJacBAPK0h/Yr0xM13MbcmIQk16oqLiWl9rHq
L8H3TRy4UukF
=642X
-----END PGP PUBLIC KEY BLOCK-----
"""

ROTATED_KEY_FPR = "3CB0FD389B4187180D6F1BB260DCA9311E314323"

# A key with its user ID stripped, so gpg considers it and imports nothing.
UNIMPORTABLE_KEY = """-----BEGIN PGP PUBLIC KEY BLOCK-----

mDMEar0oJxYJKwYBBAHaRw8BAQdA9qrKXk+/Z/Nhoh3VwHWzxvL7tuan/xkvJj/O
TgsWBY8=
=e2Qc
-----END PGP PUBLIC KEY BLOCK-----
"""


def _publisher_app(pgp_keys):
    """A stand-in publisher serving just the /credentials endpoint."""

    async def handle_credentials(request):
        return web.json_response({"ssh_keys": [], "pgp_keys": pgp_keys, "hosting": []})

    app = web.Application()
    app.router.add_get("/credentials", handle_credentials)
    return app


def _archiver_app(pgp_keys):
    """A stand-in archiver serving just the /pgp_keys endpoint."""

    async def handle_pgp_keys(request):
        return web.json_response(pgp_keys)

    app = web.Application()
    app.router.add_get("/pgp_keys", handle_pgp_keys)
    return app


async def _site(database_location, publisher, archiver=None):
    _private_app, app = await create_app(
        config=create_config(database_location),
        redis=FakeRedis(),
        publisher_url=str(publisher.make_url("/")),
        archiver_url=(
            str(archiver.make_url("/")) if archiver else "http://archiver.invalid/"
        ),
    )
    return app


async def test_pgp_keys_without_publisher_keys_returns_404(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([]))
    app = await _site(database_location, publisher)
    client = await aiohttp_client(app)
    resp = await client.get("/pgp_keys")
    assert resp.status == 404
    assert await resp.text() != ""


async def test_pgp_keys_asc_serves_the_publisher_keys(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([IMPORTABLE_KEY]))
    app = await _site(database_location, publisher)
    client = await aiohttp_client(app)
    resp = await client.get("/pgp_keys.asc")
    assert resp.status == 200
    assert resp.content_type == "application/pgp-keys"
    assert IMPORTABLE_KEY in await resp.text()


async def test_pgp_keys_serves_the_publisher_keys(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([IMPORTABLE_KEY]))
    app = await _site(database_location, publisher)
    client = await aiohttp_client(app)
    resp = await client.get("/pgp_keys")
    assert resp.status == 200
    assert resp.content_type == "application/pgp-keys"
    exported = await resp.read()
    assert exported != b""
    assert _fingerprints_of(exported) == [IMPORTABLE_KEY_FPR]


async def test_pgp_keys_does_not_serve_the_rest_of_the_keyring(
    aiohttp_client, aiohttp_server, database_location
):
    # One gpg home is shared by every keyring route, so seed it via the archive.
    publisher = await aiohttp_server(_publisher_app([UNIMPORTABLE_KEY]))
    archiver = await aiohttp_server(_archiver_app([IMPORTABLE_KEY]))
    app = await _site(database_location, publisher, archiver)
    client = await aiohttp_client(app)

    seeded = await client.get("/archive-keyring.gpg")
    assert seeded.status == 200
    assert _fingerprints_of(await seeded.read()) == [IMPORTABLE_KEY_FPR]

    resp = await client.get("/pgp_keys")
    assert IMPORTABLE_KEY_FPR not in _fingerprints_of(await resp.read())
    assert resp.status == 502


async def test_credentials_without_publisher_keys_still_renders(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([]))
    app = await _site(database_location, publisher)
    client = await aiohttp_client(app)
    resp = await client.get("/credentials")
    assert resp.status == 200


async def test_credentials_does_not_list_the_rest_of_the_keyring(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([UNIMPORTABLE_KEY]))
    archiver = await aiohttp_server(_archiver_app([IMPORTABLE_KEY]))
    app = await _site(database_location, publisher, archiver)
    client = await aiohttp_client(app)

    seeded = await client.get("/archive-keyring.gpg")
    assert seeded.status == 200

    resp = await client.get("/credentials")
    assert IMPORTABLE_KEY_FPR not in await resp.text()
    assert resp.status == 502


async def test_archive_keyring_without_archiver_keys_returns_404(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([IMPORTABLE_KEY]))
    archiver = await aiohttp_server(_archiver_app([]))
    app = await _site(database_location, publisher, archiver)
    client = await aiohttp_client(app)
    resp = await client.get("/archive-keyring.gpg")
    assert resp.status == 404


async def test_archive_keyring_does_not_serve_the_rest_of_the_keyring(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([IMPORTABLE_KEY]))
    archiver = await aiohttp_server(_archiver_app([UNIMPORTABLE_KEY]))
    app = await _site(database_location, publisher, archiver)
    client = await aiohttp_client(app)

    seeded = await client.get("/pgp_keys")
    assert seeded.status == 200
    assert _fingerprints_of(await seeded.read()) == [IMPORTABLE_KEY_FPR]

    resp = await client.get("/archive-keyring.gpg")
    assert IMPORTABLE_KEY_FPR not in _fingerprints_of(await resp.read())
    assert resp.status == 502


async def test_pgp_keys_serves_every_reported_key(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([IMPORTABLE_KEY, ROTATED_KEY]))
    app = await _site(database_location, publisher)
    client = await aiohttp_client(app)
    resp = await client.get("/pgp_keys")
    assert resp.status == 200
    assert sorted(_fingerprints_of(await resp.read())) == sorted(
        [IMPORTABLE_KEY_FPR, ROTATED_KEY_FPR]
    )


async def test_archive_keyring_serves_every_reported_key(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([IMPORTABLE_KEY]))
    archiver = await aiohttp_server(_archiver_app([IMPORTABLE_KEY, ROTATED_KEY]))
    app = await _site(database_location, publisher, archiver)
    client = await aiohttp_client(app)
    resp = await client.get("/archive-keyring.gpg")
    assert resp.status == 200
    assert sorted(_fingerprints_of(await resp.read())) == sorted(
        [IMPORTABLE_KEY_FPR, ROTATED_KEY_FPR]
    )


async def test_credentials_lists_every_reported_key(
    aiohttp_client, aiohttp_server, database_location
):
    publisher = await aiohttp_server(_publisher_app([IMPORTABLE_KEY, ROTATED_KEY]))
    app = await _site(database_location, publisher)
    client = await aiohttp_client(app)
    resp = await client.get("/credentials")
    assert resp.status == 200
    body = await resp.text()
    assert IMPORTABLE_KEY_FPR in body
    assert ROTATED_KEY_FPR in body
