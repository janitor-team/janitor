## Tools and scripts

The services run as containers, see [Dockerfiles_.md](Dockerfiles_.md).
This page covers the other commands and files in the tree.

### Commands

`janitor-admin` and `janitor-schedule` are built by `cargo build` at the
top of the tree, and `debian-build` and `generic-build` by
`cargo build -p janitor-worker`; neither pip nor the containers install
them.

- `janitor-admin` - command line client for administering a running
  instance over its HTTP API: workers, rescheduling, the queue and active
  runs. See [production.md](production.md).
- `janitor-schedule` - adds a run to the queue for every candidate, at a
  position worked out from its value and earlier runs (see
  [flow.md](flow.md)). The database location comes from the file given
  with `--config` (default `janitor.conf`). `--campaign` and the codebase
  names given as arguments limit what is considered, `--refresh` asks for
  runs from scratch, and `--dry-run` does not touch the queue.
  `python -m janitor.schedule` is the Python version of the same job,
  without `--refresh`.
- `janitor-publish-one` - publishes the result of a single run. The publisher
  starts it for every publish; it reads a JSON request on standard input and
  writes a JSON result to standard output. `--template-env-path` is the
  directory with the merge proposal templates.
- `janitor-dist` - builds the upstream tarball for a tree, optionally inside a
  schroot (`--schroot`, or the `SCHROOT` environment variable). The worker
  does not run it itself; it passes the command line to the codemod of a
  Debian campaign in the `DIST` environment variable, for scripts that need
  an upstream tarball. It reads `PACKAGE` and `VERSION` from the environment
  and, when `DIST_RESULT` is set, writes a JSON description of a failure
  there.
- `debian-build` and `generic-build` - run the worker's build step on the tree
  in the current directory, outside a worker. `--config` is a JSON file with
  the build configuration, `--output-directory` is where the results go, and
  the outcome is printed as JSON. Useful when debugging a build.
- `janitor-webhook` (`python -m janitor.site.webhook`) - goes through the
  runner's codebases and registers a push webhook, pointing at the callback
  URL given, for those hosted on GitHub or GitLab. `--runner-url` is where it
  asks for the list of codebases.
- `python -m janitor.artifacts list LOCATION` - lists the run ids that have
  artifacts in an artifact store.
- `python -m janitor.config FILE` - reads a configuration file, which fails if
  it does not parse.
- `python -m janitor.vcs URL` - opens a branch the way the janitor does, as a
  check that it is reachable. A local branch needs a `file://` URL.

### Scripts in the tree

- `reschedule.py` - asks an instance to reschedule the runs with a given
  result code. `janitor-admin reschedule` does the same.
- `reprocess-build-results.py` - asks an instance to process the logs of
  runs again: those given with `--run-id`, or otherwise every run that
  failed while building. `--reschedule` queues a new run where the result
  code changed and `--dry-run` only reports. Like `reschedule.py`, it
  talks to the instance given with `--base-url`, which only accepts these
  requests from an admin.
- `create-sbuild-chroot-unshare.py` and `create-sbuild-chroot-schroot.py` -
  create the chroot `sbuild` builds in, for its unshare and schroot modes,
  one for each distribution in `janitor.conf`, and give it an extra name
  for the `build_distribution` of every campaign based on that
  distribution. Both are installed with the janitor Python package, which
  they need; the schroot one also needs `iniparse` (in the `debian` extra)
  and does nothing unless `--base-directory` is given.
- `helpers/cleanup-repositories.py` - removes the forks that no open merge
  proposal uses any more, for forges that limit how many repositories an
  account can own. With `--dry-run` nothing is deleted.
- `helpers/migrate-logs.py` - moves the logs of every run from one log store
  to another.
- `helpers/render-publish-template.py` - prints the merge proposal description
  for a run.
- `run_worker.sh` (and `pull_worker.sh`, a link to it) - starts
  `janitor-worker` from a checkout, with `AUTOPKGTEST` pointing at
  `autopkgtest-wrapper` and, unless it is already set, `SBUILD_CONFIG` at an
  `sbuildrc` next to the script.
- `autopkgtest-wrapper` - runs `autopkgtest` and does not count skipped tests
  as a failure. The worker image sets `AUTOPKGTEST` to it.

### Other files

- `sbuildrc.example` - `sbuild` settings to copy to `~/.sbuildrc` on a host
  that runs `sbuild` outside a worker container.
- `sieve/` - a Sieve filter and notes for feeding GitLab's notification mail
  to `janitor-mail-filter`, so that a merge proposal's status is refreshed as
  soon as the mail arrives.
- `examples/janitor.rules` - Prometheus alerting rules for the services.
