This repository contains the setup for a "[Janitor](https://jelmer.uk/code/janitor)" bot. This is basically
a platform for managing large-scale automated code improvements on
top of [silver-platter](https://github.com/jelmer/silver-platter).

Any code that is not related to the platform but to actually making changes
should probably live in either ``silver-platter``, ``breezy`` or a
specific codemod (such as [lintian-brush](https://salsa.debian.org/jelmer/lintian-brush)).

There are currently several instances of the Janitor running. For their configuration, see:

* [Debian Janitor](https://janitor.debian.net/) - Setup at https://salsa.debian.org/janitor-team/janitor.debian.net
* [Kali Janitor](https://janitor.kali.org/) - Configuration repository is private
* [Upstream Janitor aka ``Scruffy``](https://www.scruffy.dev/) - Setup at https://github.com/scruffy-team/scruffy

Philosophy
==========

There are some straightforward changes to code that can be made
using scripting. The janitor's job is to opportunistically make those changes
when it is certain it can do so with a high confidence, and to back off
otherwise.

The janitor continuously tries to run changes on the set of repositories it
knows about. It tries to be clever about scheduling those operations that
are more likely to yield results and be published (i.e. merged or pushed).

Design
======

The janitor is made up out of multiple components. The janitor is built on top of
[silver-platter](https://github.com/jelmer/silver-platter) and relies
on that project for most of the grunt work.

Several permanently running jobs:

* the [*publisher*](devnotes/overview.md#the-publisher) proposes or pushes changes that have been successfully
  created and built previously
* the [*vcs store*](devnotes/overview.md#the-gitbzr-store) manages and stores VCS repositories (git, bzr) and serves their diffs [optional]
* the [ognibuild](https://github.com/jelmer/ognibuild) dep server is used to
  resolve missing dependencies [optional]
* the [*runner*](devnotes/overview.md#the-runner) processes the queue, kicks off workers for
  each package and stores the results.
* one or more [*workers*](devnotes/overview.md#the-worker) which are responsible for actually generating and
  building changes.
* an [*archiver*](devnotes/overview.md#the-archiver-optional) that takes care of managing the apt archives and publishes them [optional]
* a *site* job that renders the web site
* the [*differ*](devnotes/overview.md#the-differ) takes care of running e.g. debdiff or diffoscope between binary runs
* the [*auto-uploader*](devnotes/overview.md#the-auto-uploader-optional) signs and uploads the packages built by successful runs [optional]

An optional *mail filter* can be run by the mail server for each e-mail from a
forge, so the merge proposal it mentions is refreshed straight away (see
[sieve/README](sieve/README)).

Each instance of the janitor should somehow upload "codebase" entries and
"candidates", which describe where to find code and what to do with it.
See [docs/quickstart.md](docs/quickstart.md) for an example.

There are no requirements that these jobs run on the same machine, but they are
expected to have secure network access to each other.

Every job runs an HTTP server to allow API requests and use of /metrics, for
prometheus monitoring.

Workers are fairly naive; they simply run a ``silver-platter`` subcommand
to create branches and they build the resulting branches. The runner
then fetches the results from each run and (if the run was successful)
uploads the .debs and optionally proposes a change.

The publisher is responsible for enforcing rate limiting, i.e. making sure
that there are no more than X pull requests open per maintainer.

See the various files in ``devnotes/`` for details.

Worker
======
The actual changes are made by various codemod scripts that implement
the [silver-platter protocol](https://github.com/jelmer/silver-platter/blob/master/codemod-protocol.md).

Installation
============

There are two common ways of deploying a new janitor instance.

 * On top of kubernetes (see the configuration for the Debian & Upstream janitor)
 * Using e.g. ansible

See [docs/production.md](docs/production.md) for running an instance.

Containers
----------

Several container images are provided (see [docs/containers.md](docs/containers.md))

 * [ghcr.io/jelmer/janitor/archive](https://github.com/users/jelmer/packages/container/package/janitor%2Farchive) - APT archive generator
 * [ghcr.io/jelmer/janitor/differ](https://github.com/users/jelmer/packages/container/package/janitor%2Fdiffer) - diffoscope/debdiff generator
 * [ghcr.io/jelmer/janitor/publish](https://github.com/users/jelmer/packages/container/package/janitor%2Fpublish) - VCS publisher
 * [ghcr.io/jelmer/janitor/runner](https://github.com/users/jelmer/packages/container/package/janitor%2Frunner) - Queue management & Run handling
 * [ghcr.io/jelmer/janitor/site](https://github.com/users/jelmer/packages/container/package/janitor%2Fsite) - Example web site & public API
 * [ghcr.io/jelmer/janitor/git_store](https://github.com/users/jelmer/packages/container/package/janitor%2Fgit_store) - storage for Git
 * [ghcr.io/jelmer/janitor/bzr_store](https://github.com/users/jelmer/packages/container/package/janitor%2Fbzr_store) - storage for Bazaar
 * [ghcr.io/jelmer/janitor/worker](https://github.com/users/jelmer/packages/container/package/janitor%2Fworker) - Base for workers
 * [ghcr.io/jelmer/janitor/auto_upload](https://github.com/users/jelmer/packages/container/package/janitor%2Fauto_upload) - Package uploader
 * [ghcr.io/jelmer/janitor/mail_filter](https://github.com/users/jelmer/packages/container/package/janitor%2Fmail_filter) - Forge e-mail filter
 * [ghcr.io/jelmer/janitor/ognibuild_dep](https://github.com/users/jelmer/packages/container/package/janitor%2Fognibuild_dep) - ognibuild dep server

Contributing
============

See [CONTRIBUTING.md](CONTRIBUTING.md) for instructions on e.g. setting up
a development environment.

If you're interested in working on adding another campaign for a janitor
instance, see [Adding a new campaign](devnotes/adding-a-new-campaign.md).

The Matrix channel for the Janitor project can be found at
[#janitor:matrix.org](https://matrix.to/#/#janitor:matrix.org).
