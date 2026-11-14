# Running Janitor in production

There are [containers](containers.md) available for each of the Janitor services.

[pre-built containers](https://github.com/jelmer?tab=packages&repo_name=janitor) are
available, but you can also create them yourself:

```console
$ sudo apt install \
    buildah \
    make
$ make build-all
```

## PostgreSQL

The services share a single PostgreSQL database. Before starting anything,
load the schema:

```console
$ createdb janitor
$ cat schema/state.sql | psql janitor
$ cat schema/debian/debian.sql | psql janitor  # only if using the Debian-specific features
```

Workers log in to the runner with HTTP Basic Auth. See
[Registering workers](#registering-workers).

Registering a worker only tells the runner which login to accept. The worker
needs the same name and password. `janitor-worker` looks for them in this order:
`--credentials <file>` (a JSON file with `login`/`password`), then the
`WORKER_NAME`/`WORKER_PASSWORD` environment variables. Passwords embedded in
`--base-url` are rejected, since the URL ends up in logs.

## Redis

Janitor needs a running Redis server. Set `redis_location` in
`janitor.conf`:

```
redis_location: "redis://localhost"
```

## Forge credentials

Publishing merge proposals requires a forge API token (GitHub/GitLab/etc),
which publish and runner read from breezy's own credential store. Log in to
each forge the janitor publishes to:

```console
$ brz github-login
$ brz gitlab-login https://salsa.debian.org/
$ brz gitea-login https://gitea.example.com/
$ brz launchpad-login janitor-bot
```

## Web UI login

The web UI supports OAuth2 login, configured via `oauth2_provider` in
`janitor.conf`:

```
oauth2_provider {
  base_url: "https://salsa.debian.org/"
  qa_reviewer_group: "janitor-reviewers"
  admin_group: "janitor-admins"
}
```

Any account at the provider can log in. `qa_reviewer_group` and
`admin_group` can be used to restrict some permissions to a select group
at the provider.

- Every logged-in user can reschedule and review runs, ask for a codebase
  to be published and have the status of merge proposals read again from
  the forge.
- Only reviews by members of `qa_reviewer_group` approve or reject a run
  for publishing.
- Members of `admin_group` can also manage workers, runs, merge proposals
  and publishing.

`client_id`/`client_secret` can go directly in `oauth2_provider`, or be left
out and set via the `OAUTH2_CLIENT_ID`/`OAUTH2_CLIENT_SECRET` environment
variables instead, which are used as a fallback if the config fields are empty.

The provider needs a matching OAuth App/client registered with it first,
with its callback URL set to `<external_url>/oauth/callback`.

## Optional configuration

A few `janitor.conf` fields are optional, each with a clean fallback if left
unset:

- `git_location`/`bzr_location` - where `git_store`/`bzr_store` repos live
  (a local path, or a remote URL pointing at a `git_store`/`bzr_store`
  instance). Without these, that VCS type is not available at all.
- `zipkin_address` - a Zipkin endpoint; if set, enables distributed tracing
  at a 0.1 sample rate. No tracing without it.
- `user_agent` - the HTTP User-Agent sent on outbound requests. Falls back to
  `janitor/<version>` if unset.
- `hide_reviewer_identities` - if `true`, visitors who are not logged in
  see a login link instead of each reviewer's e-mail address. If unset,
  everyone sees the addresses.

For a Janitor instance, you probably want a custom website in combination with
the Janitor API. See the existing instances for inspiration.

## Worker images

The published worker image carries Debian's build tooling, but it carries no
codemods. Build your own worker image based on the published one with the
codemods your campaigns need installed.

See [Custom worker tooling](containers.md#custom-worker-tooling) for the
Dockerfile to start from, then run your image in place of the published one.

## sbuild chroots

Workers build Debian packages with `sbuild` in unshare mode. For each build,
`sbuild` unpacks a tarball from `~/.cache/sbuild` named after the campaign's
`build_distribution`, such as `lintian-fixes-amd64-sbuild.tar.xz`.

`create-sbuild-chroot-unshare.py` creates the tarball for a distribution
defined in `janitor.conf` with `mmdebstrap`, and links it under the
`build_distribution` of each campaign based on that distribution. Run it on
the host, from a checkout set up as in [CONTRIBUTING.md](../CONTRIBUTING.md):

```console
$ sudo apt install mmdebstrap dpkg-dev
$ ./create-sbuild-chroot-unshare.py --config janitor.conf unstable
```

Without a distribution name it does every distribution in `janitor.conf`. The
worker container looks in `/root/.cache/sbuild`, so mount the directory there
with `--volume ~/.cache/sbuild:/root/.cache/sbuild`.

Run the script again after adding a campaign, and at least once a week:
`sbuild` stops using a tarball older than `$unshare_mmdebstrap_max_age` (one
week by default).

## Registering workers

Workers log in to the runner with a name and password stored in the `worker`
table. Register one with `janitor-admin`, which makes up the password for you:

```console
$ janitor-admin --url https://your-janitor-instance/ worker add myworker
Created worker myworker
Password: <generated password>
(Store this password now; it is not recoverable.)
```

This calls the admin-only `POST /cupboard/api/workers` API on the site, which
generates a random password, stores its hash, and returns the plaintext password once
in the response. It isn't kept anywhere in recoverable form, so copy it down when it's
printed.

## Adding a new campaign

[Define a campaign](quickstart.md#define-a-campaign) in the quickstart shows
the `campaign` block in `janitor.conf`. This section shows how to write
the codemod script and the merge proposal text, and how to find
candidates.

### Write the codemod script

Every campaign has a codemod script that makes changes to a version
controlled branch. It runs in a checkout of the codebase and can change
whatever it needs to. See
[this blog post](https://www.jelmer.uk/silver-platter-intro.html) for more
information about creating codemod scripts.

You can test the script on its own with silver-platter. In a checkout of a
Debian package, run:

```console
$ debian-svp apply --diff /path/to/myscript
```

This commits the change to that checkout, so use a copy you can throw away.

### Write the merge proposal text

The description of a merge proposal comes from a template named after the
campaign: `lintian-fixes.md` for forges that take Markdown,
`lintian-fixes.txt` for the others. Put the templates in a directory the
publisher can read, and pass it to `janitor-publish` with
`--template-env-path`. Without a template, the description is
`Changes for lintian-fixes (<run id>)`.

Templates use the [MiniJinja syntax](https://docs.rs/minijinja/latest/minijinja/syntax/index.html).
A line that holds only a block tag, such as `{% endfor %}`, leaves no blank
line in the output. A description template can use:

- `campaign`: the campaign name.
- `log_id`: the run id.
- `role`: the branch role, such as `main`.
- `codemod`: the `context` that the codemod script writes to `$SVP_RESULT`.
  Each of its keys is also a variable of its own, such as `applied` for
  lintian-brush.
- `debdiff`: for Debian campaigns, the debdiff between the package built by
  the run and the one built without the change, when there is one.
- `external_url`: the URL given to `janitor-publish` with `--external-url`,
  without a trailing slash.

It can also call these functions:

- `debdiff_is_empty(debdiff)`: whether the debdiff shows no changes.
- `markdownify_debdiff(debdiff)`: the debdiff as Markdown.
- `parseaddr("Name <email>")`: splits an address into `[name, email]`.

For example, a `lintian-fixes.md`:

```
Fix some issues reported by lintian

{% for fix in applied %}
* {{ fix.summary }}
{% endfor %}
```

The proposal's title and the commit message it suggests for the merge can
come from `title` and `commit_message` in a `merge_proposal` block of the
campaign. They are templates too:

```
campaign {
  name: "lintian-fixes"
  ...
  merge_proposal {
    title: "Fix {{ applied|length }} lintian issues"
  }
}
```

They can use the keys of the codemod script's `context`, `external_url`
and the functions above. Without a `title`, the title is the first line of
the `.txt` description, up to its first full stop. Where the forge supports
labels, the proposal gets the campaign name as its label.

### Add a script for finding candidates

Write a script that finds candidates for the new campaign and uploads them
to the runner, as the
[quickstart](quickstart.md#add-a-codebase-a-publish-policy-and-a-candidate)
does by hand, and run it regularly. [Candidates](scheduling.md#candidates)
lists the fields a candidate can set.
