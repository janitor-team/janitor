from janitor.queue import Queue


async def test_get_buckets(con):
    queue = Queue(con)
    assert await queue.get_buckets() == []


async def test_add(con):
    queue = Queue(con)
    await con.execute("INSERT INTO codebase (name) VALUES ('foo')")
    assert await queue.add(codebase="foo", campaign="bar", command="true") == (
        1,
        "default",
    )
    queue_item, vcs_info = await queue.next_item()
    assert queue_item
    assert queue_item.codebase == "foo"
    assert queue_item.campaign == "bar"


async def test_double_add(con):
    queue = Queue(con)
    await con.execute("INSERT INTO codebase (name) VALUES ('foo')")
    assert await queue.add(codebase="foo", campaign="bar", command="true") == (
        1,
        "default",
    )
    assert await queue.add(codebase="foo", campaign="bar", command="true") == (
        1,
        "default",
    )


async def test_vcs_only(con):
    queue = Queue(con)
    await con.execute(
        "INSERT INTO codebase (name, vcs_type, branch_url) VALUES ('foo', 'git', NULL)"
    )
    assert await queue.add(codebase="foo", campaign="bar", command="true") == (
        1,
        "default",
    )
    queue_item, vcs_info = await queue.next_item()
    assert queue_item
    assert queue_item.codebase == "foo"
    assert queue_item.campaign == "bar"
    assert vcs_info == {"vcs_type": "git", "subpath": ""}


async def test_next_item_filters_by_campaign(con):
    # regression: a missing `f` on the campaign condition sent Postgres the
    # literal `${len(args)}`, so any call with campaign= raised a syntax error
    queue = Queue(con)
    await con.execute("INSERT INTO codebase (name) VALUES ('foo')")
    await queue.add(codebase="foo", campaign="alpha", command="true")
    await queue.add(codebase="foo", campaign="beta", command="true")

    queue_item, _ = await queue.next_item(campaign="beta")
    assert queue_item is not None
    assert queue_item.campaign == "beta"


async def test_next_item_campaign_no_match(con):
    queue = Queue(con)
    await con.execute("INSERT INTO codebase (name) VALUES ('foo')")
    await queue.add(codebase="foo", campaign="alpha", command="true")

    queue_item, _ = await queue.next_item(campaign="beta")
    assert queue_item is None


async def test_iter_queue_limit_zero(con):
    queue = Queue(con)
    await con.execute("INSERT INTO codebase (name) VALUES ('foo'), ('bar')")
    await queue.add(codebase="foo", campaign="mycampaign", command="true")
    await queue.add(codebase="bar", campaign="mycampaign", command="true")
    assert sorted([item.codebase async for item in queue.iter_queue()]) == [
        "bar",
        "foo",
    ]
    assert [item.codebase async for item in queue.iter_queue(limit=1)] == ["foo"]
    assert [item.codebase async for item in queue.iter_queue(limit=0)] == []
