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

import asyncio
import base64
import json
import tempfile
from datetime import timedelta

import aiohttp
import asyncpg
import pytest
import pytest_asyncio
from aiohttp import web
from bs4 import BeautifulSoup
from fakeredis.aioredis import FakeRedis

from janitor import utcnow
from janitor.config import read_string as read_config_string
from janitor.site.simple import create_app, main_async

from .conftest import _insert_run


@pytest_asyncio.fixture()
async def database_location(postgresql_factory):
    with postgresql_factory() as postgresql:
        yield postgresql.url()


def create_config(database_location=None, extra=""):
    return read_config_string(f"""
campaign {{
  name: "lintian-fixes"
}}
artifact_location: "{tempfile.mkdtemp()}"
{f'database_location: "{database_location}"' if database_location else ""}
{extra}
""")


@pytest.mark.parametrize("setting", ["campaign", "artifact_location", "redis_location"])
async def test_main_without_setting(tmp_path, setting):
    lines = {
        "campaign": 'campaign { name: "lintian-fixes" }',
        "artifact_location": f'artifact_location: "{tmp_path}"',
        "redis_location": 'redis_location: "redis://localhost"',
    }
    del lines[setting]
    path = tmp_path / "janitor.conf"
    path.write_text("\n".join(lines.values()))
    with pytest.raises(SystemExit) as e:
        await main_async(["--config", str(path)])
    assert e.value.code == f"{path} has no {setting}"


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


async def test_ws_notifications_forwards_redis_channels(
    aiohttp_client, database_location
):
    redis = FakeRedis()
    _private_app, app = await create_app(
        config=create_config(database_location), redis=redis
    )
    client = await aiohttp_client(app)
    async with client.ws_connect(
        "/ws/notifications", timeout=aiohttp.ClientWSTimeout(ws_close=0.1)
    ) as ws:
        for _ in range(100):
            if app["topic_notifications"].subscriptions:
                break
            await asyncio.sleep(0.01)
        link = "http://192.168.1.5:8080/"
        queue = {"processing": [{"worker_link": link, "backchannel": {"my_url": link}}]}
        hidden = {
            "processing": [{"worker_link": None, "backchannel": {"my_url": None}}]
        }
        for channel in ["publish", "merge-proposal", "queue", "result"]:
            expected = hidden if channel == "queue" else queue
            payload = json.dumps(queue)
            for _ in range(100):
                if await redis.publish(channel, payload):
                    break
                await asyncio.sleep(0.01)
            assert await ws.receive_json(timeout=5) == [channel, expected]


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


@pytest.mark.parametrize("encoding", ["json", "form"])
async def test_webhook_github_push_reschedules_without_crashing(
    aiohttp_client, database_location, encoding
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
    payload = {
        "ref": "refs/heads/main",
        "after": "0" * 40,
        "repository": {
            "clone_url": "https://github.com/jelmer/example",
            "html_url": "https://github.com/jelmer/example",
            "git_url": "git://github.com/jelmer/example.git",
            "ssh_url": "git@github.com:jelmer/example.git",
            "default_branch": "main",
        },
    }
    if encoding == "json":
        body = {"json": payload}
    else:
        body = {"data": {"payload": json.dumps(payload)}}
    resp = await client.post("/webhook", headers={"X-GitHub-Event": "push"}, **body)
    assert (resp.status, await resp.json()) == (
        200,
        {"rescheduled": {}, "urls": ["https://github.com/jelmer/example"]},
    )


async def test_webhook_push_schedules_via_runner(aiohttp_client, database_location):
    conn = await asyncpg.connect(database_location)
    try:
        await conn.execute(
            "INSERT INTO codebase (name, branch_url, url, vcs_type) "
            "VALUES ($1, $2, $2, $3)",
            "example",
            "https://github.com/jelmer/example",
            "git",
        )
        await conn.execute(
            "INSERT INTO named_publish_policy (name, per_branch_policy) VALUES "
            "('mypolicy', ARRAY[ROW('main', 'propose', NULL)"
            "::branch_publish_policy])"
        )
        await conn.execute(
            "INSERT INTO candidate (suite, command, codebase, publish_policy) "
            "VALUES ('lintian-fixes', 'true', 'example', 'mypolicy')"
        )
        now = utcnow()
        await _insert_run(
            conn,
            run_id="run-1",
            codebase="example",
            campaign="lintian-fixes",
            start_time=now - timedelta(minutes=30),
            finish_time=now,
        )
        await conn.execute(
            "INSERT INTO new_result_branch (run_id, role, remote_name, "
            "base_revision, revision) VALUES ('run-1', 'main', 'main', 'base', "
            "'revid')"
        )
    finally:
        await conn.close()

    scheduled = []

    async def handle_schedule(request):
        scheduled.append(await request.json())
        return web.json_response({})

    runner_app = web.Application()
    runner_app.router.add_post("/schedule", handle_schedule)
    runner_client = await aiohttp_client(runner_app)

    _private_app, app = await create_app(
        config=create_config(database_location),
        redis=FakeRedis(),
        runner_url=str(runner_client.make_url("/")),
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
    assert (resp.status, await resp.json()) == (
        200,
        {
            "rescheduled": {"example": ["lintian-fixes"]},
            "urls": ["https://github.com/jelmer/example"],
        },
    )
    assert scheduled == [
        {
            "campaign": "lintian-fixes",
            "codebase": "example",
            "requester": "Push hook for https://github.com/jelmer/example",
            "bucket": "hook",
        }
    ]


async def test_webhook_gitlab_push_matches_codebase(aiohttp_client, database_location):
    conn = await asyncpg.connect(database_location)
    try:
        await conn.execute(
            "INSERT INTO codebase (name, branch_url, url, vcs_type) "
            "VALUES ($1, $2, $2, $3)",
            "example",
            "https://gitlab.com/jelmer/example.git",
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
        headers={"X-Gitlab-Event": "Push Hook"},
        json={
            "ref": "refs/heads/main",
            "after": "0" * 40,
            "project": {
                "git_http_url": "https://gitlab.com/jelmer/example.git",
                "git_ssh_url": "git@gitlab.com:jelmer/example.git",
                "default_branch": "main",
            },
        },
    )
    assert (resp.status, await resp.json()) == (
        200,
        {"rescheduled": {}, "urls": ["https://gitlab.com/jelmer/example.git"]},
    )


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


async def _start_provider(aiohttp_server, *, discovery=None, userinfo=None):
    """A local server standing in for the OpenID provider."""

    async def handle_discovery(request):
        if discovery is not None:
            return web.json_response(discovery)
        base = request.url.origin()
        return web.json_response(
            {
                "authorization_endpoint": str(base / "authorize"),
                "token_endpoint": str(base / "token"),
                "userinfo_endpoint": str(base / "userinfo"),
            }
        )

    async def handle_token(request):
        form = await request.post()
        assert (form["grant_type"], form["code"]) == ("authorization_code", "authcode")
        return web.json_response(
            {
                "token_type": "Bearer",
                "access_token": "the-access-token",
            }
        )

    async def handle_userinfo(request):
        assert request.headers["Authorization"] == "Bearer the-access-token"
        return web.json_response(userinfo)

    provider_app = web.Application()
    provider_app.router.add_get("/.well-known/openid-configuration", handle_discovery)
    provider_app.router.add_post("/token", handle_token)
    provider_app.router.add_get("/userinfo", handle_userinfo)
    return await aiohttp_server(provider_app)


async def _openid_site(aiohttp_client, database_location, provider=None, groups=""):
    base_url = f'base_url: "{provider.make_url("/")}"' if provider else ""
    oauth2_provider = f"""oauth2_provider {{
  client_id: "test-client"
  client_secret: "test-secret"
  {base_url}
  {groups}
}}"""
    _private_app, app = await create_app(
        config=create_config(database_location, oauth2_provider),
        external_url="https://example.com/",
        redis=FakeRedis(),
    )
    return await aiohttp_client(app)


def _login_links(html):
    soup = BeautifulSoup(html, "html.parser")
    return [a.get_text() for a in soup.select('a[href^="/login?"]')]


async def test_oauth_callback_without_state(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(aiohttp_server)
    client = await _openid_site(aiohttp_client, database_location, provider)
    resp = await client.get("/oauth/callback", params={"code": "some-code"})
    assert (resp.status, await resp.text()) == (400, "state variable mismatch")


@pytest.mark.parametrize(
    "discovery, links",
    [(None, ["Login"]), ({}, [])],
)
async def test_login_link_with_provider(
    aiohttp_client, aiohttp_server, database_location, discovery, links
):
    provider = await _start_provider(aiohttp_server, discovery=discovery)
    client = await _openid_site(aiohttp_client, database_location, provider)
    resp = await client.get("/lintian-fixes/candidates")
    assert (resp.status, _login_links(await resp.text())) == (200, links)


async def test_login_link_hidden_without_provider(aiohttp_client, database_location):
    # app["openid_config"] is always set, to None when login is disabled.
    client = await _openid_site(aiohttp_client, database_location)
    resp = await client.get("/lintian-fixes/candidates")
    assert (resp.status, _login_links(await resp.text())) == (200, [])


async def _callback(client, **cookies):
    return await client.get(
        "/oauth/callback",
        params={"code": "authcode", "state": "mystate"},
        cookies={"state": "mystate", **cookies},
        allow_redirects=False,
    )


async def _add_session(client, session_id, userinfo):
    async with client.app.database.acquire() as conn:
        await conn.execute(
            "INSERT INTO site_session (id, userinfo) VALUES ($1, $2)",
            session_id,
            userinfo,
        )


async def _get_session(client, session_id):
    async with client.app.database.acquire() as conn:
        return await conn.fetchval(
            "SELECT userinfo FROM site_session WHERE id = $1", session_id
        )


@pytest.mark.parametrize(
    "userinfo, groups",
    [
        ({"name": "Alice", "email": "alice@example.com"}, []),
        ({"name": "Alice", "email": "alice@example.com", "groups": None}, []),
        ({"name": "Alice", "email": "alice@example.com", "groups": ["x"]}, ["x"]),
    ],
)
async def test_oauth_callback_defaults_missing_groups_claim(
    aiohttp_client, aiohttp_server, database_location, userinfo, groups
):
    provider = await _start_provider(aiohttp_server, userinfo=userinfo)
    client = await _openid_site(
        aiohttp_client, database_location, provider, groups='admin_group: "admins"'
    )
    resp = await _callback(client)
    session_id = resp.cookies["session_id"].value
    page = await client.get(
        "/lintian-fixes/candidates", cookies={"session_id": session_id}
    )
    stored = await _get_session(client, session_id)
    assert (resp.status, page.status, stored["groups"]) == (302, 200, groups)


async def test_result_file_log_from_runner(aiohttp_client, database_location):
    async def handle_log(request):
        key = (request.match_info["run_id"], request.match_info["filename"])
        responses = {
            ("run-id", "build.log"): web.Response(text="log"),
            ("run-id", "worker.log"): web.Response(
                status=500, text="storage unavailable"
            ),
        }
        return responses.get(key, web.Response(status=404))

    runner_app = web.Application()
    runner_app.router.add_get("/log/{run_id}/{filename}", handle_log)
    runner_client = await aiohttp_client(runner_app)
    _private_app, app = await create_app(
        config=create_config(database_location),
        redis=FakeRedis(),
        runner_url=str(runner_client.make_url("/")),
    )
    client = await aiohttp_client(app)

    got = []
    for name in ["build.log", "worker.log", "dist.log"]:
        resp = await client.get(f"/lintian-fixes/pkg/pkg/run-id/{name}")
        got.append((name, resp.status, await resp.text()))
    assert got == [
        ("build.log", 200, "log"),
        ("worker.log", 502, "Unable to read log file worker.log for run run-id"),
        ("dist.log", 404, "No log file dist.log for run run-id"),
    ]


async def test_result_file_artifact_from_runner(aiohttp_client, database_location):
    async def handle_artifact(request):
        key = (request.match_info["run_id"], request.match_info["filename"])
        responses = {
            ("run-id", "foo.deb"): web.Response(
                body=b"deb contents", content_type="application/octet-stream"
            ),
            ("run-id", "public.deb"): web.Response(
                status=307,
                headers={"Location": "https://storage.example.com/run-id/public.deb"},
            ),
            ("run-id", "broken.deb"): web.Response(status=500, text="storage failed"),
        }
        return responses.get(key, web.Response(status=404))

    runner_app = web.Application()
    runner_app.router.add_get("/runs/{run_id}/artifacts/{filename}", handle_artifact)
    runner_client = await aiohttp_client(runner_app)
    _private_app, app = await create_app(
        config=create_config(database_location),
        redis=FakeRedis(),
        runner_url=str(runner_client.make_url("/")),
    )
    client = await aiohttp_client(app)

    got = []
    for name in ["foo.deb", "public.deb", "broken.deb", "missing.deb"]:
        resp = await client.get(
            f"/lintian-fixes/pkg/pkg/run-id/{name}", allow_redirects=False
        )
        got.append((name, resp.status, resp.headers.get("Location"), await resp.read()))
    assert got == [
        ("foo.deb", 200, None, b"deb contents"),
        (
            "public.deb",
            307,
            "https://storage.example.com/run-id/public.deb",
            b"307: Temporary Redirect",
        ),
        (
            "broken.deb",
            502,
            None,
            b"Unable to retrieve artifact broken.deb for run run-id",
        ),
        ("missing.deb", 404, None, b"No artifact missing.deb for run run-id"),
    ]


async def test_oauth_callback_clears_state_cookie_with_matching_path(
    aiohttp_client, aiohttp_server, database_location
):
    provider = await _start_provider(
        aiohttp_server, userinfo={"email": "alice@example.com", "groups": []}
    )
    client = await _openid_site(aiohttp_client, database_location, provider)
    resp = await _callback(client)
    state = resp.cookies["state"]
    assert (resp.status, state.value, state["path"], state["max-age"]) == (
        302,
        "",
        "/oauth/callback",
        "0",
    )


ALICE = {"name": "Alice", "email": "alice@example.com", "groups": []}


async def test_logout_deletes_session_and_redirects_home(
    aiohttp_client, database_location
):
    client = await _openid_site(aiohttp_client, database_location)
    await _add_session(client, "mysession", ALICE)
    resp = await client.post(
        "/logout", cookies={"session_id": "mysession"}, allow_redirects=False
    )
    assert resp.status == 302
    assert resp.headers["Location"] == "/"
    assert resp.cookies["session_id"]["max-age"] == "0"
    assert await _get_session(client, "mysession") is None


async def test_logout_get_keeps_session(aiohttp_client, database_location):
    # A plain GET, as from a link or image on another site, must not log out.
    client = await _openid_site(aiohttp_client, database_location)
    await _add_session(client, "mysession", ALICE)
    resp = await client.get(
        "/logout", cookies={"session_id": "mysession"}, allow_redirects=False
    )
    assert resp.status == 405
    assert await _get_session(client, "mysession") == ALICE


async def test_logout_without_session_cookie_still_redirects(
    aiohttp_client, database_location
):
    client = await _openid_site(aiohttp_client, database_location)
    resp = await client.post("/logout", allow_redirects=False)
    assert resp.status == 302
    assert resp.headers["Location"] == "/"


def _fingerprints_of(exported):
    """Fingerprints of the keys in an exported gpg keyring blob."""
    import gpg

    with tempfile.TemporaryDirectory() as home, gpg.Context(home_dir=home) as ctx:
        return [i.fpr for i in ctx.key_import(exported).imports]


def _listed_fingerprints(html):
    """Fingerprints of the "pub" lines on the credentials page."""
    soup = BeautifulSoup(html, "html.parser")
    texts = [pre.get_text() for pre in soup.find_all("pre")]
    return [t.split("]", 1)[1].split()[0] for t in texts if t.startswith("pub ")]


async def _keys_client(
    aiohttp_client,
    aiohttp_server,
    database_location,
    publisher_keys=None,
    archiver_keys=None,
):
    """A site client whose publisher and archiver report the given keys."""

    async def handle_credentials(request):
        return web.json_response(
            {"ssh_keys": [], "pgp_keys": publisher_keys, "hosting": []}
        )

    async def handle_pgp_keys(request):
        return web.json_response(archiver_keys)

    keys_app = web.Application()
    keys_app.router.add_get("/credentials", handle_credentials)
    keys_app.router.add_get("/pgp_keys", handle_pgp_keys)
    url = str((await aiohttp_server(keys_app)).make_url("/"))
    _private_app, app = await create_app(
        config=create_config(database_location),
        redis=FakeRedis(),
        publisher_url=None if publisher_keys is None else url,
        archiver_url=None if archiver_keys is None else url,
    )
    return await aiohttp_client(app)


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
# The packets inside the armor, which a minimal export of the key gives back.
IMPORTABLE_KEY_PACKETS = base64.b64decode("".join(IMPORTABLE_KEY.splitlines()[2:-2]))
IMPORTABLE_KEY_FPR = "DD236995FC6FCA0FAFE2EAF21001FD453522C7ED"


@pytest.mark.parametrize(
    "publisher, archiver, path",
    [(True, False, "/pgp_keys"), (False, True, "/archive-keyring.gpg")],
)
async def test_keyring_route_with_only_one_url(
    aiohttp_client, aiohttp_server, database_location, publisher, archiver, path
):
    keys = [IMPORTABLE_KEY]
    client = await _keys_client(
        aiohttp_client,
        aiohttp_server,
        database_location,
        keys if publisher else None,
        keys if archiver else None,
    )
    resp = await client.get(path)
    assert (resp.status, await resp.read()) == (200, IMPORTABLE_KEY_PACKETS)


# A key with its user ID stripped, so gpg considers it and imports nothing.
UNIMPORTABLE_KEY = """-----BEGIN PGP PUBLIC KEY BLOCK-----

mDMEar0oJxYJKwYBBAHaRw8BAQdA9qrKXk+/Z/Nhoh3VwHWzxvL7tuan/xkvJj/O
TgsWBY8=
=e2Qc
-----END PGP PUBLIC KEY BLOCK-----
"""

# A second one, for a janitor part way through a key rotation.
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


async def test_keyring_routes_without_keys_return_404(
    aiohttp_client, aiohttp_server, database_location
):
    client = await _keys_client(
        aiohttp_client, aiohttp_server, database_location, [], []
    )
    resp = await client.get("/pgp_keys")
    assert (resp.status, await resp.text()) == (404, "The publisher has no PGP keys.")
    resp = await client.get("/archive-keyring.gpg")
    assert (resp.status, await resp.text()) == (404, "The archiver has no PGP keys.")


async def test_pgp_keys_does_not_serve_other_keys_of_the_keyring(
    aiohttp_client, aiohttp_server, database_location
):
    # The keyring routes share one gpg home, so the archive key stays in it.
    client = await _keys_client(
        aiohttp_client,
        aiohttp_server,
        database_location,
        [UNIMPORTABLE_KEY],
        [IMPORTABLE_KEY],
    )
    seeded = await client.get("/archive-keyring.gpg")
    assert _fingerprints_of(await seeded.read()) == [IMPORTABLE_KEY_FPR]
    resp = await client.get("/pgp_keys")
    assert (resp.status, await resp.read()) == (
        502,
        b"None of the reported PGP keys could be imported.",
    )


@pytest.mark.parametrize(
    "publisher_keys, listed",
    [
        ([], []),
        ([IMPORTABLE_KEY, ROTATED_KEY], [IMPORTABLE_KEY_FPR, ROTATED_KEY_FPR]),
    ],
)
async def test_credentials_lists_only_the_reported_keys(
    aiohttp_client, aiohttp_server, database_location, publisher_keys, listed
):
    client = await _keys_client(
        aiohttp_client,
        aiohttp_server,
        database_location,
        publisher_keys,
        [IMPORTABLE_KEY],
    )
    seeded = await client.get("/archive-keyring.gpg")
    assert _fingerprints_of(await seeded.read()) == [IMPORTABLE_KEY_FPR]
    resp = await client.get("/credentials")
    assert (resp.status, _listed_fingerprints(await resp.text())) == (200, listed)


async def test_pgp_keys_serves_every_reported_key(
    aiohttp_client, aiohttp_server, database_location
):
    client = await _keys_client(
        aiohttp_client, aiohttp_server, database_location, [IMPORTABLE_KEY, ROTATED_KEY]
    )
    resp = await client.get("/pgp_keys")
    assert (resp.status, _fingerprints_of(await resp.read())) == (
        200,
        [IMPORTABLE_KEY_FPR, ROTATED_KEY_FPR],
    )
