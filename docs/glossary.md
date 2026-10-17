# Glossary

```{glossary}
archiver
  The optional, Debian-specific service (`janitor-archive`) that builds APT
  repositories from the artifacts of successful runs.

build target
  The kind of build a campaign does, `debian` or `generic`. It is set by which
  of `debian_build` or `generic_build` the campaign has, and it decides how the
  worker builds the result.

campaign
  A specific kind of change to attempt across codebases, such as
  `lintian-fixes`. Defined by a `campaign { ... }` block in `janitor.conf`.

candidate
  A codebase paired with a campaign that might yield a change, along with an
  estimated value and success chance.

codebase
  A collection of source code files that are managed together in a version
  control system. Usually this will be the root of a specific branch in a VCS
  repository. Sometimes, it will be a subdirectory in a VCS. It can also be,
  e.g., a tarball somewhere.

codemod
  A script that makes one kind of change in a checkout, such as
  `lintian-brush`. A campaign runs one as its `command`.

context
  Something a candidate's script reports about the current state of the world,
  e.g. the latest upstream version. The scheduler gives a run a lower place in
  the queue when the context has not changed since the last run.

cotenants
  Other codebases that share the same branch as the current codebase. The
  publisher adds the codebase name to the branch name it pushes when there are
  cotenants.

cupboard
  The website's internal status and administrative pages: the queue, run
  history, workers, review, and related pages.

dependency server
  The optional service (`dep-server`, image `ognibuild_dep`) that finds the
  packages for missing build dependencies. Workers use it in generic builds
  when the runner is started with `--public-dep-server-url`.

differ
  The service (`janitor-differ`) that compares the artifacts of two runs with
  `diffoscope` or `debdiff`.

forge
  A code hosting site with an API, such as GitHub or GitLab, where the
  publisher pushes changes and opens merge proposals.

merge proposal
  A pull or merge request the publisher opens once a change has built
  successfully.

nascent
  A codebase that does not exist anywhere yet. The Janitor creates and
  populates it.

publish mode
  What the publisher does with one branch of a run: `push`, `propose`,
  `attempt-push`, `push-derived`, `build-only`, `skip` or `bts` (not
  supported by the publisher yet).

publish policy
  A named record that sets the publish mode for each branch of a run, and a
  rate-limiting bucket. Candidates refer to one by name.

publisher
  The service (`janitor-publish`) that pushes changes or opens merge proposals
  for successful runs.

queue bucket
  A priority class in the queue. Buckets are served in a fixed order, e.g.
  `update-existing-mp` and `manual` before `default`.

result code
  The short code a run ends with, such as `success`, `nothing-to-do` or a
  failure code.

run
  One attempt at processing a candidate, from cloning the codebase through
  building the result.

runner
  The permanently running service (`janitor-runner`) that processes the queue
  and hands work to workers.

silver-platter
  The library and command-line tool (`svp`, `debian-svp`) that applies one
  change to one repository and publishes it. The Janitor is built on top of
  it.

site
  The service (`janitor-site`) that serves the web UI and the public API.

slow start
  A rate limit for the publisher (`--slowstart`): a rate-limiting bucket may
  have one more open merge proposal than it has had merged or applied.

store
  The Git store (`janitor-git-store`) and the Bazaar store
  (`janitor-bzr-store`) host the repositories with the results of runs.

suite
  The old name for a campaign. The database still calls it `suite`.

transient
  A failure that a repeat run is expected not to hit. The scheduler ignores
  transient failures when it estimates the chance of success.

value
  A relative number that says how useful a change would be. Candidates and
  codebases both have one. Higher values get a better place in the queue.

worker
  A service that generates and builds a change. Unlike the runner, it doesn't
  have to be permanently running.
```
