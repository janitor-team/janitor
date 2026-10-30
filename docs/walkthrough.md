# A codebase's journey through the Janitor

This walks through the real path a codebase takes end to end: how it gets
into the system, how to watch the janitor work on it, and where to find its
logs both while it's running and after it's done. It's meant to be readable
standalone, with runnable examples against the raw HTTP API, and it points
at [flow.md](flow.md) and `devnotes/overview.rst` rather than repeating what
they already cover.

## Getting a codebase and a candidate in

The janitor only works on codebases and candidates it already knows about.
A codebase is a place in version control; a candidate says "run this
campaign against that codebase". Both are uploaded to the runner's own
admin API (`janitor.runner`, port 9911 by default, set with `--port`) -
this is a separate, internal API from the public site, and it isn't
authenticated, so it's normally only reachable from inside the deployment.

First the codebase:

```console
$ curl -X POST http://localhost:9911/codebases \
    -H 'Content-Type: application/json' \
    -d '[{"name": "my-project", "branch_url": "https://salsa.debian.org/example/my-project"}]'
```

`name` and `branch_url` are the only fields you need; `vcs_type`,
`subpath`, `web_url` and a few others are optional. The insert is an
upsert keyed on `name`, so re-posting the same name updates the existing
row instead of creating a duplicate.

Then the candidate, once the codebase exists (candidates have a foreign key
on `codebase`, so posting one before its codebase is uploaded just gets it
reported back as unknown rather than erroring out):

```console
$ curl -X POST http://localhost:9911/candidates \
    -H 'Content-Type: application/json' \
    -d '[{"codebase": "my-project", "campaign": "lintian-fixes"}]'
```

`campaign` has to match the name of a `campaign` block in the instance's
`janitor.conf` (see `campaign { name: "lintian-fixes" ... }` in
`janitor.conf.example`); if you don't pass `command`, the campaign's
configured default command is used. The response reports what actually
happened per candidate:

```json
{
  "success": [
    {
      "campaign": "lintian-fixes",
      "codebase": "my-project",
      "bucket": null,
      "change_set": null,
      "offset": 3,
      "estimated_duration": null,
      "queue-id": 12345,
      "refresh": false
    }
  ],
  "invalid_command": [],
  "invalid_value": [],
  "unknown_campaigns": [],
  "unknown_codebases": [],
  "unknown_publish_policies": []
}
```

A non-empty `unknown_codebases` or `unknown_campaigns` list means exactly
what it says - fix the name and re-post, nothing else was inserted for that
entry. `queue-id` is the queue row the candidate was scheduled into; you'll
need it (or the codebase/campaign pair) for the next step.

### The webhook alternative

`POST /webhook` on the public site accepts GitHub/GitLab/Gitea/Gogs/Launchpad
push payloads and reschedules any codebase whose `branch_url` already
matches. It's a convenient way to get faster reruns for codebases the
janitor already tracks, but it does not create new codebases or
candidates - for that you still need the two calls above.

## Watching it enter the queue

Once a candidate is scheduled, it sits in the queue until a worker picks it
up. `GET /api/queue` on the public site (also what `janitor-admin queue
list` calls) lists what's waiting; the `/cupboard/queue` page renders the
same information for humans, plus a "currently processing" table.

```console
$ curl http://localhost:8090/api/queue
```

`/cupboard/queue` holds open a WebSocket connection to `/ws/notifications`
and updates its "currently processing" table in place whenever the server
pushes a "queue" message, so leaving the tab open is enough to watch entries
appear and disappear as workers pick them up.

## Watching it process

While something is running, `GET /api/active-runs` lists everything a
worker currently has in progress, and `GET /api/active-runs/{run_id}` gets
just one (404 if it's no longer active - runs don't stay "active" once
they finish):

```console
$ curl http://localhost:8090/api/active-runs
$ curl http://localhost:8090/api/active-runs/<run-id>
```

Each entry includes `id`, `codebase`, `campaign`, `command`,
`estimated_duration`, `current_duration`, `start_time` and more.
`janitor-admin run list` wraps the same call, and `janitor-admin run kill
<run-id>` can stop one.

### Logs while it's running

This is genuinely a different code path from logs on a finished run, and
nothing else documents the split explicitly, so it's worth being precise
about it. While a run is active, its log files live on the worker itself
and are only reachable by proxying through the runner's live backchannel to
that worker:

```console
$ curl http://localhost:8090/api/active-runs/<run-id>/log/
$ curl http://localhost:8090/api/active-runs/<run-id>/log/worker.log
```

The index endpoint content-negotiates on `Accept`: `application/json`
gets you the raw filename list, `text/plain` gets one filename per line,
and `text/html` (the default in a browser) renders the same list as a
page. The per-file endpoint streams the log file's contents as `text/plain`
as of that request - each call gets you whatever's been written so far, not
a held-open connection that keeps growing, so poll it again for later
output. Once the run finishes and the worker goes away, these endpoints
stop working - that's the signal to switch to the finished-run logs below,
not a sign that something broke.

## Once it's finished

A finished run gets its own page, keyed by codebase and run ID, not run ID
alone:

```console
$ curl http://localhost:8090/cupboard/c/my-project/<run-id>/
```

This is where the split from the previous section matters: these logs are
no longer being proxied from a live worker. They're stored artifacts -
`codemod.log`, `build.log`, `dist.log` and `worker.log` (whichever of
those exist for that run's result) - fetched from wherever the instance's
`logfile_manager` keeps completed logs, and rendered inline on the run
page. The active-run log endpoints above won't serve them; the run page is
the way to get at them once the run is no longer active.

The same page shows the result code, description, and - if the campaign
produces one - the diff. To fetch the raw diff on its own:

```console
$ curl http://localhost:8090/api/run/<run-id>/diff
```

## What happens next

Whether a successful run actually turns into a push or a merge proposal is
the publisher's job, governed by the candidate's `publish_policy` and
per-maintainer rate limiting - see "The publisher" in
`devnotes/overview.rst` for how that decision gets made, and
[flow.md](flow.md) for how the run got scheduled in the first place.
