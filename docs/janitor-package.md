# janitor-package

`janitor-package` is a command-line client for the actions a logged-in,
non-admin janitor user can take: the things a package maintainer or QA
reviewer does from the web UI rather than from the shell on the janitor
host itself. It talks to a running janitor site instance over its public
HTTP API, the same way [`janitor-admin`](structure.md) does for admin-only
actions (worker management, mass-reschedules, log reprocessing, and
publisher passes). If an action needs admin rights, it lives in
`janitor-admin`, not here.

Typical uses: kicking off a publish attempt for a codebase you maintain,
approving or rejecting a run from the review queue, rescheduling a run that
failed transiently, and pulling a run's log or diff without going through
the browser.

## Building and running

```console
$ cargo build --bin janitor-package
$ ./target/debug/janitor-package --url http://localhost:8090 <command>
```

The base URL, and HTTP basic auth credentials if the instance needs them,
can be passed as flags or environment variables:

```console
$ export JANITOR_URL=http://localhost:8090
$ export JANITOR_USER=jrandom
$ export JANITOR_PASSWORD=hunter2
$ janitor-package publish trigger lintian-fixes mypkg
```

`--url` defaults to `https://janitor.debian.net` if unset. Most janitor
deployments authenticate browser sessions via OpenID rather than HTTP basic
auth; `--user`/`--password` are there for instances that also accept basic
auth on the API, and can simply be left unset against ones that don't -
some of the routes below (like triggering a publish) work for anonymous
callers too, just attributed to "user from web UI" rather than a named
account.

Every subcommand prints an error to stderr and exits non-zero on failure,
so `janitor-package ... || echo failed` works as expected in scripts.

## `publish` - trigger and inspect publish attempts

### `publish trigger <campaign> <codebase> [--mode <mode>]`

Ask the publisher to attempt a publish for a codebase in a campaign right
now, instead of waiting for the next scheduled publish pass. `--mode`
overrides the codebase's configured publish mode for this one attempt; it
accepts `push-derived`, `push`, `propose`, or `attempt-push`. Leave it unset
to use whatever mode the codebase is already configured for.

```console
$ janitor-package publish trigger lintian-fixes mypkg
{
  "id": "9c1b2f3a-...",
  "mode": "propose",
  ...
}

$ janitor-package publish trigger lintian-fixes mypkg --mode propose
```

The command prints the publisher's JSON response either way; on a
publisher-side error, the response body is still printed (it usually
explains what went wrong, e.g. "no unpublished changes") and the process
exits non-zero.

### `publish show <publish-id>`

Show the recorded detail of a single publish attempt, by the ID printed by
`publish trigger` or shown in the web UI's publish history.

```console
$ janitor-package publish show 9c1b2f3a-1234-4a5b-9c1d-abcdef012345
```

## `review` - QA review queue

### `review submit <run-id> <verdict> [--comment <text>] [--publishable-only <bool>] [--suite <campaign>]...`

Submit a review verdict for a run. `<verdict>` is one of `approve`,
`reject`, `reschedule`, or `abstain`. A `reschedule` verdict rejects the
current run and immediately schedules a fresh one for the same
codebase/campaign; an `abstain` verdict records that you looked at the run
without taking a position on it.

```console
$ janitor-package review submit a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab approve \
    --comment "LGTM, minor changelog nit only"

$ janitor-package review submit a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab reschedule \
    --comment "flaky build, please retry"
```

The underlying endpoint (`POST /cupboard/review`) re-renders the review
queue as an HTML page rather than returning JSON, since that's what the web
UI's review form posts to; `--suite`/`--publishable-only` only affect that
discarded HTML, not the verdict itself, so there's normally no need to pass
them. `janitor-package` doesn't print the HTML back - it just confirms the
verdict was accepted.

### `review needs-review [--campaign <campaign>] [--reviewer <email>] [--publishable-only <bool>] [--required-only <bool>] [--limit <n>]`

List runs waiting for review, in the order the web UI's review queue would
show them.

```console
$ janitor-package review needs-review --campaign lintian-fixes --limit 20
CODEBASE                              CAMPAIGN                 RUN ID
mypkg                                 lintian-fixes             a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab
otherpkg                              lintian-fixes             b2c3d4e5-6a7b-4c5d-8e9f-234567890abc
```

`--reviewer` defaults to the authenticated user; `--publishable-only`
defaults to `true` (only runs that are actually ready to publish), matching
the web UI's default view.

## `run` - reschedule and inspect runs

### `run reschedule <run-id> [--offset <n>] [--refresh]`

Reschedule a single run, keeping its existing queue bucket. `--offset` can
be negative to move it closer to the top of the queue; `--refresh` forces a
clean rebuild instead of reusing cached build artifacts.

```console
$ janitor-package run reschedule a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab --refresh
```

### `run schedule-control <run-id> [--offset <n>] [--refresh]`

Reschedule a run via the QA-reviewer "queue jump" control endpoint. Same
arguments as `run reschedule`; the response additionally includes the run's
new position and estimated wait time in the queue.

```console
$ janitor-package run schedule-control a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab --offset -10
```

### `run active`

List runs currently in progress on a worker.

```console
$ janitor-package run active
```

### `run peek`

Show the next run that would be handed out to a worker, without actually
assigning it. Useful for sanity-checking the queue ordering.

```console
$ janitor-package run peek
```

### `run show <run-id>`

Show a single active (in-progress) run's detail. Returns a "not found"
error for runs that have already finished - this only covers runs a
worker currently has checked out, not run history in general.

```console
$ janitor-package run show a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab
```

### `run log <run-id> [<filename>]`

Without a filename, list the log files available for a run. With one,
print that log file's content to stdout - this is what you want for
watching a build or dist log:

```console
$ janitor-package run log a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab
build.log
dist.log

$ janitor-package run log a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab build.log
... build output ...
```

### `run diff <run-id> [--role <role>]`

Print the VCS diff a run produced, as plain `text/x-diff`. `--role`
selects a specific result branch when a run produced more than one (for
example, packaging changes vs. an upstream merge); it defaults to `main`.

```console
$ janitor-package run diff a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab
$ janitor-package run diff a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab --role upstream
```

### `run debdiff <run-id> [--filter-boring]` / `run diffoscope <run-id> [--filter-boring]`

Print the debdiff or diffoscope output comparing a run's build against the
last successful, unchanged build for the same codebase. `--filter-boring`
asks the differ to drop uninteresting differences (e.g. pure changelog or
timestamp churn).

```console
$ janitor-package run debdiff a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab --filter-boring
$ janitor-package run diffoscope a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab
```

Both can fail with a "not calculated yet" or "no matching unchanged build"
error if the differ hasn't produced a result yet, or there's nothing to
compare against.

## `merge-proposals` - browse and refresh merge proposals

### `merge-proposals list [--codebase <codebase> | --campaign <campaign>]`

List merge proposals, optionally restricted to a single codebase or
campaign (the two are mutually exclusive). With neither, lists every known
merge proposal.

```console
$ janitor-package merge-proposals list --codebase mypkg
$ janitor-package merge-proposals list --campaign lintian-fixes
$ janitor-package merge-proposals list
```

Note: as of this writing, all three of the underlying listing routes
(`/merge-proposals`, `/c/{codebase}/merge-proposals`,
`/{campaign}/merge-proposals`) aren't implemented server-side yet and
return `501 Not Implemented`. `janitor-package merge-proposals list`
will report an `HTTP 501` error until the publisher side is filled in.

### `merge-proposals refresh-status <url>`

Ask the publisher to re-check a merge proposal's status, for example after
it was merged or closed outside of the janitor's own bookkeeping.

```console
$ janitor-package merge-proposals refresh-status https://github.com/example/mypkg/pull/1
Success
```

## `status`

Show the runner's current status: how many workers are active, queue
depth, and similar operational detail. This is the same information the
janitor site's own status indicators are built from.

```console
$ janitor-package status
```

## What's *not* here

Queue listing (`GET /queue`) is intentionally not duplicated here:
`janitor-admin queue list` already talks to this exact route (it's not
actually admin-gated server-side), so use that instead of a second
`janitor-package` command for the same data.

Every other action that requires admin rights - worker registration,
mass-reschedules, log reprocessing, autopublish/scan passes, and merge
proposal status overrides via `POST /merge-proposal` - lives in
`janitor-admin`, not here, because the server rejects them for non-admin
credentials regardless of which CLI you send them from.
