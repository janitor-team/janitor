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

Workers authenticate against the runner with HTTP Basic Auth, checked against
the `worker` table. Passwords are hashed with pgcrypto:

```console
$ psql janitor -c "INSERT INTO worker (name, password) VALUES ('myworker', crypt('mypassword', gen_salt('bf')))"
```

The worker process itself needs those same credentials supplied to it -
inserting the row above only lets the runner *accept* a login, it does not
give any worker one. `janitor-worker` looks for them in this order:
`--credentials <file>` (a JSON file with `login`/`password`), the
`WORKER_NAME`/`WORKER_PASSWORD` environment variables, or embedded directly
in `--base-url` (`http://myworker:mypassword@runner-host/`).

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
- `user_agent` - the HTTP User-Agent `differ`/`publish` send on outbound
  requests. Falls back to a library default if unset.

For a Janitor instance, you probably want a custom website in combination with
the Janitor API. See the existing instances for inspiration.

Once an instance is running, see [roles.md](roles.md) for what its
`oauth2_provider.qa_reviewer_group` and `admin_group` config actually
grant on the web UI, and [usage.md](usage.md) for a guide to using an
instance by audience - visitor, package maintainer, distro/OS
maintainer, or admin.

## Registering workers

Workers authenticate to the site with a name and password stored in the `worker`
table. There are two ways to register one.

The `janitor-admin` CLI is the easiest and safest option, since it generates the
password for you rather than asking you to invent and hash one yourself:

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

If you don't have `janitor-admin` set up, you can register a worker directly in the
database instead:

```console
$ psql janitor -c "INSERT INTO worker (name, password) VALUES ('myworker', crypt('mypassword', gen_salt('bf')))"
```

Replace `mypassword` with the password you want the worker to use.
