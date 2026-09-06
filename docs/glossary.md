* *campaign*: A specific kind of change to attempt across codebases, such
as `lintian-fixes`. Defined by a `campaign { ... }` block in
`janitor.conf`.

* *candidate*: A codebase paired with a campaign that might yield a
change, along with an estimated value and success chance.

* *codebase*: A collection of source code files that are managed together in a
version control system. Usually this will be the root of a specific branch in a
vcs repository. Sometimes, it will be a subdirectory in a VCS. It can also be
e.g. a tarball somewhere.

* *cotenants*: Other codebases that share the same branch as the current codebase.

* *cupboard*: The web site's internal status and administrative pages:
the queue, run history, workers, review, and related pages.

* *merge proposal*: A pull or merge request the publisher opens once a
change has built successfully.

* *run*: One attempt at processing a candidate, from cloning the
codebase through building the result.

* *runner*: The permanently running job that processes the queue and
hands work to workers.

* *worker*: A job that generates and builds a change. Unlike the
runner, it doesn't have to be permanently running.
