# Deployment

An Ansible playbook that deploys a janitor instance: every service as a
rootless podman container under systemd (quadlet), PostgreSQL with the
`debversion` extension, Redis and Caddy. It deploys onto Debian 13 hosts
you already have (bare metal, VMs or cloud servers), one host or
several, over SSH (see [Deploy onto your own hosts](#deploy-onto-your-own-hosts)).

Vagrant is optional: the Vagrantfiles only create local VMs to try the
playbook on (see [Vagrant (optional)](#vagrant-optional)).

## Where to find what

| Question | Section |
|---|---|
| What do I need first? | [Before you start](#before-you-start) |
| How do I run it on my own machines (bare metal, VMs, cloud)? | [Deploy onto your own hosts](#deploy-onto-your-own-hosts) |
| How do I change what a run deploys? | [What to change where](#what-to-change-where) |
| How do I check that it worked? | [Check that it works](#check-that-it-works) |
| Where do I open the site, the cupboard and the other pages? | [Using it](#using-it) |
| Which services run, on which ports, and how do I use each one? | [Every service](#every-service) |
| Where are the logins? | [Where the logins are](#where-the-logins-are) |
| How do I upgrade and back up? | [Upgrades, own campaigns, backups](#upgrades-own-campaigns-backups) |
| How do I use several hosts or more workers? | [Several hosts](#several-hosts) |
| How do I set up dev, test, staging or production? | [Environments](#environments) |
| Can I self-host everything, or use hosted services? | [Self-hosted or hosted](#self-hosted-or-hosted) |
| What does the playbook install, and where? | [Layout](#layout), [What runs where](#what-runs-where) |
| What are the optional features, and how do I turn one on? | [Optional features](#optional-features) |
| How do I try it in local VMs (Vagrant)? | [Vagrant (optional)](#vagrant-optional) |

This README covers the playbook. For janitor itself, see
[docs/production.md](../docs/production.md) (setting it up by hand) and
[docs/flow.md](../docs/flow.md) (how codebases and candidates get in).

## Before you start

- Debian 13 hosts, each with an `ansible_user` that can use sudo without
  a password (or add `--ask-become-pass`), reachable over SSH with its
  host key already in your `known_hosts` (`host_key_checking` is on).
- On the machine you run the playbook from: `ansible-core` (Debian 13's
  2.19 works) and the collections in `ansible/requirements.yml`.
- A DNS name for the control host (`janitor_domain`, the host that runs
  the site, database and runner, or your only host), with ports 80 and
  443 open to the internet so Caddy can get a certificate for it (with
  Gitea, `gitea_domain` too). Without one, set `caddy_self_signed: true`
  (a self-signed certificate that every host trusts, as in the Vagrant
  VMs) or `janitor_scheme: http` (plain HTTP, only on a network nobody
  else can read).
- Sizes tested: a control host with 8 CPUs, 24 GB of memory and 200 GB
  of disk, and workers with 6 CPUs, 12 GB and 150 GB (the Vagrant VMs).
  Such a worker ran 3 builds at once (`janitor_worker_concurrency: 3`).
- The first run builds every image from source. In the Vagrant VMs this
  takes 60 to 80 minutes. `use_prebuilt_images: true` pulls them instead.

## Deploy onto your own hosts

No Vagrant needed. From any machine that can SSH to the hosts:

```
cd deploy/ansible   # its ansible.cfg points at inventory.ini
# 1. inventory.ini: your hosts, their SSH user and key
# 2. group_vars/all.yml: janitor_domain, janitor_github_org, any enable_* switch
# 3. a secrets file outside the repository (see below)
ansible-galaxy collection install -r requirements.yml
ansible-playbook site.yml -e @/path/to/secrets.yml
```

The secrets file holds the three required secrets,
`janitor_db_password`, `worker_password` and `breezy_whoami`
(`Name <email>`), and any optional ones listed at the top of
`group_vars/all.yml`, as YAML:

```
janitor_db_password: "..."
worker_password: "..."
breezy_whoami: "Janitor Bot <bot@example.com>"
```

With several hosts, `janitor_workers` replaces `worker_password`, which
can then be left out.

Encrypt it with ansible-vault and add `--ask-vault-pass` if you like.
Apart from `breezy_whoami`, the values must not contain `@ : / % ' "`
or whitespace. `site.yml` checks this.
`janitor_github_org` names the archive signing key and janitor.conf's
`origin`.

Run the same command again after any change: it only changes what
differs, and restarts only the services whose image, unit or
configuration changed.

With GitHub (`breezy_github_token` set), each run prints the bot's SSH
public key (kept in `janitor_ssh_dir`, apart from `ansible_user`'s
`~/.ssh`). Add it to the bot account once, before publish opens its
first merge proposal.

### What to change where

| To change | Edit |
|---|---|
| Which hosts, their SSH user and key | `ansible/inventory.ini` |
| Settings for every host: domain, branch, defaults | `ansible/group_vars/all.yml` |
| Settings for one host, e.g. a worker's `janitor_worker_address` | `ansible/host_vars/<host>.yml` (create it, see [Several hosts](#several-hosts)) |
| The workers' [firewall](#several-hosts) | `enable_worker_firewall` and `worker_firewall_sources` |
| Builds a worker host runs at once (one `janitor-worker` container each) | `janitor_worker_concurrency` |
| Secrets, and the workers (`janitor_workers`) | a file passed with `-e @file`, never one in the repository |
| Campaigns | `ansible/roles/janitor_config/templates/janitor.conf.j2` |
| [Publish policies](#check-that-it-works) that candidates name | `janitor_publish_policies` |
| [Gitea](#gitea-optional) | `enable_gitea` and the `gitea_*` variables |
| [Auto-upload](#auto-upload-optional) | `enable_auto_upload` (test target: `enable_auto_upload_target`) and `auto_upload_*` |
| [ognibuild dependency server](#ognibuild-dependency-server-optional) | `enable_ognibuild_dep` and `ognibuild_dep_clients` |
| [Generic campaign](#generic-campaign-optional) | `enable_generic_campaign` (defined in `janitor.conf.j2`) |
| [S3 log storage](#s3-log-storage-optional) | `janitor_logs_location` and `janitor_s3_*` (test server: `enable_garage`) |
| [Tracing](#tracing-optional) | `enable_tracing` and the `jaeger_*` variables |
| [Monitoring](#monitoring-optional) | `enable_monitoring` and the `prometheus_*` and `grafana_*` variables |
| [Mail filter](#mail-filter-optional) | `enable_mail_filter` (test MTA: `enable_mta`) and `mail_filter_*` |
| [The Vagrant VMs](#vagrant-optional) | `ansible/group_vars/vagrant.yml`, their sizes in the Vagrantfiles |

### Check that it works

Run these on the control host. Log in over SSH as `ansible_user`,
because `systemctl --user` does not work through `sudo -u`:

```
systemctl --user list-units 'janitor-*'    # every unit active (running)
curl -s http://127.0.0.1:9911/health        # the runner: "status":"healthy"
journalctl --user -u janitor-runner         # a unit's log
```

The other ports are in [Every service](#every-service). The site is
then at `janitor_external_url` (see [Using it](#using-it)). How
codebases and candidates get in is in [docs/flow.md](../docs/flow.md).

A candidate publishes when its `publish-policy` names a policy that
publish knows. The playbook makes those in `janitor_publish_policies`
(`propose`, and `build-only`, which never publishes). For example, to
build sysvbanner:

```
curl -s -X POST -H 'Content-Type: application/json' http://127.0.0.1:9911/codebases \
  -d '[{"name": "sysvbanner", "vcs_type": "git", "branch": "master",
        "url": "https://salsa.debian.org/debian/sysvbanner.git",
        "branch_url": "https://salsa.debian.org/debian/sysvbanner.git,branch=master"}]'
curl -s -X POST -H 'Content-Type: application/json' http://127.0.0.1:9911/candidates \
  -d '[{"codebase": "sysvbanner", "campaign": "lintian-fixes", "publish-policy": "build-only"}]'
curl -s -X POST -H 'Content-Type: application/json' http://127.0.0.1:9911/schedule \
  -d '{"codebase": "sysvbanner", "campaign": "lintian-fixes"}'
```

### Using it

Where to open each part. `<url>` is `janitor_external_url`, which ends
in `/` (in the Vagrant VMs, `https://<control IP>/`, see
[Vagrant (optional)](#vagrant-optional)). To open the site on another
URL, such as through an SSH tunnel, set `janitor_site_external_url`.
It defaults to `janitor_external_url`.

| What | Where | Login |
|---|---|---|
| The site: campaigns, codebases, runs | `<url>` | none to read |
| The cupboard: the review, queue, ready and rejected pages | `<url>cupboard/` | none to read, reviewing needs the site login (below) |
| Git branches (klaus) | `<url>git/`, then `<url>git/<codebase>/` (also the clone URL) | none to read |
| Bazaar branches (loggerhead) | `<url>bzr/`, then `<url>bzr/<codebase>/` | none to read |
| The apt repository | `janitor_apt_archive_url` (below) | none |
| [Gitea](#gitea-optional) (optional) | `https://<gitea_domain>/` | a Gitea account ([Where the logins are](#where-the-logins-are)) |
| [ognibuild dependency server](#ognibuild-dependency-server-optional) (optional), an API for the workers | `<url>ognibuild-dep/` | none, only from `ognibuild_dep_clients` |

Log in at `<url>login`, linked from the cupboard. The site uses an
OpenID Connect provider (the `oauth_*` variables) or, with
`gitea_site_login: true`, Gitea. The provider's redirect URI is
`<url>oauth/callback`. Anyone logged in can review. Only reviews by
members of `oauth_qa_reviewer_group` change what publishes. Only members
of `oauth_admin_group` get the admin pages and buttons.

To use the apt repository from a host that trusts the site's
certificate:

```
sudo curl -fsSLo /etc/apt/keyrings/janitor.gpg <url>archive-keyring.gpg
echo 'deb [signed-by=/etc/apt/keyrings/janitor.gpg] <janitor_apt_archive_url> lintian-fixes main' \
  | sudo tee /etc/apt/sources.list.d/janitor.list
sudo apt update && sudo apt install -t lintian-fixes <package>
```

To clone a codebase's branches from the git store (one branch per
campaign, e.g. `lintian-fixes/main`):

```
git clone -b lintian-fixes/main <url>git/<codebase>
```

Every other service on the control host listens only on loopback
([Every service](#every-service)). To open one of those pages, forward
its port over SSH, then open `http://127.0.0.1:<port>/` on your
machine. For Prometheus:

```
ssh -N -L 9090:127.0.0.1:9090 <user>@<control host>
```

For another page, run the same command with its port. With one host,
the worker's status page is there too, on port 9821. Any user that can
log in to the control host over SSH can forward a port, unless sshd
turns off forwarding.

### Every service

Every service the playbook runs that has a port or a socket, on the
control host unless the row says otherwise. Every port listens only on
loopback, except Caddy's 80 and 443 and the workers' ports. The ports
of Gitea, Garage, Jaeger, Prometheus and Grafana are set in
`group_vars/all.yml`, e.g. `grafana_port`. The worker hosts also run
`janitor-sbuild-chroot.timer` ([What runs where](#what-runs-where)) and,
on worker-only hosts, `janitor-worker-firewall.service`. Neither has a
port.

| Service | Port | Web UI | How to use it |
|---|---|---|---|
| Caddy | 80 and 443, 2019 (Caddy's own admin API) | serves the pages under [Using it](#using-it) | `<url>` |
| `janitor-site` | 8090 (behind Caddy), 8080 (admin) | `<url>` | 8080 has only `/health` and `/metrics` |
| `janitor-runner` | 9911 (admin), 9919 (behind Caddy at `<url>runner/`, for the workers) | none | the API in [Check that it works](#check-that-it-works) |
| `janitor-publish` | 9912 | none | its API (below) |
| `janitor-differ` | 9920 | none | only the site and publish use it |
| `janitor-archive` | 9914 | none | the apt repository (above) |
| `janitor-git_store` | 9923 (admin), 9924 (behind Caddy at `<url>git/`) | `<url>git/` | `git clone` (above) |
| `janitor-bzr_store` | 9929 (admin), 9930 (behind Caddy at `<url>bzr/`) | `<url>bzr/` | its web page |
| `janitor-worker`, on each worker host | 9821, and 9820 + n for `janitor-worker-<n>`, on `janitor_worker_address` | a status page at `http://<janitor_worker_address>:9821/` | the runner sends it builds |
| PostgreSQL | 5432 | none | `psql` (below) |
| Redis | 6379 | none | `redis-cli` (below) |
| [Gitea](#gitea-optional) (optional) | 3000 (behind Caddy), 2222 (SSH, for publish) | `https://<gitea_domain>/` | its API (below) |
| [Auto-upload](#auto-upload-optional) (optional) | 9933 (`/metrics` only) | none | uploads to `auto_upload_dput_host`, or with the test target into `auto_upload_incoming` |
| [ognibuild dependency server](#ognibuild-dependency-server-optional) (optional) | 9934 (behind Caddy at `<url>ognibuild-dep/`) | none | the generic builds use it |
| [Garage](#s3-log-storage-optional) (optional, `enable_garage`) | 3900 (S3), 3901 (RPC), 3903 (admin) | none | an S3 client (below) |
| [Jaeger](#tracing-optional) (optional) | 16686 (UI and API), 16685 (gRPC), 4318 (OTLP), 9412 (Zipkin), 9411 (Caddy, which passes the spans on to 4318 and 9412), 13133 (`/status`) | `http://127.0.0.1:16686/` | the services send it their spans |
| [Prometheus](#monitoring-optional) (optional) | 9090 | `http://127.0.0.1:9090/` | its API, e.g. `/api/v1/targets` ([Monitoring](#monitoring-optional)) |
| [Grafana](#monitoring-optional) (optional) | 3300 | `http://127.0.0.1:3300/` | the Publishing and Runner dashboards |
| [Postfix](#mail-filter-optional) (optional, `enable_mta`) | 25 | none | `sendmail` (below) |
| [Mail filter](#mail-filter-optional) (optional) | none, a socket in `/run/user/<uid>/` | none | `mail_filter_command` (below) |

On the control host, as `ansible_user`:

```
curl -s http://127.0.0.1:9912/policy         # publish: the publish policies
sudo -u postgres psql janitor                # PostgreSQL, as its admin
redis-cli ping                               # Redis: PONG
curl -s -H "Authorization: token $(cat ~/gitea/secrets/janitor-bot.token)" \
  http://127.0.0.1:3000/api/v1/user          # Gitea's API, as the bot
(set -a; . ~/janitor/s3.env; aws s3 ls s3://debian-janitor/logs/)   # the build logs in Garage
printf 'Subject: test\n\nhello\n' | /usr/sbin/sendmail test@<mta domain>   # Postfix: into /var/mail/<ansible_user>
/usr/local/bin/janitor-mail-filter < <mail file>   # one forge mail into the mail filter
```

`~/janitor`, `~/gitea` and `janitor-bot` are the defaults of
`janitor_home`, `gitea_home` and `gitea_bot_user`. The S3 line needs
awscli, which the playbook does not install, and `janitor_logs_location`
as in `group_vars/vagrant.yml`. `<mta domain>` is one of `mta_domains`.

### Where the logins are

The secrets file is the one you pass with `-e @file`. Every other file
and directory below is on the control host, unless the row says
otherwise, and only `ansible_user` can read it.

| Login | Where it is |
|---|---|
| The site | the accounts of your OpenID Connect provider, or with `gitea_site_login: true` the Gitea accounts. The site's OAuth client ID and secret are `oauth_client_id` and `oauth_client_secret` in the secrets file, or with `gitea_site_login: true` in `gitea_home/secrets/oauth2-site.json` |
| Gitea: `gitea_admin_user`, `gitea_bot_user` and the `gitea_test_users` | `gitea_home/secrets/<user>.password`. The bot and each test user also have an API token in `gitea_home/secrets/<user>.token` |
| Grafana | the user `admin`, with `grafana_admin_password` from the secrets file (also in `grafana_home/grafana.env`) |
| PostgreSQL | the role `janitor`, with `janitor_db_password` from the secrets file (also in `janitor_home/janitor.conf`) |
| The workers, to the runner | `janitor_workers` (or `worker_password`) in the secrets file, and on each worker host `janitor_home/worker-credentials.json` |
| S3 | `janitor_s3_access_key_id` and `janitor_s3_secret_access_key` in the secrets file, or with Garage `garage_home/secrets/`. The runner and site read the key from `janitor_home/s3.env` |
| GitHub | `breezy_github_token` in the secrets file |
| The bot's SSH key, for GitHub and Gitea | `janitor_ssh_dir` |
| Auto-upload's signing key | `auto_upload_gnupg_home` |

Prometheus, Jaeger, Redis, Caddy's admin API and the ognibuild
dependency server have no login.

### Upgrades, own campaigns, backups

- Changing `janitor_branch` rebuilds the images. The schema is loaded
  into an empty database. Schema changes from a new `janitor_branch` are
  applied by hand.
- `janitor.conf` is a fixed template,
  `ansible/roles/janitor_config/templates/janitor.conf.j2`. Edit it for
  your own campaigns.
- Back up PostgreSQL, `janitor_home/data`, `janitor_home/gnupg-archive`
  and `janitor_ssh_dir`. Also back up `gitea_home/secrets` with Gitea,
  `garage_home` with Garage, and `auto_upload_gnupg_home` with
  auto-upload.

### Several hosts

Put the hosts in `[control]` and `[worker]`, and set:

- `janitor_domain`: a name every worker can reach the control host on.
- `janitor_worker_address` and `worker_name`, per worker host (in
  `host_vars/<host>.yml`): the address the runner reaches it on, and its
  entry in `janitor_workers`.
- `janitor_workers` in the secrets file, since it holds the passwords
  (the default is one entry made from `worker_name` and
  `worker_password`): every worker's `name` and its own `password`. The
  control host registers them all, and each worker uses the entry for
  its `worker_name`.

Workers need the control host's ports 443 and 80, and the control host
needs each worker's ports 9821 to 9820 + `janitor_worker_concurrency`.
On a host only in `[worker]` the playbook adds an nftables table,
`janitor_worker`, that lets only the control host and the host itself
reach them. It allows each control host's `ansible_host` (or
`janitor_domain`). Set `worker_firewall_sources` if the runner reaches
the workers from another address, or `enable_worker_firewall: false` to
have the next run remove the table. A drop-in for `nftables.service`
loads the table again whenever nftables starts or reloads. No other
firewall rule is touched.

## Environments

Every environment uses the same playbook. Only the inventory, the
secrets file and a few variables differ. Give each environment an
inventory with a group named after it (`[staging]` below), and put its
settings in `ansible/group_vars/<group>.yml`, as `group_vars/vagrant.yml`
does for the Vagrant VMs:

```
# ansible/inventory-staging.ini: one host, as in inventory.ini
[control]
staging1 ansible_host=203.0.113.10 ansible_user=deploy

[worker]
staging1 ansible_host=203.0.113.10 ansible_user=deploy

[janitor:children]
control
worker

[staging:children]
janitor
```

```
# ansible/group_vars/staging.yml
janitor_domain: staging.example.org
```

Then run:

```
ansible-playbook -i inventory-staging.ini site.yml -e @/path/to/staging-secrets.yml
```

`group_vars/staging.yml` overrides `group_vars/all.yml`, and `-e`
overrides both. Two things to watch:

- A `group_vars/janitor.yml`, `control.yml` or `worker.yml` overrides
  every environment's file, so keep environment settings out of them.
- A `group_vars/all.yml` in an inventory directory of its own (e.g.
  `inventories/staging/group_vars/all.yml`) loses to the playbook's
  `group_vars/all.yml` for every variable both files set.

| | Dev or test | Staging | Production |
|---|---|---|---|
| Hosts | one VM ([Vagrant](#vagrant-optional)) or one host | one host, or as in production | a control host and [worker hosts](#several-hosts) |
| Sizes | [the tested ones](#before-you-start) | as dev or test | more worker hosts, or a higher `janitor_worker_concurrency` |
| Name and certificate | `caddy_self_signed: true`, or `janitor_scheme: http` (log in to the site only on a `127.0.0.1` URL, for example through an SSH tunnel, see [Using it](#using-it)) | a DNS name of its own, a low `caddy_hsts_max_age` at first | a DNS name (Caddy gets the certificate), a low `caddy_hsts_max_age` at first |
| Secrets | throwaway values | ansible-vault, none shared with production | ansible-vault |
| Images | `use_prebuilt_images: true` (faster, `latest` may differ from `janitor_branch`) | as in production | built from `janitor_branch` (the default) |
| Services | the test servers ([Self-hosted or hosted](#self-hosted-or-hosted)) | the upload test target (`enable_auto_upload_target`), and only codebases in repositories you own | your choice ([Self-hosted or hosted](#self-hosted-or-hosted)) |
| Worker firewall | none (needs a worker-only host) | on for worker-only hosts (default) | on for worker-only hosts (default) |
| Backups | none | your choice | [what to back up](#upgrades-own-campaigns-backups) |

A staging copy with real codebases opens real proposals (pull
requests) on those codebases' own repositories.

## Self-hosted or hosted

Per part, what the playbook can run itself and which hosted service it
can use instead. Use the Self-hosted column to run everything yourself,
as the Vagrant VMs do. Servers marked "test" run on the control host
and are only for trying janitor out. The playbook supports nothing
outside this table.

| Part | Self-hosted | Hosted |
|---|---|---|
| Forge | test [Gitea](#gitea-optional) (`enable_gitea`) | GitHub (`breezy_github_token`) |
| Site login | test Gitea (`gitea_site_login: true`) | an OpenID Connect provider (see **Site login** below) |
| Build logs | a local directory (default), or test [Garage](#s3-log-storage-optional) | an S3 service (see **Build logs** below) |
| Artifacts | a local directory | |
| Uploads | the test target (`enable_auto_upload_target`) | a `dput` host reached without SSH, e.g. ftp ([Auto-upload](#auto-upload-optional)) |
| Mail | test Postfix, or your own MTA on the control host ([Mail filter](#mail-filter-optional)) | |
| Tracing | test [Jaeger](#tracing-optional) | |
| Monitoring | [Prometheus and Grafana](#monitoring-optional) on the control host | |
| Images | built from source (default) | pulled from ghcr.io (`use_prebuilt_images: true`) |
| PostgreSQL, Redis, git and bzr stores | on the control host | |

- **Forge**: the playbook writes breezy logins for GitHub and for its
  own Gitea.
- **Site login**: with an OpenID Connect provider, set
  `oauth_client_id`, `oauth_client_secret`, `oauth_base_url`,
  `oauth_qa_reviewer_group` and `oauth_admin_group`. Use a provider that
  serves `/.well-known/openid-configuration` at the root of its host and
  sends a `groups` claim.
- **Build logs**: an S3 service, see
  [S3 log storage](#s3-log-storage-optional).
- **Mail**: `mail_filter_command` talks to a socket on the control host,
  so the MTA must run there. A hosted mailbox cannot run the command.

## Layout

- `ansible/site.yml`: a play that checks the secrets, then one play per
  inventory group:
  - `control`: `base`, `janitor_source`, `postgres`, `redis`, `caddy`,
    `archive_signing_key`, `gitea` (optional), `garage` (optional),
    `jaeger` (optional), `auto_upload_target` (optional),
    `janitor_config`, `janitor_quadlet`, `mail_filter` (optional), `mta`
    (optional), `monitoring` (optional).
  - `worker`: `base`, `worker_firewall`, `janitor_source`,
    `janitor_config`, `janitor_quadlet`.

  The optional roles are described under
  [Optional features](#optional-features).
- `ansible/inventory.ini`: the hosts. One host in both groups for a single
  host.
- `ansible/group_vars/all.yml`: every variable, with the secrets listed at
  the top.
- `ansible/group_vars/vagrant.yml`: fake, local-only values for the Vagrant
  VMs.
- `ansible/requirements.yml`: the Galaxy collections the playbook needs.
- `Vagrantfile` (one VM) and `Vagrantfile.multi-host` (a control VM and two
  worker VMs).

## What runs where

`janitor_source` clones `janitor_repo` at `janitor_branch` into
`janitor_home` and builds the images the host needs with
`make build-<name>` (or pulls `ghcr.io/jelmer/janitor/<name>:latest` with
`use_prebuilt_images: true`). A build is skipped when the image is already
tagged with the checkout's commit. The builds run in
`~/janitor-build-src/`, a copy of the checkout's tracked files, so the
secrets in `janitor_home` stay out of the images. The build logs are kept in
`~/janitor-build-logs/`.

`janitor_quadlet` writes one unit per service to
`~/.config/containers/systemd/janitor-<name>.container`, starts them in
this order and waits for each health route:

| Unit | Command | Listens on |
|---|---|---|
| `janitor-git_store` | `janitor-git-store` | 127.0.0.1:9923 (admin), 127.0.0.1:9924 (public, `/git/`) |
| `janitor-bzr_store` | `janitor-bzr-store` | 127.0.0.1:9929 (admin), 127.0.0.1:9930 (public, `/bzr/`) |
| `janitor-differ` | `janitor-differ` | 127.0.0.1:9920 |
| `janitor-archive` | `janitor-archive` | 127.0.0.1:9914 |
| `janitor-runner` | `janitor-runner` | 127.0.0.1:9911 (admin), 127.0.0.1:9919 (public) |
| `janitor-publish` | `janitor-publish` | 127.0.0.1:9912 |
| `janitor-site` | `janitor-site` | 127.0.0.1:8080 (admin), 127.0.0.1:8090 (public) |
| `janitor-auto_upload` (optional) | `janitor-auto-upload` | 127.0.0.1:9933 (`/metrics` only) |
| `janitor-ognibuild_dep` (optional) | `dep-server` | 127.0.0.1:9934 |
| `janitor-worker`, `janitor-worker-<n>` | `janitor-worker --loop` | `janitor_worker_address`:9821, 9820 + n |

Caddy serves `janitor_external_url` and proxies `/runner/` (without the
prefix) to the runner's public port, `/git/` and `/bzr/` to the stores'
public ports, `/dists/` to the archive, `/ognibuild-dep/` (optional) to
the dependency server and everything else to the site.
No private port is proxied. Workers use only that URL.

With `janitor_scheme: https` (the default) plain HTTP serves only the apt
repository (`janitor_apt_archive_url`), which the build chroots read
without CA certificates. Every other path, worker logins included, is
redirected to HTTPS on `janitor_domain`. `caddy_self_signed: true`
creates a self-signed certificate for a control host without a real
domain. Every host fetches it from the control host when the playbook
runs, adds it to its CA bundle and mounts that bundle into the worker,
publish, runner and site.

With every page, file and redirect, Caddy's public sites send
`X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy` and,
unless the service sets its own, a `Content-Security-Policy` against
framing by other sites (inline scripts still run). They drop the
`Server` header. Over HTTPS they also send `Strict-Transport-Security`
(`caddy_hsts_max_age`).

Each public site logs every request as one JSON line (Caddy's default)
to the journal, which rotates it. The playbook adds `ansible_user` to
`systemd-journal`, so `journalctl -u caddy` shows them.

Each unit sets `Entrypoint=` and its whole command line.
Every unit but the worker's, whose image has its own init, sets
`RunInit=true`.

The worker builds in an unshare session (`chroot: "unshare:<suite>"` in
janitor.conf) from the image `ogni cache-env` makes, and sbuild runs in
unshare mode from a tarball made with `mmdebstrap`. Both are kept under
`janitor_home/data`. A user timer (`janitor-sbuild-chroot.timer`)
remakes the tarball every Monday and Thursday, and a failed run retries
hourly.
The published worker image has no codemods: list the packages your
campaigns need in `janitor_worker_extra_packages` and the playbook
builds a derived image with them.

Builds run with the host name `janitor-worker`, and the changelog entry
each build adds has `janitor_worker_maintainer` (`Name <email>`, by
default the same as janitor.conf's `committer`) as its author. This
keeps the worker host's name out of the `.changes` files and the build
logs.

## Optional features

Each feature is off by default. Each section says what it is, then gives
the **Switch**, what it **Needs**, and how to **Check** it (on the
control host, as `ansible_user`, unless the line says otherwise). The
Vagrant VMs turn them all on but monitoring.

### Gitea (optional)

Gitea on the control host: the playbook creates an admin, the bot
(`gitea_bot_user`, whose token publish and the runner use and whose SSH
key publish pushes with), the `gitea_test_users`, the `gitea_test_org`,
and copies `gitea_test_repos` into it. Gitea's SSH server listens on
127.0.0.1:`gitea_ssh_port` only, for publish. People clone over HTTPS.
With `gitea_site_login: true`
the site logs in through Gitea
(OAuth2). Set `oauth_qa_reviewer_group` and `oauth_admin_group` to Gitea
organizations or teams (`org:team`).

Gitea's weekly check for a new version is off. Gitea's URL,
`gitea_root_url`, is also the one the runner, publish and the site
login use, and Gitea's clone URLs start with it. So the browser, the
control host and every worker host that builds from this Gitea must
reach it. It defaults to `https://<gitea_domain>/`. With
`janitor_scheme: http`, Caddy does not serve Gitea, and Gitea listens
only on 127.0.0.1, so the workers that build from this Gitea must run
on the control host. Set `gitea_root_url` to
`http://127.0.0.1:<gitea_http_port>/` and open it through an SSH tunnel
on that same port. The site's login redirect URI follows
`janitor_site_external_url` ([Using it](#using-it)). Gitea keeps the
redirect URI from the first run, so set `janitor_site_external_url`
before that run.

- **Switch**: `enable_gitea: true`.
- **Needs**: `gitea_domain`, a second DNS name for the control host
  (with `janitor_scheme: http`, just a name). With
  `janitor_scheme: https`, Caddy serves Gitea at its root.
- **Check**: `curl -s https://<gitea_domain>/api/v1/version` prints
  Gitea's version (with `janitor_scheme: http`, run
  `curl -s <gitea_root_url>api/v1/version` on the control host).

### Auto-upload (optional)

`janitor-auto-upload` on the control host: it signs the `.changes` of
every successful Debian build (only `auto_upload_distributions` if set)
with `debsign` and uploads it with `dput`.

- **Switch**: `enable_auto_upload: true` (to try it out, also
  `enable_auto_upload_target: true`).
- **Needs**: a signing key without a passphrase in
  `auto_upload_gnupg_home` (a signing-only subkey, not the primary key),
  its id in `auto_upload_debsign_keyid`, and the upload host's section in
  `auto_upload_dput_cf` (it replaces the image's `/etc/dput.cf`), named
  by `auto_upload_dput_host`. The container has no SSH key, so the
  upload method must not need one (e.g. ftp, not scp, sftp or rsync).
  `enable_auto_upload_target` instead creates a test-only key and a dput
  host that copies uploads into `auto_upload_incoming` on the control
  host.
- **Check**: after a successful build, `journalctl --user -u
  janitor-auto_upload` shows `received` with its run id. With the test
  target, `ls <auto_upload_incoming>` lists its `.changes`.

### ognibuild dependency server (optional)

ognibuild's `dep-server` (image `ognibuild_dep`) runs on the control
host as nobody, with no capabilities and a read-only root. The runner
passes its URL to generic builds (`--public-dep-server-url`).

- **Switch**: `enable_ognibuild_dep: true`.
- **Needs**: nothing else. It has no login: Caddy serves it under
  `/ognibuild-dep/` only to `ognibuild_dep_clients` (private addresses by
  default) and answers 403 to the rest.
- **Check**: `curl -s -o /dev/null -w '%{http_code}\n'
  https://<janitor_domain>/ognibuild-dep/health` prints 200 from a
  worker and 403 from an address outside `ognibuild_dep_clients`.

### Generic campaign (optional)

A campaign named `generic` that builds a codebase's branch as it is, on
the worker, with ognibuild (for example by running `make`). It uses no
chroot and makes no Debian package. If the
[dependency server](#ognibuild-dependency-server-optional)
is on, its builds are given that server's URL.

- **Switch**: `enable_generic_campaign: true`.
- **Needs**: nothing else. Add codebases and `generic` candidates as for
  any campaign ([docs/flow.md](../docs/flow.md)). The campaign itself is
  in `janitor.conf.j2`.
- **Check**: open `<janitor_external_url>generic/`: it lists the
  campaign. A codebase with no missing dependencies gets a run with
  result `success`.

### S3 log storage (optional)

Build logs in an S3 bucket instead of a local directory
(`janitor_home/data/logs`). Artifacts stay in a local directory.

Set `janitor_s3_region` for a server outside `us-east-1`.

- **Switch**: set `janitor_logs_location` to the bucket, as
  `s3://BUCKET` or `s3://BUCKET/PREFIX`, and for a server other than AWS
  `janitor_s3_endpoint_url` to its URL (e.g. `https://s3.example.com`).
  There is no `enable_*` switch. To try it out, `enable_garage: true`
  instead, with `janitor_logs_location` set to `s3://debian-janitor/logs` and
  `janitor_s3_endpoint_url` to `http://127.0.0.1:3900` (`garage_s3_port`)
  as in `group_vars/vagrant.yml`.
- **Needs**: the bucket, and a key for it in `janitor_s3_access_key_id`
  and `janitor_s3_secret_access_key` (written to `janitor_home/s3.env`,
  mode 0600, which the runner and site sign their requests with).
  `enable_garage` instead runs Garage, an S3 server, on the control
  host's loopback with the bucket `debian-janitor` and a key of its own,
  created once in `garage_home/secrets`. Leave the S3 key unset then: a
  key passed with `-e` overrides Garage's.
- **Check**: `curl -s http://127.0.0.1:9911/health` lists `logs` as
  healthy. With Garage, `podman exec garage /garage bucket info
  debian-janitor` counts more objects after each build.

### Tracing (optional)

Jaeger on the control host's loopback, with the spans in memory (at most
`jaeger_max_traces` traces). The runner, git_store and archive send
OTLP to janitor.conf's `zipkin_address`, and the site sends Zipkin JSON
to `/api/v2/spans` on the same server. Caddy passes each to the matching
Jaeger receiver. The other services send no spans. One in 10 traces is
sampled.

- **Switch**: `enable_tracing: true`.
- **Needs**: nothing else.
- **Check**: `curl -s http://127.0.0.1:16686/api/v3/services` lists
  `janitor.runner`, `janitor.site` and the others once they have had
  traffic. The UI is at `http://127.0.0.1:16686/` on the control host
  (e.g. through `ssh -L 16686:127.0.0.1:16686`).

### Monitoring (optional)

Prometheus and Grafana on the control host's loopback. Prometheus
scrapes `/metrics` of each service in `prometheus_jobs`: every janitor
service on the control host but the dependency server and the worker,
which have no `/metrics`. Grafana gets two datasources, Prometheus and
the janitor database (read only, as `ansible_user` over PostgreSQL's
socket), and two dashboards of janitor.debian.net, Publishing and
Runner, which the playbook fetches from `grafana_dashboards_url` (a
fixed commit of https://salsa.debian.org/janitor-team/janitor.debian.net).

- **Switch**: `enable_monitoring: true`.
- **Needs**: the secret `grafana_admin_password`. Grafana reads it only
  when it first creates its database. Change it later in Grafana.
- **Check**: `curl -s http://127.0.0.1:9090/api/v1/targets` shows every
  target `"health":"up"`. Grafana is at `http://127.0.0.1:3300/` on the
  control host (e.g. through `ssh -L 3300:127.0.0.1:3300`), as `admin`.

### Mail filter (optional)

`janitor-mail-filter` reads a forge's notification mail about a proposal
and has publish refresh that proposal's status at once, through
`mail_filter_refresh_url` (publish's own `/refresh-status`). The playbook
installs `mail_filter_command`. It reads one mail from stdin and passes
it to a socket of `ansible_user`'s systemd, which runs the filter on it
(an MTA's own systemd unit may not allow rootless podman). The filter's
container can reach only this host (`mail_filter_network`), runs
read-only as nobody with no capabilities, under memory and time caps.

- **Switch**: `enable_mail_filter: true` (to try it out, also
  `enable_mta: true`).
- **Needs**: an MTA that pipes the forge mail for the bot's address into
  `mail_filter_command`, as `ansible_user`, with fewer deliveries at
  once than `mail_filter_max_instances` (`sieve/README` shows a Dovecot
  setup).
  `enable_mta` instead runs Postfix on the control host for
  `mta_domains`, taking mail from that host only and sending none to
  other hosts. Mail for `mta_mail_filter_recipients` goes to the
  filter, with at most half of `mail_filter_max_instances` running at
  once (`local_destination_concurrency_limit`). All other mail goes to
  `ansible_user`'s mailbox (up to `mta_mailbox_size_limit`). With Gitea
  on, Gitea sends its mail there.
- **Check**: after the forge mails the bot about a proposal, `journalctl
  --user -u 'janitor-mail-filter@*'` shows `Found merge proposal URL`.

## Vagrant (optional)

Local VMs to try the playbook on, not needed to deploy. Needs vagrant
with the libvirt provider. The VMs run the playbook themselves
(`ansible_local`).

```
vagrant up                                         # one VM with everything
VAGRANT_VAGRANTFILE=Vagrantfile.multi-host vagrant up   # control, worker1 and worker2
vagrant rsync && vagrant provision                 # copy changed files in, run the playbook again
vagrant ssh [control|worker1|worker2]
vagrant destroy -f
```

The site is at `https://<control IP>/` (`192.168.56.10` for one VM,
`192.168.60.11` for several), with the self-signed certificate. Both
Vagrantfiles turn on every optional feature but monitoring, with the
test servers: Gitea at `https://gitea.janitor.test/`, auto-upload of
lintian-fixes builds to the local test target, the mail filter with
Postfix (which gets Gitea's mail for the bot), the build logs in Garage,
the spans in Jaeger, the dependency server and the generic campaign.
Each VM has an /etc/hosts entry for Gitea, and the machine your browser
runs on needs one too (`<control IP> gitea.janitor.test`), or the site
login cannot reach Gitea.

With several VMs, each worker gets its own password, created once in
`.vagrant/worker-passwords.json`.

Environment variables:

- `JANITOR_SOURCE`: a local janitor checkout to deploy instead of cloning
  `JANITOR_REPO`. It is rsynced into each VM. `JANITOR_BRANCH` names the
  branch to check out (default `main`).
- `JANITOR_REPO`: the repository to clone (default upstream).
- `JANITOR_USE_PREBUILT` (default `false`): pull the published images
  instead of building `JANITOR_BRANCH`.
- One VM: `JANITOR_VM_CPUS` (8), `JANITOR_VM_MEMORY` (24576 MB),
  `JANITOR_VM_DISK` (200 GB), `JANITOR_VM_IP`.
- Several VMs: `JANITOR_NETWORK` (`192.168.60`), `JANITOR_STORAGE_POOL`
  (`default`). The sizes are in the `MACHINES` table at the top of the file.
