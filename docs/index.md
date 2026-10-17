# What is Janitor?

The Janitor makes the same kind of change across many repositories. It runs a
script (a {term}`codemod`) on each repository, builds the result, and then
pushes the change or opens a merge proposal, when it can do so with high
confidence. Otherwise it backs off. It is for teams that want one fix made in
many places. Support for Debian packages is built in but optional. Other code
uses `generic_build`.

It is built on top of
[silver-platter](https://github.com/jelmer/silver-platter), the library and
command-line tool that applies one change to one repository.

See it running: [Debian Janitor](https://janitor.debian.net/),
[Kali Janitor](https://janitor.kali.org/) and
[Scruffy](https://www.scruffy.dev/).

New here? Start with {doc}`How Janitor works <devnotes/overview>` and the
{doc}`glossary`.

The source lives at
[https://github.com/janitor-team/janitor](https://github.com/janitor-team/janitor).
For help, ask in [#janitor:matrix.org](https://matrix.to/#/#janitor:matrix.org).
To set up a development environment, see
[CONTRIBUTING.md](https://github.com/janitor-team/janitor/blob/main/CONTRIBUTING.md).

% Pages in reading order. The glob adds the last page and any new one.

```{toctree}
:caption: Using and running
:maxdepth: 1
:glob:

glossary
scheduling
production
containers
*
```

```{toctree}
:caption: Developer notes
:maxdepth: 1
:glob:

devnotes/overview
devnotes/adding-a-new-campaign
devnotes/*
```
