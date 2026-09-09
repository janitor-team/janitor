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
import os
from datetime import datetime, timedelta
from io import BytesIO

import aiozipkin
import pytest
from aiohttp import MultipartWriter, web
from fakeredis.aioredis import FakeRedis

from janitor.config import read_string as read_config_string
from janitor.debian import dpkg_vendor
from janitor.logs import LogFileManager
from janitor.runner import (
    ActiveRun,
    Backchannel,
    PollingBackchannel,
    QueueItemAlreadyClaimed,
    QueueProcessor,
    WorkerResult,
    _naive_utc,
    committer_env,
    create_app,
    is_log_filename,
    store_change_set,
    store_run,
)
from janitor.vcs import get_vcs_managers


class MemoryLogFileManager(LogFileManager):
    def __init__(self) -> None:
        self.m: dict[tuple[str, str], dict[str, bytes]] = {}

    async def has_log(self, pkg: str, run_id: str, name: str, timeout=None):
        return name in self.m.get((pkg, run_id), {})

    async def get_log(self, pkg: str, run_id: str, name: str, timeout=None):
        try:
            return BytesIO(self.m.get((pkg, run_id), {})[name])
        except KeyError as e:
            raise FileNotFoundError from e

    async def get_ctime(self, pkg: str, run_id: str, name: str):
        if self.has_log(pkg, run_id, name):
            return datetime.utcnow()
        raise FileNotFoundError

    async def import_log(
        self, codebase, run_id, orig_path, timeout=None, mtime=None, basename=None
    ):
        if basename is None:
            basename = os.path.basename(orig_path)
        with open(orig_path, "rb") as f:
            self.m.setdefault((codebase, run_id), {})[basename] = f.read()

    async def delete_log(self, pkg, run_id, name):
        try:
            del self.m.setdefault((pkg, run_id), {})[name]
        except KeyError as e:
            raise FileNotFoundError from e

    async def iter_logs(self):
        for (pkg, run_id), logs in self.m.items():
            for name in logs:
                yield (pkg, run_id, name)


async def create_client(aiohttp_client, queue_processor=None, *, campaigns=None):
    endpoint = aiozipkin.create_endpoint("janitor.runner", ipv4="127.0.0.1", port=80)
    tracer = await aiozipkin.create_custom(endpoint)
    config_text = """\
distribution {
  name: "unstable"
}
"""
    if campaigns:
        for name in campaigns:
            config_text += f"""\
campaign {{
  name: "{name}"
  debian_build {{
    base_distribution: "unstable"
  }}
  default_empty: true
}}
"""
    config = read_config_string(config_text)
    return await aiohttp_client(
        await create_app(
            queue_processor,
            config,
            queue_processor.database if queue_processor else None,
            tracer,
        )
    )


async def test_status(aiohttp_client, db):
    qp = await create_queue_processor(db)
    client = await create_client(aiohttp_client, qp)
    resp = await client.get("/status")
    assert resp.status == 200
    assert {
        "avoid_hosts": [],
        "processing": [],
        "rate_limit_hosts": {},
    } == await resp.json()


async def test_get_active_runs(aiohttp_client, db):
    qp = await create_queue_processor(db)
    client = await create_client(aiohttp_client, qp)
    resp = await client.get("/active-runs")
    assert resp.status == 200
    assert [] == await resp.json()


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
    assert text == "ok"


def test_committer_env():
    assert committer_env(None) == {}
    assert committer_env("Joe Example <joe@example.com>") == {
        "DEBFULLNAME": "Joe Example",
        "DEBEMAIL": "joe@example.com",
        "COMMITTER": "Joe Example <joe@example.com>",
        "BRZ_EMAIL": "Joe Example <joe@example.com>",
        "GIT_COMMITTER_NAME": "Joe Example",
        "GIT_COMMITTER_EMAIL": "joe@example.com",
        "GIT_AUTHOR_NAME": "Joe Example",
        "GIT_AUTHOR_EMAIL": "joe@example.com",
        "EMAIL": "joe@example.com",
    }


def test_is_log_filename():
    assert is_log_filename("foo.log")
    assert is_log_filename("foo.log.1")
    assert not is_log_filename("foo.deb")


async def create_queue_processor(db=None, vcs_managers=None):
    redis = FakeRedis()
    return QueueProcessor(
        db,
        redis,
        run_timeout=30,
        logfile_manager=MemoryLogFileManager(),
        public_vcs_managers=vcs_managers,
    )


async def test_watch_dog():
    qp = await create_queue_processor()
    qp.start_watchdog()
    assert qp._watch_dog is not None
    qp.stop_watchdog()
    assert qp._watch_dog is None


async def test_rate_limit_hosts():
    qp = await create_queue_processor()
    assert [x async for x in qp.rate_limited_hosts()] == []

    retry_after = datetime.utcnow() - timedelta(seconds=30)
    await qp.rate_limited("expired.com", retry_after)
    assert [x async for x in qp.rate_limited_hosts()] == []

    retry_after = datetime.utcnow() + timedelta(seconds=30)
    await qp.rate_limited("github.com", retry_after)

    assert [x async for x in qp.rate_limited_hosts()] == [("github.com", retry_after)]


async def test_status_json():
    qp = await create_queue_processor()
    data = await qp.status_json()
    assert data == {"avoid_hosts": [], "processing": [], "rate_limit_hosts": {}}


async def test_register_run():
    qp = await create_queue_processor()
    assert await qp.active_run_count() == 0
    active_run = ActiveRun(
        campaign="test",
        change_set=None,
        command="blah",
        queue_id=12,
        log_id="some-id",
        start_time=datetime.utcnow(),
        codebase="test-1.1",
        vcs_info={},
        backchannel=Backchannel(),
        worker_name="tester",
        instigated_context=None,
        estimated_duration=timedelta(seconds=10),
    )
    await qp.register_run(active_run)
    assert await qp.active_run_count() == 1
    assert await qp.redis.hkeys("active-runs") == [b"some-id"]
    assert await qp.redis.hkeys("assigned-queue-items") == [b"12"]
    assert await qp.redis.hkeys("last-keepalive") == [b"some-id"]

    assert await qp.get_run("nonexistent-id") is None
    assert (await qp.get_run("some-id")).queue_id == 12
    await qp.unclaim_run("unknown-id")
    await qp.unclaim_run("some-id")
    assert await qp.redis.hkeys("active-runs") == []
    assert await qp.redis.hkeys("assigned-queue-items") == []
    assert await qp.redis.hkeys("last-keepalive") == []
    assert await qp.active_run_count() == 0


async def test_submit_codebase(aiohttp_client, db):
    qp = await create_queue_processor(db)
    client = await create_client(aiohttp_client, qp)
    resp = await client.post(
        "/codebases",
        json=[{"name": "foo", "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200
    assert {} == await resp.json()

    resp = await client.get("/codebases")
    assert resp.status == 200
    assert [
        {
            "name": "foo",
            "branch_url": "https://example.com/foo.git",
            "url": "https://example.com/foo.git",
            "branch": None,
            "subpath": "",
            "vcs_type": None,
            "vcs_last_revision": None,
            "value": None,
            "web_url": None,
        }
    ] == await resp.json()


async def test_submit_codebase_multiple(aiohttp_client, db):
    qp = await create_queue_processor(db)
    client = await create_client(aiohttp_client, qp)
    resp = await client.post(
        "/codebases",
        json=[
            {"name": "foo", "branch_url": "https://example.com/foo.git"},
            {"name": "bar", "branch_url": "https://example.com/bar.git"},
            {"name": "baz", "branch_url": "https://example.com/baz.git"},
        ],
    )
    assert resp.status == 200
    assert {} == await resp.json()

    resp = await client.get("/codebases")
    assert resp.status == 200
    assert {"foo", "bar", "baz"} == {c["name"] for c in await resp.json()}


async def test_candidate_invalid_value(aiohttp_client, db, tmp_path):
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])
    resp = await client.post(
        "/codebases",
        json=[{"name": "foo", "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200

    resp = await client.post(
        "/candidates",
        json=[
            {
                "campaign": "mycampaign",
                "codebase": "foo",
                "command": "true",
                "value": 0,
            }
        ],
    )
    assert resp.status == 200
    assert (await resp.json())["invalid_value"] == [0]


async def test_submit_candidate(aiohttp_client, db, tmp_path):
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])
    resp = await client.post(
        "/codebases",
        json=[{"name": "foo", "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200
    resp = await client.post(
        "/candidates",
        json=[
            {
                "campaign": "mycampaign",
                "codebase": "foo",
                "command": "true",
            }
        ],
    )
    assert resp.status == 200
    [result] = (await resp.json())["success"]
    assert result == {
        "bucket": "default",
        "campaign": "mycampaign",
        "change_set": None,
        "codebase": "foo",
        "estimated_duration": 15.0,
        "offset": 35000.0,
        "queue-id": 1,
        "refresh": False,
    }

    resp = await client.post("/active-runs", json={})
    assert resp.status == 201
    assignment = await resp.json()
    assert assignment == {
        "branch": {
            "additional_colocated_branches": None,
            "cached_url": None,
            "default-empty": True,
            "subpath": "",
            "url": "https://example.com/foo.git",
            "vcs_type": None,
        },
        "build": {
            "config": {
                "build-distribution": "mycampaign",
                "build-extra-repositories": [],
                "build-suffix": "",
                "dep_server_url": None,
                "lintian": {"profile": None},
            },
            "environment": {
                "DEB_VENDOR": dpkg_vendor(),
                "DISTRIBUTION": "unstable",
            },
            "target": "debian",
        },
        "campaign": "mycampaign",
        "codebase": "foo",
        "codemod": {"command": "true", "environment": {}},
        "command": "true",
        "description": "mycampaign on foo",
        "env": {
            "DEB_VENDOR": dpkg_vendor(),
            "DISTRIBUTION": "unstable",
        },
        "force-build": False,
        "id": assignment["id"],
        "queue_id": 1,
        "resume": None,
        "skip-setup-validation": False,
        "target_repository": {"url": None, "vcs_type": None},
    }

    ts = datetime.utcnow().isoformat()

    with MultipartWriter("form-data") as mpwriter:
        mpwriter.append_json(
            {"finish_time": ts, "start_time": ts},
            headers=[  # type: ignore
                (
                    "Content-Disposition",
                    'attachment; filename="result.json"; '
                    "filename*=utf-8''result.json",
                )
            ],
        )  # type: ignore

    resp = await client.post(f"/active-runs/{assignment['id']}/finish", data=mpwriter)
    assert resp.status == 201
    ret = await resp.json()
    cs = ret["result"]["change_set"]
    assert ret == {
        "id": assignment["id"],
        "artifacts": None,
        "filenames": [],
        "logs": [],
        "result": {
            "branches": None,
            "branch_url": None,
            "campaign": "mycampaign",
            "change_set": cs,
            "code": "missing-result-code",
            "codebase": "foo",
            "codemod": None,
            "description": None,
            "duration": 0.0,
            "failure_details": None,
            "failure_stage": None,
            "finish_time": ts,
            "log_id": assignment["id"],
            "logfilenames": [],
            "main_branch_revision": None,
            "remotes": None,
            "resume": None,
            "revision": None,
            "start_time": ts,
            "tags": None,
            "target": {},
            "transient": None,
            "value": None,
        },
    }

    await qp.stop()


async def test_active_runs_exclude_hosts(aiohttp_client, db, tmp_path):
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])
    resp = await client.post(
        "/codebases",
        json=[
            {"name": "foo", "branch_url": "https://example.com/foo.git"},
            {"name": "bar", "branch_url": "https://example.org/bar.git"},
        ],
    )
    assert resp.status == 200
    resp = await client.post(
        "/candidates",
        json=[
            {"campaign": "mycampaign", "codebase": "foo", "command": "true"},
            {"campaign": "mycampaign", "codebase": "bar", "command": "true"},
        ],
    )
    assert resp.status == 200

    # foo is on example.com. A request that excludes example.com skips it
    # and gets bar instead, even though foo would otherwise be picked first
    # (lower queue id).
    resp = await client.post("/active-runs", json={"exclude_hosts": ["example.com"]})
    assert resp.status == 201
    assignment = await resp.json()
    assert assignment["branch"]["url"] == "https://example.org/bar.git"

    # The exclusion applied only to the request above, not globally: a
    # second request with no exclude_hosts still gets the foo candidate.
    resp = await client.post("/active-runs", json={})
    assert resp.status == 201
    assignment = await resp.json()
    assert assignment["branch"]["url"] == "https://example.com/foo.git"

    assert [x async for x in qp.rate_limited_hosts()] == []

    await qp.stop()


async def test_submit_unknown_candidate_codebase(aiohttp_client, db):
    qp = await create_queue_processor(db)
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])
    resp = await client.post(
        "/candidates",
        json=[
            {
                "codebase": "foo",
                "command": "true",
                "campaign": "mycampaign",
            }
        ],
    )
    assert resp.status == 200
    assert ("unknown_codebases", ["foo"]) in (await resp.json()).items()


async def test_submit_unknown_candidate_publish_policy(aiohttp_client, db):
    qp = await create_queue_processor(db)
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])
    resp = await client.post(
        "/codebases",
        json=[{"name": "foo", "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200
    resp = await client.post(
        "/candidates",
        json=[
            {
                "codebase": "foo",
                "command": "true",
                "campaign": "mycampaign",
                "publish-policy": "some-policy",
            }
        ],
    )
    assert resp.status == 200
    assert ("unknown_publish_policies", ["some-policy"]) in (await resp.json()).items()


async def test_submit_unknown_campaign(aiohttp_client, db):
    qp = await create_queue_processor(db)
    client = await create_client(aiohttp_client, qp)
    resp = await client.post(
        "/codebases",
        json=[{"name": "foo", "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200

    resp = await client.post(
        "/candidates", json=[{"campaign": "mycampaign", "codebase": "foo"}]
    )
    assert resp.status == 200
    assert ("unknown_campaigns", ["mycampaign"]) in (await resp.json()).items()


def test_serialize_active_run():
    run = ActiveRun(
        worker_name="myworker",
        worker_link="http://example.com/",
        campaign="mycampaign",
        codebase="foo",
        change_set=None,
        command="ls",
        instigated_context="instigated-context",
        estimated_duration=timedelta(seconds=2),
        queue_id=4242,
        log_id="some-log-id",
        backchannel=Backchannel(),
        start_time=datetime.utcnow(),
        vcs_info={"vcs_type": "git", "branch_url": "http://example.com/foo"},
    )
    orig_json = run.json()
    run_copy = ActiveRun.from_json(orig_json)
    run_copy_json = run_copy.json()
    run_copy_json["current_duration"] = orig_json["current_duration"]
    assert run_copy_json == orig_json
    assert run_copy == run


async def create_dummy_run(
    conn, campaign="mycampaign", run_id="run-id", codebase="foo"
):
    await store_change_set(conn, "run-id", campaign="mycampaign")
    await store_run(
        conn,
        run_id=run_id,
        codebase=codebase,
        campaign=campaign,
        vcs_type="git",
        subpath="",
        start_time=datetime.utcnow(),
        finish_time=datetime.utcnow(),
        command="true",
        result_code="missing-result-code",
        codemod_result={},
        main_branch_revision=b"some-revid",
        revision=b"revid",
        description="Did a thing",
        context=None,
        instigated_context=None,
        logfilenames=[],
        value=1,
        change_set=run_id,
        worker_name=None,
        branch_url="https://example.com/blah",
    )
    return run_id


async def test_tweak_run(aiohttp_client, db, tmp_path):
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    campaign = "mycampaign"
    codebase = "foo"
    client = await create_client(aiohttp_client, qp, campaigns=[campaign])
    resp = await client.post(
        "/codebases",
        json=[{"name": codebase, "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200

    async with db.acquire() as conn:
        run_id = await create_dummy_run(conn, campaign=campaign, codebase=codebase)

    resp = await client.get(f"/runs/{run_id}")
    assert resp.status == 200
    assert {
        "campaign": campaign,
        "codebase": codebase,
        "publish_status": "unknown",
    } == await resp.json()

    resp = await client.post(f"/runs/{run_id}", json={"publish_status": "approved"})
    assert resp.status == 200
    assert {
        "campaign": campaign,
        "codebase": codebase,
        "publish_status": "approved",
        "run_id": run_id,
    } == await resp.json()

    resp = await client.get(f"/runs/{run_id}")
    assert resp.status == 200
    assert {
        "campaign": campaign,
        "codebase": codebase,
        "publish_status": "approved",
    } == await resp.json()


async def test_tweak_unknown_run(aiohttp_client, db, tmp_path):
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])

    resp = await client.get("/runs/run-id")
    assert resp.status == 404

    resp = await client.post("/runs/run-id", json={"publish_status": "approved"})
    assert resp.status == 404


async def test_assignment_with_only_vcs(aiohttp_client, db, tmp_path):
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])
    resp = await client.post(
        "/codebases",
        json=[
            {
                "name": "foo",
                "vcs_type": "hg",
            }
        ],
    )
    assert resp.status == 200
    resp = await client.post(
        "/candidates",
        json=[
            {
                "campaign": "mycampaign",
                "codebase": "foo",
                "command": "true",
            }
        ],
    )
    assert resp.status == 200
    [result] = (await resp.json())["success"]
    assert result == {
        "bucket": "default",
        "campaign": "mycampaign",
        "change_set": None,
        "codebase": "foo",
        "estimated_duration": 15.0,
        "offset": 35000.0,
        "queue-id": 1,
        "refresh": False,
    }

    resp = await client.post("/active-runs", json={})
    assert resp.status == 201, await resp.json()
    assignment = await resp.json()
    assert assignment == {
        "branch": {
            "additional_colocated_branches": None,
            "cached_url": None,
            "default-empty": True,
            "subpath": "",
            "url": None,
            "vcs_type": "hg",
        },
        "build": {
            "config": {
                "build-distribution": "mycampaign",
                "build-extra-repositories": [],
                "build-suffix": "",
                "dep_server_url": None,
                "lintian": {"profile": None},
            },
            "environment": {
                "DEB_VENDOR": dpkg_vendor(),
                "DISTRIBUTION": "unstable",
            },
            "target": "debian",
        },
        "campaign": "mycampaign",
        "codebase": "foo",
        "codemod": {"command": "true", "environment": {}},
        "command": "true",
        "description": "mycampaign on foo",
        "env": {
            "DEB_VENDOR": dpkg_vendor(),
            "DISTRIBUTION": "unstable",
        },
        "force-build": False,
        "id": assignment["id"],
        "queue_id": 1,
        "resume": None,
        "skip-setup-validation": False,
        "target_repository": {"url": None, "vcs_type": "hg"},
    }
    await qp.stop()


def _make_active_run(*, queue_id, log_id, codebase="foo"):
    return ActiveRun(
        campaign="test",
        change_set=None,
        command="blah",
        queue_id=queue_id,
        log_id=log_id,
        start_time=datetime.utcnow(),
        codebase=codebase,
        vcs_info={},
        backchannel=Backchannel(),
        worker_name="tester",
        instigated_context=None,
        estimated_duration=timedelta(seconds=10),
    )


async def test_register_run_rejects_duplicate_queue_id():
    qp = await create_queue_processor()
    await qp.register_run(_make_active_run(queue_id=1, log_id="first"))
    with pytest.raises(QueueItemAlreadyClaimed):
        await qp.register_run(_make_active_run(queue_id=1, log_id="second"))
    # only the first claim's side-effects are visible
    assert await qp.redis.hkeys("active-runs") == [b"first"]
    assert await qp.redis.hkeys("assigned-queue-items") == [b"1"]
    assert await qp.redis.hget("assigned-queue-items", "1") == b"first"


async def test_register_run_concurrent_claims_pick_exactly_one():
    # regression: hget-then-hset let two register_run calls both claim the
    # same queue_id. HSETNX makes the claim atomic - exactly one wins.
    qp = await create_queue_processor()
    results = await asyncio.gather(
        qp.register_run(_make_active_run(queue_id=42, log_id="a")),
        qp.register_run(_make_active_run(queue_id=42, log_id="b")),
        return_exceptions=True,
    )
    successes = [r for r in results if r is None]
    failures = [r for r in results if isinstance(r, QueueItemAlreadyClaimed)]
    assert len(successes) == 1
    assert len(failures) == 1
    winner = await qp.redis.hget("assigned-queue-items", "42")
    assert winner in (b"a", b"b")
    assert await qp.redis.hkeys("active-runs") == [winner]


def test_naive_utc_from_aware_rfc3339():
    assert _naive_utc("2026-08-24T12:34:56+00:00") == datetime(2026, 8, 24, 12, 34, 56)


def test_naive_utc_converts_offset_to_utc():
    # +02:00 wall clock 14:30 is 12:30 UTC
    assert _naive_utc("2026-08-24T14:30:00+02:00") == datetime(2026, 8, 24, 12, 30, 0)


def test_naive_utc_passes_through_naive():
    assert _naive_utc("2026-08-24T12:34:56") == datetime(2026, 8, 24, 12, 34, 56)


def test_naive_utc_result_is_naive():
    # asyncpg rejects aware datetimes for `timestamp without time zone`, so
    # the returned datetime must have tzinfo stripped regardless of input
    for value in ("2026-08-24T12:34:56+00:00", "2026-08-24T14:30:00+02:00"):
        assert _naive_utc(value).tzinfo is None


def test_naive_utc_invalid_raises():
    with pytest.raises(ValueError):
        _naive_utc("not-a-timestamp")


def _minimal_worker_result(**extra):
    result = {"code": "success", "target": {"name": None}}
    result.update(extra)
    return result


def test_worker_result_from_json_aware_timestamps():
    wr = WorkerResult.from_json(
        _minimal_worker_result(
            start_time="2026-08-24T14:30:00+02:00",
            finish_time="2026-08-24T15:00:00+02:00",
        )
    )
    assert wr.start_time == datetime(2026, 8, 24, 12, 30, 0)
    assert wr.finish_time == datetime(2026, 8, 24, 13, 0, 0)
    assert wr.start_time.tzinfo is None
    assert wr.finish_time.tzinfo is None


def test_worker_result_from_json_naive_timestamps():
    wr = WorkerResult.from_json(
        _minimal_worker_result(
            start_time="2026-08-24T12:00:00",
            finish_time="2026-08-24T12:30:00",
        )
    )
    assert wr.start_time == datetime(2026, 8, 24, 12, 0, 0)
    assert wr.finish_time == datetime(2026, 8, 24, 12, 30, 0)


def test_worker_result_from_json_missing_timestamps():
    wr = WorkerResult.from_json(_minimal_worker_result())
    assert wr.start_time is None
    assert wr.finish_time is None


async def _register_dummy_active_run(
    qp, *, campaign="mycampaign", codebase="foo", backchannel=None
):
    # estimate_wait divides queue wait_time by active_run_count; keep the
    # denominator non-zero so scheduling responses don't 500 in tests
    await qp.register_run(
        ActiveRun(
            campaign=campaign,
            change_set=None,
            command="blah",
            queue_id=999,
            log_id="dummy-active-run",
            start_time=datetime.utcnow(),
            codebase=codebase,
            vcs_info={},
            backchannel=backchannel or Backchannel(),
            worker_name="tester",
            instigated_context=None,
            estimated_duration=timedelta(seconds=10),
        )
    )


async def test_kill_no_active_run(aiohttp_client):
    # regression: if the worker restarted mid-run, killing it used to
    # raise a bare NotImplementedError instead of a clear message
    qp = await create_queue_processor()

    worker_app = web.Application()

    async def _handle_kill_no_run(request):
        return web.Response(status=410, text="no run in progress")

    worker_app.router.add_post("/kill", _handle_kill_no_run)
    worker_client = await aiohttp_client(worker_app)

    await _register_dummy_active_run(
        qp, backchannel=PollingBackchannel(my_url=worker_client.make_url("/"))
    )
    client = await create_client(aiohttp_client, qp)

    resp = await client.post("/kill/dummy-active-run")
    assert resp.status == 410
    assert await resp.text() == (
        "worker has no active run - it may have restarted "
        "while this run was in progress"
    )


async def test_kill_not_supported(aiohttp_client):
    qp = await create_queue_processor()

    worker_app = web.Application()

    async def _handle_kill_not_supported(request):
        return web.Response(status=501, text="kill is not yet supported")

    worker_app.router.add_post("/kill", _handle_kill_not_supported)
    worker_client = await aiohttp_client(worker_app)

    await _register_dummy_active_run(
        qp, backchannel=PollingBackchannel(my_url=worker_client.make_url("/"))
    )
    client = await create_client(aiohttp_client, qp)

    resp = await client.post("/kill/dummy-active-run")
    assert resp.status == 501
    assert await resp.text() == "kill is not yet supported"


async def test_schedule_response_includes_queue_position(aiohttp_client, db, tmp_path):
    # regression: the frontend showed "position undefined" because
    # handle_schedule didn't populate queue_position/queue_wait_time
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    await _register_dummy_active_run(qp)
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])
    resp = await client.post(
        "/codebases",
        json=[{"name": "foo", "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200
    resp = await client.post(
        "/candidates",
        json=[{"campaign": "mycampaign", "codebase": "foo", "command": "true"}],
    )
    assert resp.status == 200

    resp = await client.post(
        "/schedule", json={"campaign": "mycampaign", "codebase": "foo"}
    )
    assert resp.status == 200
    body = await resp.json()
    assert body["campaign"] == "mycampaign"
    assert body["codebase"] == "foo"
    assert "queue_position" in body
    assert "queue_wait_time" in body
    assert body["queue_position"] == 1
    assert body["queue_wait_time"] == 0.0
    await qp.stop()


async def test_schedule_with_no_active_runs_does_not_500(aiohttp_client, db, tmp_path):
    # /schedule with zero active runs returns queue_wait_time=None, not a 500.
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])
    resp = await client.post(
        "/codebases",
        json=[{"name": "foo", "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200
    resp = await client.post(
        "/candidates",
        json=[{"campaign": "mycampaign", "codebase": "foo", "command": "true"}],
    )
    assert resp.status == 200

    resp = await client.post(
        "/schedule", json={"campaign": "mycampaign", "codebase": "foo"}
    )
    assert resp.status == 200
    body = await resp.json()
    assert body["queue_wait_time"] is None
    await qp.stop()


async def test_estimate_wait_with_no_active_runs(aiohttp_client, db, tmp_path):
    # estimate_wait's per-worker wait estimate is None when there are no
    # active runs to divide the queue wait time by.
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])
    resp = await client.post(
        "/codebases",
        json=[{"name": "foo", "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200
    resp = await client.post(
        "/candidates",
        json=[{"campaign": "mycampaign", "codebase": "foo", "command": "true"}],
    )
    assert resp.status == 200

    position, per_worker_wait, wait_time = await qp.estimate_wait("foo", "mycampaign")
    assert position == 1
    assert per_worker_wait is None
    await qp.stop()


async def test_schedule_by_run_id_includes_queue_position(aiohttp_client, db, tmp_path):
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    campaign = "mycampaign"
    codebase = "foo"
    await _register_dummy_active_run(qp, campaign=campaign, codebase=codebase)
    client = await create_client(aiohttp_client, qp, campaigns=[campaign])
    resp = await client.post(
        "/codebases",
        json=[{"name": codebase, "branch_url": "https://example.com/foo.git"}],
    )
    assert resp.status == 200
    resp = await client.post(
        "/candidates",
        json=[{"campaign": campaign, "codebase": codebase, "command": "true"}],
    )
    assert resp.status == 200

    async with db.acquire() as conn:
        run_id = await create_dummy_run(conn, campaign=campaign, codebase=codebase)

    resp = await client.post("/schedule", json={"run_id": run_id})
    assert resp.status == 200
    body = await resp.json()
    assert body["campaign"] == campaign
    assert body["codebase"] == codebase
    assert "queue_position" in body
    assert "queue_wait_time" in body
    await qp.stop()


async def test_schedule_unknown_run_id_returns_404(aiohttp_client, db, tmp_path):
    vcs = tmp_path / "vcs"
    vcs.mkdir()
    qp = await create_queue_processor(db, vcs_managers=get_vcs_managers(str(vcs)))
    client = await create_client(aiohttp_client, qp, campaigns=["mycampaign"])

    resp = await client.post("/schedule", json={"run_id": "nonexistent"})
    assert resp.status == 404
    await qp.stop()
