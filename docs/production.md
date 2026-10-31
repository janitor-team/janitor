# Running Janitor in production

There are [containers](Dockerfiles_.md) available for each of the Janitor services.

[pre-built containers](https://github.com/jelmer?tab=packages&repo_name=janitor) are
available, but you can also create them yourself:

```console
$ sudo apt install \
    buildah \
    make
$ make build-all
```

## Database

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

## Forge credentials

Publishing merge proposals requires a forge API token (GitHub/GitLab/etc),
but this is not part of janitor.conf - publish and runner delegate to
breezy's own credential store instead, at
`~/.config/breezy/authentication.conf`.

For GitHub, run:

```console
$ brz github-login
```

which prompts for a username and token and writes the `[Github]` section
itself. To do it by hand instead, the section breezy expects is:

```
[Github]
scheme = https
host = github.com
url = https://api.github.com
private_token = <your token>
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

For a Janitor instance, you probably want a custom website in combination with
the Janitor API. See the existing instances for inspiration.

## Worker images

The published worker image carries Debian's build tooling, but it carries no
codemods. Build your own worker image based on the published one with the
codemods your campaigns need installed.

See "Custom worker tooling" in [Dockerfiles_.md](Dockerfiles_.md) for the
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
table. There are two ways to register one.

One way is `janitor-admin`, which makes up the password for you:

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

You can also register a worker through the runner's private API (port 9911 in
the container image), with `worker.json` holding
`{"name": "myworker", "password": "<the worker's password>"}`:

```console
$ curl -X POST -H 'Content-Type: application/json' \
    --data @worker.json http://localhost:9911/admin/workers
```
