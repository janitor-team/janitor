# Overview

The janitor can manage automated changes to a set of
projects in version control.

It does this through a number of interconnected
services, each of which provides some basic features:

 * a rest service
 * /metrics target for use with prometheus
 * /health and /ready targets for use with kubernetes or other
   health-checking services

Access to the rest services is unauthenticated and is meant to be restricted
to the other services and administrators.
 
Some services also provide a separate rest service on a different port,
for exposure to the internet. In particular:

 * janitor-runner: the worker API (at /runner behind a proxy)
 * janitor-git-store: /git
 * janitor-bzr-store: /bzr

janitor-archive serves /dists on its only port.

## The runner

A "codebase" is a place in a VCS repository that changes can be made to.
Generally, it will refer to the root of a branch in a repository
(either Git or Bazaar), but it can also refer to a subpath.
Codebases can also be "nascent", which means that they don't exist
anywhere yet, but are created and populated by the janitor.

"campaigns" are efforts to fix a particular issue across the
codebases. Campaigns do not have to be relevant for all
of the codebases, but the idea is that a campaign tries to
improve one particular aspect of each codebase. An example
would be fixing a particular kind of formatting. 

Usually, a campaign has a default command that should be run against
all codebases. Extra arguments and environment variables can be
specified on a per-codebase basis.

A "candidate" is a record that a particular campaign should run for
a particular codebase. As long as a candidate exists,
the janitor will make sure the command gets run every now and then.

Scheduling runs for candidates happens regularly, and takes into account
several factors:

 * the score for the codebase
   (so e.g. more popular projects can be prioritized)
 * the score for the candidate
   (so e.g. more impacting candidates are processed more frequently)
 * previous track record for the run
   (if it often generates new changes successfully, it's prioritized)
 * expected actions after a successful run - priority is
   given to runs that will be pushed or included in a merge proposal

Items in the queue sit in various predefined buckets. These allow
particular runs to be prioritized, e.g. because they're associated
with an existing merge proposal that is now conflicting.

Workers retrieve the first item in the queue, process it and then
upload the results. This creates a "run", with a result code,
possible artifacts (including logs), a description and some other
metadata.

Runs can be successful, be a no-op ("nothing-to-do") a continuation of
a successful run without additional changes ("nothing-new-to-do")
or have failed (any other result code). Some failures are classified
as transient, which means that the expectation is that a repeat
run will succeed. The scheduler takes this into account, and
runs with transient failures are ignored in various places.

## The publisher

Once a run is approved for publishing, it's the publisher's job to
either create a merge proposal or to push the changes directly to
the VCS.

The publisher can be manually operated, e.g. if somebody
requests a run from the web UI that is always executed.

Each candidate has a "publish_policy" associated with it, which
describes the mode to use ("push", "propose", "attempt-push") and
a rate limiting bucket. So long as the number of
open merge proposals is below the rate-limiting threshold,
new merge proposals can be created. Different algorithms
for rate-limiting are supported, but the default is slow-start.

The "attempt-push" mode will try to push first, but if it
gets a permission denied error (because the janitor does
not have write access) it will create a merge proposal instead.

The publisher will regularly scan old merge proposals for their
status, and take appropriate action:

 * it triggers new runs (with priority) for merge proposals for
   which the target branch has changed and that now conflict
 * it will close merge proposals for which the latest run
   was nothing-to-do or too trivial - generally because the target
   has incorporated our changes, just not via a merge
 * it will close merge proposals for which
   the codebase is abandoned, e.g. because the debian
   package has been removed from the archive

It also updates its own records about the merge proposals, for
convenient use in the web UI.

The publisher regularly publishes runs that have not been published
before in any way.

It prioritizes publishing of runs with a high value, as reported by the
worker.

Each cycle, it publishes by pushing changes directly to the target and
creating new merge proposals. It tries hard not to produce too many changes
at once or overwhelm:

 * Honor 429s (and their Retry-After header) that forges sent
 * Obey the per-candidate rate-limiting-bucket
 * At most X pushes (configurable)
 * At most Y new merge proposals (configurable)

## The Git/Bzr Store

These are simple implementations of a Git and Bazaar servers,
available over HTTP and with a web UI built in. The only Janitor-specific
properties they have are:

 * repositories are created automatically for codebases the janitor has a record for
 * writing to repositories is restricted to authenticated users, specifically
   those with worker credentials

Each also provide an endpoint for getting the diff between two arbitrary runs.

## The Differ

The (binary) differ takes the artifacts from two runs and compares them using a particular
diffing tool. It can cache the results. Currently supported are diffoscope and
debdiff.

## The Worker

The worker is where the bulk of the work happens, but it's fairly simple overall.

With `--loop`, it repeats these steps:

 * fetches an assignment from the runner
 * downloads the specified repository
 * validates the repository
 * runs the codemod specified
 * builds the projects
 * runs validation on the output
 * uploads the output

The worker provides a web UI and rest API as well, which are used by
the runner to health-check it and query for intermediate results.

## The Archiver (optional)

This Debian specific component can generate APT repositories that
include the artifacts of all successful runs.

## The Auto-Uploader (optional)

This Debian specific component automatically uploads
the artifacts of successful runs using dput.
