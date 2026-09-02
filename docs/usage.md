# Using a Janitor instance

Janitor is a platform that runs automated code changes ("campaigns")
across a set of source repositories and tracks the results on a web
site. This page orients four different kinds of visitor toward the parts
of the project relevant to them. It does not repeat what is documented
elsewhere; it points at the real pages and files.

There are several running instances, listed in the top-level
[README.md](../README.md): the [Debian Janitor](https://janitor.debian.net/),
the [Kali Janitor](https://janitor.kali.org/), and
[Scruffy](https://www.scruffy.dev/) for upstream projects. Everything
below applies to any of them, and to a self-hosted instance.

## Browsing as a visitor

No account is required to look around. From an instance's home page
(`/`), each campaign gets its own page (`/{campaign}/`) listing the kind
of change it makes and linking to its candidates
(`/{campaign}/candidates`) - the repositories a campaign might still run
against. A specific repository's page (`/{campaign}/c/{codebase}/`) shows
its run history for that campaign.

`/cupboard/` is the instance's internal status area: the processing
queue, run history, worker list, result-code breakdowns, publish
history, and the merge-proposal review queue. All of it is readable
without logging in; see [roles.md](roles.md) for exactly what changes
once you do log in.

[glossary.md](glossary.md) defines the recurring terms (codebase,
campaign, candidate, run, and so on) used across this page and the rest
of the docs.

## Checking on your own package as a maintainer

If a Janitor instance already covers a package or repository you
maintain, its per-codebase page (`/{campaign}/c/{codebase}/`) is the
place to check what the instance has tried, and its most recent run for
a codebase is linked from there. From a run's own page you can see the
diff, the build log, and - if the change is ready to go out - a "Publish
now" button; see [roles.md](roles.md) for who else can act on a run and
under what conditions.

Runs also link back to any merge proposal the janitor opened, so if a
change was proposed against your repository, following it from the run
page is the most direct route back to it.

If a codebase you maintain is not covered yet, or you think a campaign
should behave differently against it, that is a question for the
specific instance's own configuration and operators (see below), not
something this repository controls per-package.

## Running your own instance for a distro or OS

Setting up an instance means deploying the janitor's own services -
publisher, runner, one or more workers, archiver, differ, VCS store(s),
and the site - against your own set of repositories and campaigns.
Start with:

* [production.md](production.md) for deployment: prebuilt containers,
  building them yourself, and general shape of an instance.
* [Dockerfiles_.md](Dockerfiles_.md) for what each service's container
  image provides.
* [structure.md](structure.md) for the top-level scripts and where admin
  tooling lives versus what an ordinary user can run.
* [flow.md](flow.md) for how package metadata and candidates get into
  the system in the first place, and how scheduling works from there.
* [roles.md](roles.md) for the `oauth2_provider` config in
  `janitor.conf.example` that controls who gets reviewer or admin access
  on your instance's site once it's running.
* `devnotes/adding-a-new-campaign.rst` for writing the codemod script
  and campaign definition behind a new kind of change.
* `devnotes/overview.rst` for how the services talk to each other and
  which of their APIs are meant to be public versus internal-only.

The [README.md](../README.md) Design section has a short description of
each permanently running job and how they fit together before you dive
into any one of them.

## Administering an existing instance

If you already operate an instance, day-to-day admin actions live in
`/cupboard/` and require being in the `admin_group` configured in your
`oauth2_provider` block: killing a stuck worker run, reprocessing stored
build logs, mass-rescheduling runs matching a result code, forcing an
autopublish pass or a merge-proposal status rescan, and setting a merge
proposal's terminal status by hand. [roles.md](roles.md) lists each of
these with the exact route it calls, including two cases where the
admin-only control in the UI is not backed by an equivalent server-side
check.

The `qa_reviewer_group` in the same config block controls whose review
verdicts on `/cupboard/review` actually change a run's publish status,
as opposed to being recorded without effect; also covered in
[roles.md](roles.md).

For redeploying, upgrading, or reconfiguring the services themselves,
[production.md](production.md) and [Dockerfiles_.md](Dockerfiles_.md)
remain the reference; [CONTRIBUTING.md](../CONTRIBUTING.md) covers
setting up a development checkout if you need to patch the janitor
itself rather than just its configuration.
