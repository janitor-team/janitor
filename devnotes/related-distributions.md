# Changes from related distributions

For https://github.com/janitor-team/janitor/issues/2079. From
https://github.com/janitor-team/janitor/issues/2079#issuecomment-6630808149:

> My thoughts were that these distributions carry their own patches
> etc, and it might be useful to try and see if we can cherry-pick
> patches and other changes from the other distributions.

## Why

A fix that one distribution has can stay missing in another, because
finding it means comparing the packaging by hand, one package at a time.
A campaign could do that search for every codebase and offer each change
as a proposal that builds.

An importer can already put a related branch's URL in the candidate's
command. But then each codemod fetches it on its own, the site cannot
show it, and nothing records in one place which revision a run used.

So the workspace would fetch the related branches, and the worker would
record each one's revision for every run, even when the codemod makes no
change and leaves no result. The run page can then show which revision
of the other distribution a run compared against, and queries across
runs can use it without knowing the codemod. That is what storing
remotes per run is for.

## Design

- A codebase gets a list of related branches (name, URL, branch),
  uploaded with `POST /codebases` and kept in a `codebase_remote` table.
  The name `origin` is reserved.
- A campaign asks for them with a new config option. Its runs get the
  remotes in the assignment, and the workspace fetches each one before
  the codemod runs.
- The worker reports each remote's URL, branch and fetched revision in
  `Metadata.remotes`. The runner stores them in a `run_remote` table
  with the run.
- The codemod lives outside this repository. It compares the codebase
  with each remote and applies the changes it picks, one commit each,
  keeping the original author.

The importer decides which branches are related, and the codemod decides
which changes to pick. The importer also puts the remotes' heads in the
candidate `context`, so a new head in a remote can lead to a new run.

## Open questions

1. Does a list per codebase plus a record per run match what you had in
   mind?
2. Should the fetch be an option of silver-platter's `WorkspaceBuilder`,
   so `svp run` gets it too, or should the worker do it?
3. How should the codemod learn about the fetched remotes: a new `SVP_`
   variable, or the assignment's `env`?
4. For bzr, should a remote be a colocated branch in the workspace, or
   should this be git only to start with?
5. One merge proposal per codebase with one commit per change, or one
   per change?
6. Where should the codemod live, for example in debian-codemods?
