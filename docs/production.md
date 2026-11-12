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

The tarball gets an apt source for the distribution and one for each suite in
its `extra` list. If the distribution sets `signed_by`, these sources use that
keyring, and the script copies the keyring into the tarball at the same path.

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
