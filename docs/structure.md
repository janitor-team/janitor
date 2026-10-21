## Structure 

- `./reschedule.py` - a tool for users of the janitor and can be run by anybody locally
- `./helpers/*` - all need to run inside of a janitor deployment (and talk to the database, etc) by an admin.
- `janitor-admin` - admin CLI that talks to a running janitor site over its public HTTP API (`--url` or `JANITOR_URL`, required). Mutating subcommands need `--user`/`--password` (or `JANITOR_USER`/`JANITOR_PASSWORD`) for HTTP basic auth. Run with `cargo run --bin janitor-admin -- <args>`.
  - `worker list` - list registered workers with their run counts
  - `worker add <name> [--link <url>]` - register a new worker; prints the generated password, shown once
  - `worker delete <name>` - remove a worker
  - `reschedule --run-id <id> [--refresh] [--offset <secs>]` - reschedule a single run
  - `reschedule --result-code <code> [--description-re <re>] [--campaign <name>] [--rejected] [--min-age <days>] [--include-transient] [--refresh] [--offset <secs>]` - mass-reschedule runs matching the given result code
  - `queue list [--limit <n>]` - show queue items, default limit 100
  - `run list` - list active runs
  - `run kill <run-id>` - kill an active run
