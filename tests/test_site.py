#!/usr/bin/python
# Copyright (C) 2019 Jelmer Vernooij <jelmer@jelmer.uk>
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

from datetime import datetime, timedelta
from unittest.mock import MagicMock

import asyncpg
import pytest
from aiohttp import web
from yarl import URL

from janitor.site import format_duration, format_timestamp, update_vars_from_request
from janitor.site.common import bad_row_count_as_400, parse_int_query_param


def test_duration():
    assert "10s" == format_duration(timedelta(seconds=10))
    assert "1m10s" == format_duration(timedelta(seconds=70))
    assert "1h0m" == format_duration(timedelta(hours=1))
    assert "1d1h" == format_duration(timedelta(days=1, hours=1))
    assert "2w1d" == format_duration(timedelta(weeks=2, days=1))


def test_timestamp():
    assert "2022-10-01T11:10" == format_timestamp(datetime(2022, 10, 1, 11, 10, 22))


def _make_request(external_url=None, request_url="https://example.com/some/path"):
    app = MagicMock()
    app.__getitem__.side_effect = {
        "external_url": external_url,
        "config": MagicMock(campaign=[]),
    }.__getitem__
    app.__contains__.side_effect = lambda k: k in {"config", "external_url"}
    request = MagicMock()
    request.__getitem__.side_effect = {"user": None}.__getitem__
    request.rel_url = URL(request_url).relative()
    request.url = URL(request_url)
    request.app = app
    return request


def test_update_vars_from_request_manager_urls_include_vcs_prefix():
    # Regression test for #1253: the VCS manager base URLs supplied to the
    # template context must produce repository URLs that keep the "/git/" and
    # "/bzr/" path prefix, not drop them via Url::join replacing the last
    # segment.
    vs: dict = {}
    update_vars_from_request(vs, _make_request())
    assert (
        vs["git_vcs_manager"].get_repository_url("mycb")
        == "https://example.com/git/mycb"
    )
    assert (
        vs["bzr_vcs_manager"].get_repository_url("mycb")
        == "https://example.com/bzr/mycb"
    )


def test_update_vars_from_request_uses_external_url():
    vs: dict = {}
    update_vars_from_request(
        vs, _make_request(external_url=URL("https://public.example.org/"))
    )
    assert (
        vs["git_vcs_manager"].get_repository_url("mycb")
        == "https://public.example.org/git/mycb"
    )
    assert (
        vs["bzr_vcs_manager"].get_repository_url("mycb")
        == "https://public.example.org/bzr/mycb"
    )


async def _parse_int(aiohttp_client, name, default, **query):
    async def handle(request):
        return web.Response(text=repr(parse_int_query_param(request, name, default)))

    app = web.Application()
    app.router.add_get("/", handle)
    client = await aiohttp_client(app)
    resp = await client.get("/", params=query)
    return resp.status, await resp.text()


async def test_parse_int_query_param_reads_and_defaults(aiohttp_client):
    assert await _parse_int(aiohttp_client, "limit", 100, limit="5") == (200, "5")
    assert await _parse_int(aiohttp_client, "limit", 100) == (200, "100")
    assert await _parse_int(aiohttp_client, "offset", 0, offset="7", limit="5") == (
        200,
        "7",
    )


async def test_parse_int_query_param_leaves_the_range_to_the_database(aiohttp_client):
    assert await _parse_int(aiohttp_client, "limit", 100, limit="-1") == (200, "-1")
    assert await _parse_int(aiohttp_client, "limit", 100, limit=str(2**63)) == (
        200,
        str(2**63),
    )


@pytest.mark.parametrize("value", ["abc", "", "1.5"])
async def test_parse_int_query_param_rejects_a_non_integer(aiohttp_client, value):
    assert await _parse_int(aiohttp_client, "limit", 100, limit=value) == (
        400,
        "limit must be an integer",
    )


async def test_parse_int_query_param_names_the_parameter_it_read(aiohttp_client):
    assert await _parse_int(aiohttp_client, "offset", 0, offset="abc") == (
        400,
        "offset must be an integer",
    )


@pytest.mark.parametrize(
    "error",
    [
        # asyncpg's own codec raises a bare DataError before postgres sees the value.
        asyncpg.DataError(
            "invalid input for query argument $1: 99999999999999999999 "
            "(value out of int64 range)"
        ),
        asyncpg.InvalidRowCountInLimitClauseError("LIMIT must not be negative"),
        asyncpg.InvalidRowCountInResultOffsetClauseError("OFFSET must not be negative"),
        asyncpg.NumericValueOutOfRangeError("bigint out of range"),
    ],
)
async def test_bad_row_count_as_400_covers_the_data_error_family(error):
    with pytest.raises(web.HTTPBadRequest) as excinfo:
        async with bad_row_count_as_400():
            raise error
    assert excinfo.value.text == str(error)


async def test_bad_row_count_as_400_leaves_other_errors_alone():
    with pytest.raises(asyncpg.UniqueViolationError):
        async with bad_row_count_as_400():
            raise asyncpg.UniqueViolationError("duplicate key value")
