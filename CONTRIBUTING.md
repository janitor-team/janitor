[The Janitor](https://github.com/jelmer/janitor) sits atop a number of other
projects, and those are where most of the interesting things happen.
You may want to check out one of them.
They're probably also easier to setup and run, unlike the Janitor.

## Development Environment

Debian testing or unstable are the recommended base environments for development,
but other Linux distributions should work too.

You'll want to install various bits of software.
On a Debian-based OS, run:

```console
$ sudo apt install \
    cargo \
    gcc \
    git \
    libclang-dev \
    libpython3-dev \
    libssl-dev \
    nettle-dev \
    pkg-config \
    protobuf-compiler \
    python3-gpg
```

If you're working on Debian-targeted code, you may also want to install:

```console
$ sudo apt install devscripts
```

- - -

In addition to these packages, you will need to install a number of
Python dependencies. These can be installed from the OS package manager,
or using PIP in a Python virtual environment.

For example, on Debian-based systems:

```console
$ sudo apt install \
    python3-venv
$ git clone https://github.com/jelmer/janitor.git
$ cd janitor/
$ python3 -m venv .venv
$ . ./.venv/bin/activate
$ pip3 install --editable .[dev]
```

## Running the tests

To run the tests, use:

```console

$ make test
```

This will run both the Python and Rust tests.

## Code Style

The Janitor uses the rustfmt for Rust code and ``ruff format`` for Python code.
You can use ``make reformat`` to format all code.

## Guidelines

### Programming Languages

The Janitor was originally written in Python, but is now being converted to
Rust. New code should be written in Rust, unless there is a good reason to
write it in Python.

### Error Handling

Don't silently ignore errors. Patterns like `unwrap_or_default()`,
`.ok()`, a `filter_map` that drops errors, or logging an error and then
returning success all hide problems. Return an error to the caller instead;
panicking is not a substitute either.

Validate input where it is parsed, using types: deserialize into
`url::Url` rather than `String`, use an unsigned integer for a limit that
can't be negative, and so on. An empty string is not the same as a missing
value; don't treat `""` as `None`.

Decode PostgreSQL enums into Rust enums (deriving `sqlx::Type`) rather than
casting them to text and comparing strings.

Fix problems where they originate rather than working around them at each
call site. For example, a check that every caller needs belongs in the one
function they all go through, and an ordering problem between database
triggers belongs in the schema.

Don't log credentials, such as passwords embedded in URLs. Things that
happen routinely, like a client disconnecting halfway through a request,
should not be logged as warnings; consider a Prometheus metric instead.

### Code Coverage

All new code should be covered by tests. The Janitor uses the
[pytest](https://pytest.org) test framework for Python code and
[Rust's built-in test framework](https://doc.rust-lang.org/book/ch11-01-writing-tests.html)

Our current test coverage is a bit spotty, but we're working on improving it.

For now, please ensure that any new code you write is covered by tests - but also
make sure to manually test code that you change and ensure that it works as expected.

### Code That Does Not Belong Here

The Janitor is the basis for a number of other projects, and we try to keep it
manageable. If you're working on a feature that is not directly related to the
Janitor itself but specific to one of the other projects, please consider
whether it would be better to add it there instead.

Any code that purely deals with VCS interactions should probably
be in silver-platter or Breezy.

Interact with Breezy through breezyshim, rather than using PyO3 directly or
running `brz` as a subprocess. If breezyshim lacks something, add it there.

If you find a bug in one of the libraries the Janitor uses (Breezy,
breezyshim, silver-platter, ognibuild, buildlog-consultant, ...), please fix
or report it there rather than working around it in the Janitor. Likewise,
prefer functionality that a dependency already provides over writing your
own version of it.

The site (except for the cupboard, which will be split out in the future)
shipped with the janitor itself is meant to be an example
of how to build a frontend for the Janitor, and is not meant to be a
full-fledged site. Each deployment will probably want their own version of
it.

### Maintenance Overhead

Every bit of code that is added to the Janitor increases the maintenance
overhead, but this is especially true for code that needs to be kept up to date
with other parts of the codebase or with external dependencies.

Please consider whether the code or test you are adding is really
necessary, and how easily it can bitrot.

Please keep documentation lean. Out of date documentation is worse than
no documentation in many cases. Avoid duplication.

### On tests

Unit tests should live in the same file as the code they are testing.
Integration tests should live in the `tests/` directory.

Please share logic between tests where relevant. Don't add many separate
integration test files with their own setup, teardown and helper code.
Add tests to the existing test file for the code you are changing, and reuse
the helpers that are already there, such as `TestDatabase` and
`test_with_database!` for tests that need PostgreSQL.

Compare complete values with `assert_eq!` (or `==` in Python) rather than
checking parts of them with `contains()` or `in`.

Other anti-patterns:

 * Rely on the real database schema, don't recreate bits of it in the test
   code (e.g. creating temporary tables). This leads to brittle tests that
   break when the schema changes, or pass when they should be failing.
 * When writing files in tests, make sure they are in temporary directories
   rather than in the current directory
 * Only use mocks if you absolutely have to; in most cases there
   are better ways to test code that interacts with external systems.
 * Don't test for obvious things - like reading a text file and then
   asserting that it has the exact expected contents.
 * Be careful when checking HTML output; if you check for exact
   output that will break when e.g. new attributes are added.
   Use HTML parsers like soup to assert what's the case.
 * Don't start the Janitor's own binaries (e.g. `janitor-publish`) from
   tests; call the code directly.
 * Don't test the behaviour of libraries you use, such as HTML escaping in
   pygments.
 * Avoid tests that depend on timing or memory use; they are slow and
   flaky.
 * Keep the size of tests in proportion to the change; a one-line fix
   rarely needs a hundred lines of tests.
 * Improve overall test coverage and improve it to cover the bugs
   you're fixing. Don't just add dedicated tests to the very specific
   issue you fixed.

### Comments

If the motivation for code is unclear or if it's hard to understand,
comments can be useful. However, more useful than code that's hard to
understand with comments is code that's easy to understand.

Comments are useful, but please don't overdo them. Justify why the
code is what it is currently.

### Documentation

Documentation should describe how things are meant to work. If something
doesn't work as described, fix it or file an issue, rather than documenting
the bug.

### Keep Changes Small and Self-Contained

Try to keep changes small and self-contained. This makes it easier to review
them and to understand the impact of the change. If you're fixing two unrelated
bugs, they should be in separate PRs.

Formatting changes should be separate from functional changes. However,
please consider whether it is worth making formatting or reordering changes at all.
These changes can make it harder to understand the history of a file
(e.g. when using `git blame`). They can also make it harder to merge changes
from other branches. And they make it harder to understand the impact of a change
for reviewers.

In general, changes should be preceded by a discussion in an issue.
PRs or commits should reference the issue they are addressing.

### Making larger changes

For any feature enhancements or larger changes, please open an issue first to
discuss the change.

Large feature changes or restructuring should usually be accompanied
by a design doc in devnotes/.

## Developer Certificate of Origin

Contributions to the Janitor are accepted under the terms of the
[Developer Certificate of Origin](DCO) (DCO) version 1.1. By signing off
your commits, you certify that you have the right to submit the
contribution under the project's open source license.

To sign off a commit, add a `Signed-off-by` trailer with your real name
and email address:

```
Signed-off-by: Jane Developer <jane@example.com>
```

`git commit -s` (or `--signoff`) adds this trailer automatically using
the name and email from your git configuration. Every commit in a pull
request must be signed off.

