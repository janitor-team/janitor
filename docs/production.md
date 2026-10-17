# Running Janitor in production

There are container images for each of the Janitor services, see
[Containers](containers.md).
[Pre-built images](https://github.com/jelmer?tab=packages&repo_name=janitor)
are available, but you can also build them yourself:

```console
$ sudo apt install \
    buildah \
    make
$ make build-all
```

You need:

- PostgreSQL, see [Database](#database)
- Redis, see [Redis](#redis)
- a forge account and API token to publish, see
  [Forge credentials](#forge-credentials)
- a reverse proxy in front of the public ports, see
  [Services and ports](#services-and-ports)

For a Janitor instance, you probably want a custom website in combination
with the Janitor API. See the existing instances for inspiration:
[Debian Janitor](https://janitor.debian.net/)
([setup](https://salsa.debian.org/janitor-team/janitor.debian.net)),
[Kali Janitor](https://janitor.kali.org/) and
[Scruffy](https://www.scruffy.dev/)
([setup](https://github.com/scruffy-team/scruffy)).

## Services and ports

The ports are the ones the container images set. The private port has the
service's internal REST API. The public port, where there is one, is meant for
the internet.

| Service | Binary | Image | Private port | Public port |
| --- | --- | --- | --- | --- |
| runner | `janitor-runner` | `runner` | 9911 | 9919 (workers) |
| publisher | `janitor-publish` | `publish` | 9912 | |
| archiver | `janitor-archive` | `archive` | 9914 (also `/dists`) | |
| differ | `janitor-differ` | `differ` | 9920 | |
| Git store | `janitor-git-store` | `git_store` | 9923 | 9924 (`/git/`) |
| Bazaar store | `janitor-bzr-store` | `bzr_store` | 9929 | 9930 (`/bzr/`) |
| auto-uploader | `janitor-auto-upload` | `auto_upload` | 9933 | |
| dependency server | `dep-server` | `ognibuild_dep` | 9934 | |
| site | `janitor-site` | `site` | 8080 | 8090 (web UI) |
| worker | `janitor-worker` | `worker` | 9821 | |

`janitor-mail-filter` (image `mail_filter`) is not a service: it reads one
e-mail on standard input and exits, see [Containers](containers.md#run).

Every image listens on all interfaces, and the private ports have no
authentication. Let only the public ports through, behind a reverse proxy, and
firewall the rest. The runner expects the proxy to forward `/runner/` to its
public port, with the prefix removed.

## Database

The services share a single PostgreSQL database. Before starting anything,
load the schema:

```console
$ createdb janitor
$ psql janitor < schema/state.sql
$ psql janitor < schema/debian/debian.sql
```

`schema/debian/debian.sql` is needed when any campaign uses `debian_build`.
It needs the `debversion` PostgreSQL extension (on Debian, the
`postgresql-NN-debversion` package).

Set `database_location` in `janitor.conf`, then register the workers (with
`psql` now, or with the runner's API once the runner runs), see
[Registering workers](#registering-workers).

## Redis

The runner, the auto-uploader and the site need Redis. The publisher, the
differ and the archiver use it when it is set. Set `redis_location` in
`janitor.conf`:

```text
redis_location: "redis://localhost"
```

## Upgrading

There are no database migrations, and `schema/state.sql` cannot be loaded a
second time into an existing database. When you upgrade, compare `schema/`
between the two versions and apply the changes by hand.

The `latest` image tag moves with every push to any branch, so pin each image
to the tag of a commit id instead, e.g.
`ghcr.io/jelmer/janitor/runner:<commit id>`.

## Forge credentials

Publishing merge proposals requires a {term}`forge` API token (GitHub, GitLab,
etc.). The token is not set in `janitor.conf`: the publisher and the runner use
Breezy's own credential store, `~/.config/breezy/authentication.conf`.

For GitHub, add this section by hand:

```ini
[Github]
scheme = https
host = github.com
url = https://api.github.com
private_token = <your token>
```

`brz github-login` asks for a password and uses an API that GitHub no longer
offers, so it does not work. For GitLab, `brz gitlab-login <url> <token>`
stores the token.

## Web UI login

The web UI supports OAuth2 login, configured via `oauth2_provider` in
`janitor.conf`:

```text
oauth2_provider {
  base_url: "https://salsa.debian.org/"
  qa_reviewer_group: "janitor-reviewers"
  admin_group: "janitor-admins"
}
```

Put `client_id` and `client_secret` in `oauth2_provider`, or leave them out
and set the `OAUTH2_CLIENT_ID` and `OAUTH2_CLIENT_SECRET` environment
variables. These are used when the config fields are empty.

The provider needs a matching OAuth app or client registered with it first,
with its callback URL set to `<external_url>/oauth/callback`.

## Optional configuration

A few `janitor.conf` fields are optional:

- `git_location`/`bzr_location`: where the Git and Bazaar repositories live,
  as a URL: `file:///srv/git` for a local directory, or the private URL of a
  `git_store`/`bzr_store` instance (e.g. `http://localhost:9923`). A plain path
  does not work. Without these, that VCS type is not available at all.
- `zipkin_address`: where to send traces. If set, tracing is on at a 0.1
  sample rate. The Rust services send OTLP over HTTP to this exact URL, e.g.
  `http://localhost:9411/v1/traces`. No tracing without it.
- `user_agent`: the HTTP `User-Agent` header that `janitor-differ` sends on
  outgoing requests. Defaults to `janitor/<version>`.

## Worker images

The published worker image carries Debian's build tooling and the codemods of
`brz-debian`, such as `deb-new-upstream`. For now `deb-new-upstream` fails
there, because `brz-debian` needs the `lintian_brush` Python module and the
image does not have it, and
[breezy-debian pull request 46](https://github.com/breezy-team/breezy-debian/pull/46)
removes that need. For other codemods, such as `lintian-brush`, build your
own worker image based on the published one with the codemods your campaigns
need installed.

See {ref}`custom-worker-tooling` on the Containers page for the Dockerfile to
start from, then run your image in place of the published one.

## Registering workers

Workers authenticate to the runner with a name and password stored in the
`worker` table. Passwords are hashed with `pgcrypto`. Once the runner runs,
register a worker with its private API:

```console
$ curl -X POST -H 'Content-Type: application/json' \
    --data '{"name": "myworker", "password": "mypassword"}' \
    http://localhost:9911/admin/workers
```

or directly in the database:

```console
$ psql janitor <<'EOF'
INSERT INTO worker (name, password)
VALUES ('myworker', crypt('mypassword', gen_salt('bf')));
EOF
```

Replace `mypassword` with the password you want the worker to use.
`GET /admin/workers` lists the workers and `DELETE /admin/workers/<name>`
removes one.

The worker process needs the same credentials. Registering the worker only
lets the runner accept a login. It does not give a worker one.
`janitor-worker` looks for them in this order: `--credentials <file>` (a JSON
file with `login`/`password`), the `WORKER_NAME`/`WORKER_PASSWORD` environment
variables, or embedded directly in `--base-url`
(`http://myworker:mypassword@runner-host:9919/`).

`janitor-admin worker add myworker` would generate the password for you, but
the site's admin API does not accept its HTTP basic auth yet, see
[Command-line tools](command-line-tools.md).
