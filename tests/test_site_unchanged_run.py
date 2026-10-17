from janitor.site.common import get_unchanged_run


async def test_get_unchanged_run_with_own_change_set(con):
    await con.execute(
        "INSERT INTO codebase (name, branch_url, url) "
        "VALUES ('pkg', 'https://example.com/pkg', 'https://example.com/pkg')"
    )
    await con.execute(
        "INSERT INTO change_set (id, campaign) "
        "VALUES ('control-run', 'control'), ('cs-other', 'unchanged')"
    )
    # The control run has a change set of its own, named after the run.
    # The later run at the same revision is part of another change set.
    await con.execute(
        "INSERT INTO run (id, codebase, suite, change_set, result_code, "
        "revision, main_branch_revision, start_time, finish_time, logfilenames) "
        "VALUES ('control-run', 'pkg', 'control', 'control-run', 'success', "
        "'base-rev', 'base-rev', "
        "now() - interval '2 hours', now() - interval '1 hour', '{}'), "
        "('other-run', 'pkg', 'unchanged', 'cs-other', 'success', "
        "'base-rev', 'base-rev', "
        "now() - interval '30 minutes', now() - interval '10 minutes', '{}')"
    )
    await con.execute(
        "INSERT INTO debian_build (run_id, version, distribution, source) "
        "VALUES ('control-run', '1.0-1~jan+unchanged1', 'unchanged', 'pkg')"
    )

    run = await get_unchanged_run(con, "pkg", b"base-rev")
    assert run is not None
    assert run["id"] == "control-run"
    assert run["build_version"] == "1.0-1~jan+unchanged1"

    assert await get_unchanged_run(con, "pkg", b"other-rev") is None
