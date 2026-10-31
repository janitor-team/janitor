Codebases
=========

A codebase is one place in a VCS repository, usually the root of a branch.
It can have a value, e.g. the popularity of a package.

To add or update codebases, POST them as a JSON list to `/codebases` on the
runner's private port (9911 in the container image). Each codebase needs a
`name`, which candidates refer to, and can have a `url`, `branch`, `subpath`,
`vcs_type`, `web_url`, `vcs_last_revision` and `value`. The older `branch_url`
still works in place of `url`.

Candidates
==========

Once the janitor knows about a codebase, candidates can be created. A candidate
is a bit of data that a particular campaign (e.g. lintian-fixes)
can be run on a particular codebase and that there is some chance it will yield
changes.

Candidates include information like:

 * value: a relative number that explains how useful this change would be
 * success_chance: an estimate of how likely this change is to succeed and result in a build
 * context: some indicator of the current state of the world. Used to avoid retrying
     builds if nothing has really changed. e.g. for new upstream releases, this
     is the upstream version number of the latest release

POST candidates as a JSON list to `/candidates` on the same port. Each
candidate needs a `codebase` and a `campaign`, and can have `value`,
`success_chance`, `context`, `command`, `publish-policy` and `change_set`. The
runner skips candidates for a codebase or campaign it does not know, and lists
them in its response. Upload the codebases first.

The candidate list can be complex, for a better schedule, or simple, e.g. one
candidate for each codebase in a campaign with a fixed value and success_chance.

Scheduling
==========

When candidates are uploaded, the runner adds them to the queue straight
away. Their place in the queue depends on:

 * the value of the candidate
 * the value of the codebase
 * previous success rate (for the campaign/codebase combination)
 * previous run duration
 * whether the context has changed since the last run

The queue consists of prioritized buckets. Manually requested runs, runs triggered
by the publisher (e.g. to resolve merge conflicts) and retried runs are always
executed before runs that were scheduled by the scheduler.
