# Branch names

This is a design note: the current layout first, then the plan.

## Current layout

For each run, the worker pushes these refs to the Git store (`$uuid` is the
run id):

- `refs/tags/run/$uuid/$function`: the result of each branch of the run
- `refs/tags/$uuid/$tag`: new or updated upstream tags

Once the build has succeeded, it also pushes:

- `refs/heads/$campaign/$function`: the same commit as the run's tag (a plain
  ref, not a symref)
- `refs/tags/$tag`: the upstream tags

## Goal

For runs:

- `runs/$uuid/$function` tags
- `runs/$uuid/tags/$tag` tags for new or updated upstream tags

There should be a symref at `refs/$campaign/$function` that points at that
tag and is updated after each new run.

Also, track this information in the database.

For related repositories, create remotes:

- `remotes/origin/` for Debian packaging
- `remotes/upstream/` for upstream

Names for functions:

- `main` for the main branch (packaging or otherwise)
- `upstream` for the upstream import branch for Debian packages
- `pristine-tar` for the `pristine-tar` branch

## Roadmap

- update the publisher to use the new tag names
- send requests to the publisher to mirror the `origin` and `upstream`
  repositories. To start with, send just:
  - name of remote (`origin`, `upstream`)
  - URL of remote
- push symrefs (`refs/$campaign/$function` => `runs/$uuid/$function`)
  - needs to be done by the publisher
