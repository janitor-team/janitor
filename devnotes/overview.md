# How Janitor works

The Janitor can manage automated changes to a set of projects in version
control.

It does this through a number of interconnected services, each of which
provides some basic features:

- a REST API
- `/metrics` for use with Prometheus
- `/health` and `/ready` for use with Kubernetes or other health-checking
  services (not every service has all three)

Access to the REST APIs is unauthenticated and is meant to be restricted to
the other services and administrators.

Some services also provide a separate REST API on a different port, for
exposure to the internet. In particular (ports as set in the container
images):

- `janitor-runner`: port 9919, used by the workers, and a reverse proxy
  usually serves it under `/runner/`
- `janitor-git-store`: port 9924, `/git/`
- `janitor-bzr-store`: port 9930, `/bzr/`
- `janitor-site`: port 8090, the web UI and the public API

`janitor-archive` serves `/dists` on its only port, 9914.

## The runner

A {term}`codebase` is a place in a VCS repository that changes can be made to.
Generally, it will refer to the root of a branch in a repository (either Git or
Bazaar), but it can also refer to a subpath. Codebases can also be
{term}`nascent`, which means that they don't exist anywhere yet, but are
created and populated by the Janitor.

{term}`Campaigns <campaign>` are efforts to fix a particular issue across the
codebases. Campaigns do not have to be relevant for all of the codebases, but
the idea is that a campaign tries to improve one particular aspect of each
codebase. An example would be fixing a particular kind of formatting.

Usually, a campaign has a default command that should be run against all
codebases. Extra arguments and environment variables can be specified on a
per-codebase basis.

A {term}`candidate` is a record that a particular campaign should run for a
particular codebase. As long as a candidate exists, the Janitor will make
sure the command gets run every now and then.

Scheduling runs for candidates happens regularly, and takes into account
several factors:

- the value of the codebase (so e.g. more popular projects can be prioritized)
- the value of the candidate (so e.g. higher-impact candidates are processed
  more often)
- previous track record for the run (if it often generates new changes
  successfully, it's prioritized)
- expected actions after a successful run: priority is given to runs that will
  be pushed or included in a merge proposal

Items in the queue sit in various predefined buckets. These allow particular
runs to be prioritized, e.g. because they're associated with an existing merge
proposal that is now conflicting. See
{doc}`How work is scheduled </scheduling>` for the details.

Workers retrieve the first item in the queue, process it and then upload the
results. This creates a {term}`run`, with a {term}`result code`, possible
artifacts (including logs), a description and some other metadata.

Runs can be successful, be a no-op (`nothing-to-do`), be a continuation of a
successful run without additional changes (`nothing-new-to-do`), or have failed
(any other result code). Some failures are classified as {term}`transient`,
which means a repeat run is expected to succeed. The scheduler takes this into
account, and runs with transient failures are ignored in various places.

(the-publisher)=

## The publisher

Once a run is approved for publishing, it's the publisher's job to either
create a merge proposal or to push the changes directly to the VCS.

The publisher can also be driven by hand: a publish that somebody requests
from the web UI skips the rate limits.

Each candidate has a {term}`publish policy` (`publish-policy` when uploading
candidates). A publish policy is a named record, set through the publisher's
`PUT /policy/{name}` API, that holds a mode for each branch of the run and a
rate-limiting bucket. A candidate without a publish policy is never published.
The modes are:

- `push`: push the changes to the target branch
- `propose`: create a merge proposal
- `attempt-push`: try to push first, but if it gets a permission denied error
  (because the Janitor does not have write access), create a merge proposal
  instead
- `push-derived`: push to a separate branch, named after the campaign's
  `branch_name`, without creating a merge proposal
- `build-only` (the default in the database schema) and `skip`: do not
  publish
- `bts`: report to a bug tracker, not supported by the publisher yet

So long as the number of open merge proposals is below the rate-limiting
threshold, new merge proposals can be created. Different algorithms for
rate-limiting are supported: {term}`slow start` (`--slowstart`), a fixed limit
(`--max-mps-per-bucket`), or none, which is the default.

The publisher will regularly scan old merge proposals for their status, and
take appropriate action:

- it triggers new runs (with priority) for merge proposals for which the
  target branch has changed and that now conflict
- it will close merge proposals for which the latest run was `nothing-to-do`
  or too trivial: generally because the target branch has incorporated the
  changes, just not via a merge
- it will close merge proposals for which the codebase is abandoned, e.g.
  because the Debian package has been removed from the archive

It also updates its own records about the merge proposals, for convenient use
in the web UI.

The publisher regularly publishes runs that have not been published before in
any way.

It prioritizes publishing of runs with a high value, as reported by the
worker.

Each cycle (every `--interval` seconds, by default 7200), it publishes by
pushing changes directly to the target branch and creating new merge
proposals. It tries hard not to produce too many changes at once or to
overwhelm the {term}`forges <forge>`:

- honor 429s (and their `Retry-After` header) that forges send
- obey the rate-limiting bucket of the candidate's publish policy
- at most `--push-limit` pushes per cycle
- at most `--modify-mp-limit` merge proposal updates per cycle

## The Git and Bazaar stores

These are simple implementations of Git and Bazaar servers, available over
HTTP and with a web UI built in. The only Janitor-specific properties they
have are:

- repositories are created automatically for codebases the Janitor has a
  record for
- writing to repositories is restricted to authenticated users, specifically
  those with worker credentials

Each also provides an endpoint for getting the diff between two arbitrary
runs.

## The differ

The (binary) differ takes the artifacts from two runs and compares them using
a particular diffing tool. It can cache the results. Currently supported are
`diffoscope` and `debdiff`.

## The worker

The worker is where the bulk of the work happens, but it's fairly simple
overall.

With `--loop`, it runs an endless loop and repeatedly:

- fetches an assignment from the runner
- downloads the specified repository
- validates the repository
- runs the codemod specified
- builds the project
- runs validation on the output
- uploads the output

The worker also provides a web UI and a REST API. The runner uses them to
health-check the worker and to query intermediate results.

## The archiver (optional)

This Debian-specific component can generate APT repositories that include the
artifacts of all successful runs.

## The dependency server (optional)

The dependency server (`dep-server`, from
[ognibuild](https://github.com/jelmer/ognibuild)) finds the packages that
provide missing build dependencies. Workers use it in generic builds when the
runner is started with `--public-dep-server-url`.

## The auto-uploader (optional)

This Debian-specific component automatically uploads the artifacts of
successful runs using `dput`.
