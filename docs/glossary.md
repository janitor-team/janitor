# Glossary

- *archiver*: The job that builds and serves the signed apt repository
indexes for the Debian packages of runs. The site serves the package files
themselves.

- *auto-upload*: The job that signs the `.changes` files of successful Debian
builds with `debsign` and uploads them to an archive with `dput`.

- *campaign*: A specific kind of change to attempt across codebases, such
as `lintian-fixes`. Defined by a `campaign { ... }` block in
`janitor.conf`.

- *candidate*: A codebase paired with a campaign that might yield a
change, along with an estimated value and success chance.

- *codebase*: A collection of source code files that are managed together in a
version control system. Usually this will be the root of a specific branch in a
vcs repository. Sometimes, it will be a subdirectory in a VCS. It can also be
e.g. a tarball somewhere.

- *cotenants*: Other codebases that share the same branch as the current codebase.

- *cupboard*: The web site's internal status and administrative pages:
the queue, run history, workers, review, and related pages.

- *differ*: The job that uses `debdiff` and `diffoscope` to compare the
Debian packages of a run with those of its control run: a build of the same
codebase without the change.

- *merge proposal*: A pull or merge request the publisher opens once a
change has built successfully.

- *publisher*: The job that pushes the changes of successful runs, or opens
merge proposals for them, within the rate limits. It also keeps the status of
those merge proposals up to date.

- *run*: One attempt at processing a candidate, from cloning the
codebase through building the result.

- *runner*: The permanently running job that processes the queue and
hands work to workers.

- *site*: The job that serves the web site, including the cupboard, and the
[API](api.md).

- *target*: The kind of build a campaign has, `debian` or `generic`, set by
whichever of `debian_build` or `generic_build` it has. It decides how the
worker builds the result.

- *VCS store*: The job, `git-store` or `bzr-store`, that holds the branches
workers push the results of runs to.

- *worker*: A job that generates and builds a change. Unlike the
runner, it doesn't have to be permanently running.
