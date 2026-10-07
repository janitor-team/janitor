#!/usr/bin/python
# Copyright (C) 2026 Jelmer Vernooij <jelmer@jelmer.uk>
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

from datetime import timedelta
from typing import cast

import pytest
from breezy.errors import TransportError
from breezy.forge import Forge

import janitor.publish as publish
from janitor import utcnow
from janitor._publish import FixedRateLimiter
from janitor.config import read_string as read_config_string
from janitor.publish import create_app


async def create_client(aiohttp_client, db):
    config = read_config_string("")
    app = await create_app(vcs_managers={}, db=db, redis=None, config=config)
    return await aiohttp_client(app)


async def test_policy_get_not_found(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.get("/policy/does-not-exist")
    assert resp.status == 404


async def test_policy_get(aiohttp_client, db):
    client = await create_client(aiohttp_client, db)
    resp = await client.put(
        "/policy/lintian-fixes",
        json={
            "rate_limit_bucket": "default",
            "per_branch": {
                "main": {"mode": "propose", "max_frequency_days": 7},
            },
        },
    )
    assert resp.status == 200

    resp = await client.get("/policy/lintian-fixes")
    assert resp.status == 200
    assert await resp.json() == {
        "rate_limit_bucket": "default",
        "per_branch": {
            "main": {"mode": "propose", "max_frequency_days": 7},
        },
    }


async def test_credentials_missing_ssh_dir_returns_no_keys(aiohttp_client, monkeypatch):
    from aiohttp import web

    app = web.Application()
    app.router.add_routes(publish.routes)
    app["gpg"] = type("FakeGpg", (), {"keylist": lambda self, secret=False: []})()

    monkeypatch.setattr(publish, "forges", {})
    monkeypatch.setattr(
        publish.os.path, "expanduser", lambda p: "/nonexistent-ssh-dir-for-test"
    )

    client = await aiohttp_client(app)
    resp = await client.get("/credentials")
    assert resp.status == 200
    body = await resp.json()
    assert body["ssh_keys"] == []


class _FakeVcsManager:
    def get_branch_url(self, codebase, branch_name):
        return f"https://example.com/{codebase}/{branch_name}"


async def test_publish_one_sends_revision_id_and_invokes_compiled_binary(monkeypatch):
    captured = {}

    async def fake_run_worker_process(args, request, **kwargs):
        captured["args"] = args
        captured["request"] = request
        return 1, {"code": "some-failure", "description": "boom"}

    monkeypatch.setattr(publish, "run_worker_process", fake_run_worker_process)

    worker = publish.PublishWorker()

    with pytest.raises(publish.PublishFailure):
        await worker.publish_one(
            campaign="lintian-fixes",
            codebase="mypkg",
            command="lintian-brush",
            target_branch_url="https://example.com/mypkg",
            mode="propose",
            role="main",
            revision=b"somerevid",
            log_id="log-1",
            unchanged_id=None,
            derived_branch_name="lintian-fixes",
            rate_limit_bucket=None,
            vcs_manager=_FakeVcsManager(),
        )

    assert captured["args"] == ["janitor-publish-one"]
    assert captured["request"]["revision_id"] == "somerevid"
    assert "revision" not in captured["request"]


async def test_publish_one_passes_template_env_path_to_compiled_binary(monkeypatch):
    captured = {}

    async def fake_run_worker_process(args, request, **kwargs):
        captured["args"] = args
        return 1, {"code": "some-failure", "description": "boom"}

    monkeypatch.setattr(publish, "run_worker_process", fake_run_worker_process)

    worker = publish.PublishWorker(template_env_path="/etc/janitor/templates")

    with pytest.raises(publish.PublishFailure):
        await worker.publish_one(
            campaign="lintian-fixes",
            codebase="mypkg",
            command="lintian-brush",
            target_branch_url="https://example.com/mypkg",
            mode="propose",
            role="main",
            revision=b"somerevid",
            log_id="log-1",
            unchanged_id=None,
            derived_branch_name="lintian-fixes",
            rate_limit_bucket=None,
            vcs_manager=_FakeVcsManager(),
        )

    assert captured["args"] == [
        "janitor-publish-one",
        "--template-env-path=/etc/janitor/templates",
    ]


class _StubForge:
    """Stand-in for a forge instance.

    The scan only uses the forge as a dictionary key and in log messages.
    """

    def __repr__(self):
        return "<StubForge>"


class _StubMergeProposal:
    """A proposal that records the scan reaching it, then gives up.

    Reading a proposal goes out to the forge over the network, so this part
    has to be a stand-in. It raises ForgeLoginRequired, which the scan already
    handles by moving on, so the only thing left to differ between the two
    tests below is whether the rate limit gate let the scan get here at all.
    """

    url = "https://example.com/mypkg/merge_requests/1"

    def __init__(self):
        self.read_attempts = 0

    def get_source_revision(self):
        self.read_attempts += 1
        raise publish.ForgeLoginRequired(self.url)


def _one_proposal(monkeypatch, forge, mp):
    # Only the forge iteration is replaced, since a real one would need a
    # forge to talk to.
    monkeypatch.setattr(
        publish,
        "iter_all_mps",
        lambda statuses=None, unreachable_forges=None: iter([(forge, mp, "open")]),
    )


async def test_check_existing_skips_a_forge_still_inside_its_backoff(
    con, monkeypatch
) -> None:
    forge = cast(Forge, _StubForge())
    mp = _StubMergeProposal()
    _one_proposal(monkeypatch, forge, mp)

    forge_rate_limiter = {forge: utcnow() + timedelta(minutes=30)}

    await publish.check_existing(
        conn=con,
        redis=None,
        config=None,
        publish_worker=None,
        bucket_rate_limiter=publish.NonRateLimiter(),
        forge_rate_limiter=forge_rate_limiter,
        vcs_managers=None,
    )

    assert mp.read_attempts == 0
    assert forge in forge_rate_limiter


async def test_check_existing_drops_a_forge_backoff_that_has_expired(
    con, monkeypatch
) -> None:
    forge = cast(Forge, _StubForge())
    mp = _StubMergeProposal()
    _one_proposal(monkeypatch, forge, mp)

    forge_rate_limiter = {forge: utcnow() - timedelta(minutes=30)}

    await publish.check_existing(
        conn=con,
        redis=None,
        config=None,
        publish_worker=None,
        bucket_rate_limiter=publish.NonRateLimiter(),
        forge_rate_limiter=forge_rate_limiter,
        vcs_managers=None,
    )

    assert mp.read_attempts == 1
    assert forge not in forge_rate_limiter


class _UnreadableMergeProposal(_StubMergeProposal):
    """A proposal whose forge answers the read with an unexpected status."""

    def get_source_revision(self):
        self.read_attempts += 1
        raise publish.UnexpectedHttpStatus(self.url, 502)


async def test_check_existing_keeps_going_after_an_unexpected_status(
    con, monkeypatch
) -> None:
    forge = cast(Forge, _StubForge())
    mp = _UnreadableMergeProposal()
    _one_proposal(monkeypatch, forge, mp)

    await publish.check_existing(
        conn=con,
        redis=None,
        config=None,
        publish_worker=None,
        bucket_rate_limiter=publish.NonRateLimiter(),
        forge_rate_limiter={},
        vcs_managers=None,
    )

    assert mp.read_attempts == 1


class _UnreachableForge:
    """A forge whose API cannot be reached at all."""

    def __repr__(self):
        return "<UnreachableForge>"

    def iter_my_proposals(self, status=None):
        raise TransportError("Connection refused")


class _WorkingForge:
    """A forge that returns one proposal per status."""

    def __repr__(self):
        return "<WorkingForge>"

    def iter_my_proposals(self, status=None):
        yield f"proposal-{status}"


def test_iter_all_mps_skips_an_unreachable_forge(monkeypatch):
    """One forge being unreachable must not stop the others being listed."""
    monkeypatch.setattr(
        publish,
        "iter_forge_instances",
        lambda: iter([_UnreachableForge(), _WorkingForge()]),
    )

    found = list(publish.iter_all_mps(statuses=["open"]))

    assert [mp for _forge, mp, _status in found] == ["proposal-open"]


def test_iter_all_mps_reports_a_forge_it_could_not_reach(monkeypatch):
    """A skipped forge has to be visible to the caller, not just to the log."""
    monkeypatch.setattr(
        publish,
        "iter_forge_instances",
        lambda: iter([_UnreachableForge(), _WorkingForge()]),
    )
    unreachable: list[Forge] = []

    list(publish.iter_all_mps(statuses=["open"], unreachable_forges=unreachable))

    assert [repr(forge) for forge in unreachable] == ["<UnreachableForge>"]


class _PartlyBrokenForge:
    """A forge that serves some statuses and fails on others."""

    def __init__(self, failing):
        self.failing = failing

    def __repr__(self):
        return "<PartlyBrokenForge>"

    def iter_my_proposals(self, status=None):
        if status in self.failing:
            raise TransportError("Connection reset by peer")
        yield f"proposal-{status}"


def test_iter_all_mps_keeps_listing_after_one_status_fails(monkeypatch):
    """A forge that fails partway still has its other statuses listed.

    Losing the rest means those proposals go unscanned, so their status and
    merge details stay as they were and last_scanned is left behind.
    """
    monkeypatch.setattr(
        publish,
        "iter_forge_instances",
        lambda: iter([_PartlyBrokenForge({"merged"})]),
    )

    found = list(publish.iter_all_mps(statuses=["open", "merged", "closed"]))

    assert [mp for _forge, mp, _status in found] == [
        "proposal-open",
        "proposal-closed",
    ]


def test_iter_all_mps_reports_a_forge_once_however_many_statuses_fail(monkeypatch):
    """The report is about the forge, so a forge appears in it at most once."""
    monkeypatch.setattr(
        publish,
        "iter_forge_instances",
        lambda: iter([_PartlyBrokenForge({"open", "merged", "closed"})]),
    )
    unreachable: list[Forge] = []

    list(
        publish.iter_all_mps(
            statuses=["open", "merged", "closed"], unreachable_forges=unreachable
        )
    )

    assert [repr(forge) for forge in unreachable] == ["<PartlyBrokenForge>"]


def _stand_in_for_the_scan(monkeypatch, proposals=(), unreachable=()):
    # Listing proposals goes out to a forge over the network, so this is the
    # one part of a scan that has to be stood in for.
    triples = list(proposals)
    skipped = list(unreachable)

    def iter_all_mps(statuses=None, unreachable_forges=None):
        if unreachable_forges is not None:
            unreachable_forges.extend(skipped)
        return iter(triples)

    monkeypatch.setattr(publish, "iter_all_mps", iter_all_mps)


def _limiter_holding(bucket, count):
    limiter = FixedRateLimiter(5)
    limiter.set_mps_per_bucket({"open": {bucket: count}})
    return limiter


async def _scan(con, bucket_rate_limiter):
    await publish.check_existing(
        conn=con,
        redis=None,
        config=None,
        publish_worker=None,
        bucket_rate_limiter=bucket_rate_limiter,
        forge_rate_limiter={},
        vcs_managers=None,
    )


async def test_check_existing_takes_the_counts_from_a_complete_scan(
    con, monkeypatch
) -> None:
    """A complete scan is the whole picture, so it replaces what came before."""
    bucket_rate_limiter = _limiter_holding("some-bucket", 2)
    _stand_in_for_the_scan(monkeypatch)

    await _scan(con, bucket_rate_limiter)

    assert bucket_rate_limiter.get_stats() == {}


async def test_check_existing_drops_the_counts_from_an_incomplete_scan(
    con, monkeypatch
) -> None:
    """A scan missing a forge must not be reported as the whole picture.

    Otherwise a forge that is briefly unreachable reads as a forge with no
    open proposals, the rate limiter is told the quota those proposals take
    up is free, and the next cycle publishes on top of them.
    """
    bucket_rate_limiter = _limiter_holding("some-bucket", 2)
    _stand_in_for_the_scan(monkeypatch, unreachable=[_StubForge()])

    await _scan(con, bucket_rate_limiter)

    assert bucket_rate_limiter.get_stats() == {"some-bucket": 2}
