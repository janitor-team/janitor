# Command-line tools

## `janitor-admin`

`janitor-admin` is the admin CLI. It talks to a running Janitor site over its
public HTTP API. Run it with `cargo run --bin janitor-admin -- <args>` from the
top of the tree. No container image ships it.

`--url` (or `JANITOR_URL`) is the site to talk to. It defaults to
`https://janitor.debian.net`, so **set it to your own instance**.
`--user`/`--password` (or `JANITOR_USER`/`JANITOR_PASSWORD`) are sent as HTTP
basic auth.

Subcommands:

- `worker list`: list registered workers with their run counts
- `worker add <name> [--link <url>]`: register a new worker and print the
  generated password, shown once
- `worker delete <name>`: remove a worker
- `reschedule --run-id <id> [--refresh] [--offset <secs>]`: reschedule a single
  run
- `reschedule --result-code <code> [--description-re <re>] [--campaign <name>] [--rejected] [--min-age <days>] [--include-transient] [--refresh] [--offset <secs>]`:
  mass-reschedule runs with the given result code
- `queue list [--limit <n>]`: show queue items, default limit 100
- `run list`: list active runs
- `run kill <run-id>`: kill an active run

`queue list` and `run list` need no login. The other subcommands call site
APIs that need a logged-in user (`reschedule --run-id`) or an admin (all the
rest), and the site only takes that from a web UI login session, not from
HTTP basic auth. So today they fail with HTTP 401.

## `reschedule.py`

`./reschedule.py <result-code>` is meant to mass-reschedule runs with that
{term}`result code`, from any machine, through the site's admin API
(`/cupboard/api/mass-reschedule`). It sends no login, and that API only takes a
web UI login session, so today it fails like the admin subcommands of
`janitor-admin`. It also sends its fields in the query string, while the site
reads them from the form body.

`--base-url` defaults to `https://janitor.debian.net`, so **set it to your own
instance**.

## `helpers/`

An admin runs these inside a Janitor deployment, with its credentials.

- `helpers/cleanup-repositories.py [--dry-run]`: deletes the forks the
  Janitor's forge accounts own that no open merge proposal uses any more (some
  forges limit how many repositories one user can own)
- `helpers/migrate-logs.py <from> <to>`: moves run logs from one logs location
  to another, using the database from `janitor.conf`
- `helpers/render-publish-template.py --run-id <id>`: prints the merge proposal
  text for a run, from the publish templates
