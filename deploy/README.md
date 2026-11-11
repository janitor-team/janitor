# Deployment

An Ansible playbook that deploys a janitor instance: every service as a
rootless podman container under systemd (quadlet), PostgreSQL with the
`debversion` extension, Redis and Caddy. It deploys onto Debian 13 hosts
you already have (bare metal, VMs or cloud servers), one host or
several, over SSH (see
[Deploy onto your own hosts](#deploy-onto-your-own-hosts)).

Vagrant is optional: the Vagrantfiles only create local VMs to try the
playbook on (see [Vagrant (optional)](#vagrant-optional)).

## Where to find what

| Question | Section |
|---|---|
| What do I need first? | [Before you start](#before-you-start) |
| How do I deploy it, step by step, on my own machines (bare metal, VMs, cloud)? | [Deploy onto your own hosts](#deploy-onto-your-own-hosts) |
| How do I change what a run deploys? | [What to change where](#what-to-change-where) |
| How do I run it with my own settings over the defaults? | [Customise it](#customise-it) |
| How do I check that it worked? | [Check that it works](#check-that-it-works) |
| Where do I open the site, the cupboard and the other pages? | [Using it](#using-it) |
| Which services run, on which ports and addresses, and how do I reach and log in to each one? | [Every service](#every-service) |
| Where are the logins? | [Where the logins are](#where-the-logins-are) |
| How do I upgrade and back up? | [Upgrades, own campaigns, backups](#upgrades-own-campaigns-backups) |
| How do I use several hosts or more workers? | [Several hosts](#several-hosts) |
| How do I set up dev, test, staging or production? | [Environments](#environments) |
| Can I self-host everything, or use hosted services? | [Self-hosted or hosted](#self-hosted-or-hosted) |
| What does the playbook install, and where? | [Layout](#layout), [What runs where](#what-runs-where) |
| What are the optional features, and how do I turn one on? | [Optional features](#optional-features) |
| How do I give the builds more apt sources, with their keys? | [Extra apt sources and the apt cache](#extra-apt-sources-and-the-apt-cache), [What runs where](#what-runs-where) |
| How do I cache the builds' apt downloads? | [apt cache (optional)](#apt-cache-optional) |
| What does each switch add, and what happens when it is off? | [Every switch, on and off](#every-switch-on-and-off) |
| What does monitoring watch, on which dashboard, with which alert? | [Monitoring](#monitoring-optional) |
| What does each variable do, and what is its default? | [Every variable](#every-variable) |
| How do I try it in local VMs (Vagrant)? | [Vagrant (optional)](#vagrant-optional) |

This README covers the playbook. For janitor itself, see
[docs/production.md](../docs/production.md) (setting it up by hand) and
[docs/scheduling.md](../docs/scheduling.md) (how codebases and
candidates get in).

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
  (a self-signed certificate, as in the Vagrant VMs) or
  `janitor_scheme: http` (plain HTTP, only on a network nobody else can
  read). The worker hosts trust the self-signed certificate. With Gitea,
  every host does.
- Sizes tested: a control host with 8 CPUs, 24 GB of memory and 200 GB
  of disk, and workers with 6 CPUs, 12 GB and 150 GB (the Vagrant VMs).
  Such a worker ran 3 builds at once (`janitor_worker_concurrency: 3`).
- The first run builds every image from source. In the Vagrant VMs this
  takes 60 to 80 minutes. `use_prebuilt_images: true` pulls them instead.

## Deploy onto your own hosts

No Vagrant needed. Run these steps from any machine that can SSH to the
hosts, in `deploy/ansible` (its `ansible.cfg` points at `inventory.ini`):

1. Check [Before you start](#before-you-start), then install the
   collections:

   ```
   cd deploy/ansible
   ansible-galaxy collection install -r requirements.yml
   ```

2. Write your hosts into `inventory.ini`. `[control]` runs the site,
   the database, the runner and Caddy. `[worker]` runs `janitor-worker`.
   For one host, put the same host in both groups. A control host and
   one worker host:

   ```
   [control]
   janitor1 ansible_host=203.0.113.10 ansible_user=deploy ansible_ssh_private_key_file=~/.ssh/id_ed25519

   [worker]
   worker1 ansible_host=203.0.113.21 ansible_user=deploy ansible_ssh_private_key_file=~/.ssh/id_ed25519

   [janitor:children]
   control
   worker
   ```

   A host only in `[worker]` also needs `host_vars/worker1.yml`
   ([Several hosts](#several-hosts)).

3. Write a secrets file outside the repository (below).

4. Write your own vars file with at least `janitor_domain` and
   `janitor_github_org`, and any `enable_*` switch
   ([Customise it](#customise-it)).

5. Run the playbook:

   ```
   ansible-playbook site.yml -e @/path/to/my-instance.yml -e @/path/to/secrets.yml
   ```

6. Run the same command again. With nothing changed, the recap shows
   `changed=0` for every host. After a change it only changes what
   differs, and restarts only the services whose image, unit or
   configuration changed.

7. Check that it works ([Check that it works](#check-that-it-works)).

8. To move to a newer janitor commit later, see
   [Upgrades, own campaigns, backups](#upgrades-own-campaigns-backups).

The secrets file is YAML. It holds the three required secrets
(`janitor_db_password`, `worker_password` and `breezy_whoami`, as
`Name <email>`) and any optional ones from the top of
`group_vars/all.yml`:

```
janitor_db_password: "..."
worker_password: "..."
breezy_whoami: "Janitor Bot <bot@example.com>"
```

With several hosts, `janitor_workers` replaces `worker_password`, which
can then be left out.

Encrypt it with ansible-vault and add `--ask-vault-pass` if you like.
`site.yml` checks that `janitor_db_password`, the worker passwords,
`breezy_github_token` and the `oauth_*` secrets hold none of
`@ : / % ' "` or whitespace, and that `grafana_admin_password` holds no
quotes or whitespace. `janitor_github_org` names the archive signing key
and janitor.conf's `origin`.

With GitHub (`breezy_github_token` set), each run prints the bot's SSH
public key (kept in `janitor_ssh_dir`, apart from `ansible_user`'s
`~/.ssh`). Add it to the bot account once, before publish opens its
first merge proposal.

### What to change where

| To change | Edit |
|---|---|
| Which hosts, their SSH user and key | `ansible/inventory.ini` |
| Settings for every host: domain, branch, switches | your own vars file passed with `-e @file` ([Customise it](#customise-it)). The defaults are in `ansible/group_vars/all.yml` |
| Settings for one host, e.g. a worker's `janitor_worker_address` | `ansible/host_vars/<host>.yml` (create it, see [Several hosts](#several-hosts)) |
| The workers' [firewall](#several-hosts) | `enable_worker_firewall` and `worker_firewall_sources` |
| Builds a worker host runs at once (one `janitor-worker` container each) | `janitor_worker_concurrency` |
| Secrets, and the workers (`janitor_workers`) | a file passed with `-e @file`, never one in the repository |
| Campaigns | `ansible/roles/janitor_config/templates/janitor.conf.j2` ([Campaigns](#campaigns)) |
| [Extra apt sources](#what-runs-where) in the sbuild chroot tarballs, for every campaign or for some, and sbuild's build command | `janitor_sbuild_extra_sources`, `janitor_sbuild_campaign_extra_sources`, `janitor_sbuild_campaigns` and `janitor_sbuild_build_command` |
| [Publish policies](#check-that-it-works) that candidates name | `janitor_publish_policies` |
| [Ports](#ports) | the `*_port` variables |
| [Log storage](#log-storage) | `janitor_logs_location` |
| [Gitea](#gitea-optional) | `enable_gitea` and the `gitea_*` variables |
| [Auto-upload](#auto-upload-optional) | `enable_auto_upload` (test target: `enable_auto_upload_target`) and `auto_upload_*` |
| [ognibuild dependency server](#ognibuild-dependency-server-optional) | `enable_ognibuild_dep` and `ognibuild_dep_clients` |
| [Generic campaign](#generic-campaign-optional) | `enable_generic_campaign` (the campaign itself is in `janitor.conf.j2`) |
| [S3 log storage](#s3-log-storage-optional) | `janitor_logs_location` and `janitor_s3_*` (test server: `enable_garage`) |
| [Tracing](#tracing-optional) | `enable_tracing` and the `jaeger_*` variables |
| [apt cache](#apt-cache-optional) | `enable_apt_cacher_ng` and the `apt_cacher_ng_*` variables |
| [Monitoring](#monitoring-optional) | `enable_monitoring`, the `prometheus_*` and `grafana_*` variables, `enable_*_exporter` and `*_exporter_port`, `enable_pushgateway`, `pushgateway_port`, `monitoring_worker_addresses` and `jaeger_metrics_port` |
| [Mail filter](#mail-filter-optional) | `enable_mail_filter` (test MTA: `enable_mta`) and `mail_filter_*` |
| [The Vagrant VMs](#vagrant-optional) | `ansible/group_vars/vagrant.yml`, their sizes in the Vagrantfiles |

### Check that it works

Run these on the control host. Log in over SSH as `ansible_user`,
because `systemctl --user` does not work through `sudo -u`:

```
systemctl --user list-units 'janitor-*'    # every unit active
curl -s http://127.0.0.1:9911/health        # the runner: "status":"healthy"
journalctl --user -u janitor-runner         # a unit's log
```

The other ports are in [Every service](#every-service). The site is
then at `janitor_external_url` (see [Using it](#using-it)). How
codebases and candidates get in is in
[docs/scheduling.md](../docs/scheduling.md).

The other web UIs listen on the control host's loopback. To open one,
forward its port over SSH ([Every service](#every-service) has a line
for each).

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

Every other service on the control host listens only on loopback. To
open one of those pages, forward its port over SSH
([Every service](#every-service) has a line for each). With one host,
the worker's status page is there too, on port 9821.

### Every service

Every port the playbook opens, on the control host unless the row says
otherwise. A port given as a variable can be changed
([Ports](#ports)). The others are fixed. The columns `dual`, `ipv4` and
`ipv6` give the bind address in each `janitor_ip_family` mode.
[IPv4 and IPv6](#ipv4-and-ipv6) lists the loopback addresses. Reach
says who can connect:

- **public**: every address of the host. The playbook adds no firewall
  rule on the control host, so open these ports yourself if a firewall
  drops them: TCP 80 and, with HTTPS, TCP 443 and UDP 443 (HTTP/3).
- **loopback**: only the host itself. Open a web UI on it through SSH
  (below).
- **firewalled**: `janitor_worker_address`. On a host only in
  `[worker]`, the `janitor_worker` table lets in only the control host
  and the host itself ([Several hosts](#several-hosts)). The control
  host must reach these ports.

| Service | Port | `dual` | `ipv4` | `ipv6` | Reach | Web UI | Login |
|---|---|---|---|---|---|---|---|
| Caddy, the public sites | 80, and with HTTPS 443 | every address | every address | every address | public | `<url>` ([Using it](#using-it)), with Gitea also `https://<gitea_domain>/` | as each page under [Using it](#using-it) |
| Caddy's admin API | 2019 | `localhost` | `localhost` | `localhost` | loopback | none | none |
| Caddy, for [tracing](#tracing-optional) (`enable_tracing`) | `tracing_port` (9411) | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | none |
| `janitor-site` | 8090 (public, behind Caddy), 8080 (admin: `/health` and `/metrics`) | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | `<url>` | none to read. Reviewing needs the site login |
| `janitor-runner` | 9911 (admin), 9919 (public, behind Caddy at `<url>runner/`) | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | the workers log in to 9919 with `janitor_workers`. 9911 has none |
| `janitor-publish` | 9912 | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | none |
| `janitor-differ` | 9920 | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | none |
| `janitor-archive` | 9914 (admin), 9915 (public, behind Caddy at `<url>dists/`) | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | none |
| `janitor-git_store` | 9923 (admin), 9924 (public, behind Caddy at `<url>git/`) | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | `<url>git/` | none to read |
| `janitor-bzr_store` | 9929 (admin), 9930 (public, behind Caddy at `<url>bzr/`) | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | `<url>bzr/` | none to read |
| `janitor-auto_upload` ([auto-upload](#auto-upload-optional)) | 9933 (`/metrics` only) | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | none |
| `janitor-ognibuild_dep` ([dependency server](#ognibuild-dependency-server-optional)) | 9934 (behind Caddy at `<url>ognibuild-dep/`) | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | none. Caddy lets in only `ognibuild_dep_clients` |
| `janitor-worker`, on each worker host | 9821, and 9820 + n for `janitor-worker-<n>` | `janitor_worker_address` (default `127.0.0.1`) | `janitor_worker_address` (default `127.0.0.1`) | `janitor_worker_address` (default `::1`) | loopback by default, firewalled on a worker-only host | a status page at `http://<janitor_worker_address>:9821/` | none |
| PostgreSQL | 5432 | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | the role `janitor` with `janitor_db_password` |
| Redis | 6379 | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | none |
| Postfix ([mail filter](#mail-filter-optional), `enable_mta`) | 25 | `127.0.0.1`, `::1` | `127.0.0.1` | `::1` | loopback | none | none, it takes mail only from this host |
| [Gitea](#gitea-optional), HTTP | `gitea_http_port` (3000) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback, public through Caddy with HTTPS | `https://<gitea_domain>/`, or `gitea_root_url` | a Gitea account |
| Gitea, SSH | `gitea_ssh_port` (2222) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | the bot's key in `janitor_ssh_dir`, for publish |
| [Garage](#s3-log-storage-optional), S3 API | `garage_s3_port` (3900) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | the S3 key in `garage_home/secrets/` |
| Garage, RPC | `garage_rpc_port` (3901) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | `garage_home/secrets/rpc_secret` |
| Garage, admin API | `garage_admin_port` (3903) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | no admin token is set. The playbook uses `/health` and, with monitoring, `/metrics` |
| [Jaeger](#tracing-optional), UI and HTTP API | `jaeger_ui_port` (16686) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | `http://127.0.0.1:16686/` | none |
| Jaeger, gRPC API | one below `jaeger_ui_port` (16685) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | none |
| Jaeger, OTLP receiver | `jaeger_otlp_port` (4318) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | none |
| Jaeger, Zipkin receiver | `jaeger_zipkin_port` (9412) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | none |
| Jaeger, health (`/status`) | `jaeger_health_port` (13133) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | none |
| Jaeger, metrics (with monitoring) | `jaeger_metrics_port` (8888) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | none |
| [Prometheus](#monitoring-optional) | `prometheus_port` (9090) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | `http://127.0.0.1:9090/`, with `/targets` and `/alerts` | none |
| Grafana | `grafana_port` (3300) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | `http://127.0.0.1:3300/` | `admin` with `grafana_admin_password` |
| Node exporter, on every host | `node_exporter_port` (9100) | control host: `127.0.0.1`, worker-only host: `janitor_worker_address` | control host: `127.0.0.1`, worker-only host: `janitor_worker_address` | control host: `::1`, worker-only host: `janitor_worker_address` | loopback on a control host, firewalled on a worker-only host | none | none |
| PostgreSQL exporter | `postgres_exporter_port` (9187) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | none |
| Redis exporter | `redis_exporter_port` (9121) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | none |
| Postfix exporter (with `enable_mta`) | `postfix_exporter_port` (9154) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | none |
| Blackbox exporter | `blackbox_exporter_port` (9115) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | none | none |
| Pushgateway (`enable_pushgateway`) | `pushgateway_port` (9091) | `127.0.0.1` | `127.0.0.1` | `::1` | loopback | `http://127.0.0.1:9091/` | none |
| [apt-cacher-ng](#apt-cache-optional) (`enable_apt_cacher_ng`) | `apt_cacher_ng_port` (3142) | `apt_cacher_ng_listen_addresses` (default `127.0.0.1`, `::1`) | `apt_cacher_ng_listen_addresses` (default `127.0.0.1`) | `apt_cacher_ng_listen_addresses` (default `::1`) | loopback by default. On another address, its nftables table, `inet janitor_apt_cacher_ng`, lets in only loopback and `apt_cacher_ng_clients` | `http://127.0.0.1:3142/acng-report.html` | none |

The mail filter has no port: `mail_filter_command` writes each mail to
the socket `/run/user/<uid>/janitor-mail-filter.sock` of `ansible_user`.
The worker hosts also run `janitor-sbuild-chroot.timer`
([What runs where](#what-runs-where)). With the node exporter, every
host runs `janitor-unit-metrics.timer`. Worker-only hosts run
`janitor-worker-firewall.service`. With the apt cache, the control host
runs `janitor-apt-cacher-ng-firewall.service` and, with monitoring and
the node exporter, `janitor-apt-cacher-ng-metrics.timer`. None of them
has a port.

To open a web UI on loopback, forward its port over SSH, then open
`http://127.0.0.1:<port>/` on your machine. `<user>` is `ansible_user`
or any other user that may log in over SSH. One line per web UI:

```
ssh -N -L 3300:127.0.0.1:3300 <user>@<control host>       # Grafana (grafana_port)
ssh -N -L 9090:127.0.0.1:9090 <user>@<control host>       # Prometheus (prometheus_port)
ssh -N -L 9091:127.0.0.1:9091 <user>@<control host>       # the Pushgateway (pushgateway_port)
ssh -N -L 16686:127.0.0.1:16686 <user>@<control host>     # Jaeger (jaeger_ui_port)
ssh -N -L 3142:127.0.0.1:3142 <user>@<control host>       # apt-cacher-ng (apt_cacher_ng_port), at /acng-report.html
ssh -N -L 9821:127.0.0.1:9821 <user>@<control host>       # the worker's status page, on a host in both groups
ssh -N -L 9821:<janitor_worker_address>:9821 <user>@<worker host>   # the status page of a worker-only host, an IPv6 address in brackets
ssh -N -L 3000:127.0.0.1:3000 <user>@<control host>       # Gitea, with janitor_scheme: http (gitea_http_port)
ssh -N -L 8000:127.0.0.1:80 <user>@<control host>         # the site, with janitor_scheme: http
```

Several at once, in one line, with one `-L` per port:

```
ssh -N -L 3300:127.0.0.1:3300 -L 9090:127.0.0.1:9090 -L 9091:127.0.0.1:9091 -L 16686:127.0.0.1:16686 <user>@<control host>
```

With `janitor_ip_family: ipv6`, forward to `[::1]` instead of
`127.0.0.1`, e.g. `-L 9090:[::1]:9090`. A site forwarded to another
local port needs `janitor_site_external_url` set to that URL, e.g.
`http://127.0.0.1:8000/`.

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
| PostgreSQL | the role `janitor`, with `janitor_db_password` from the secrets file (also in `janitor_home/janitor.conf`). With monitoring, the PostgreSQL exporter logs in as the role `prometheus` and Grafana as a role named after `ansible_user`, both over the socket without a password |
| The workers, to the runner | `janitor_workers` (or `worker_password`) in the secrets file, and on each worker host `janitor_home/worker-credentials.json` |
| S3 | `janitor_s3_access_key_id` and `janitor_s3_secret_access_key` in the secrets file, or with Garage `garage_home/secrets/`. The runner and site read the key from `janitor_home/s3.env` |
| GitHub | `breezy_github_token` in the secrets file |
| The bot's SSH key, for GitHub and Gitea | `janitor_ssh_dir` |
| Auto-upload's signing key | `auto_upload_gnupg_home` |

Prometheus, the Pushgateway, the exporters, Jaeger, Redis, Caddy's admin
API, Garage's `/metrics` (with monitoring), Grafana's `/metrics` and the
ognibuild dependency server and apt-cacher-ng have no login. Gitea's
`/metrics` needs the token in `gitea_home/secrets/metrics_token`, which
Prometheus reads.

### Upgrades, own campaigns, backups

To move to a newer janitor commit:

1. Set `janitor_branch` to the new branch or tag. Each run checks out
   the newest commit of `janitor_branch`, so a branch that moved needs
   no change.
2. Note the commit the control host runs now:
   `git -C ~/janitor rev-parse HEAD`.
3. Run the playbook. It builds the images for the new commit (with
   `use_prebuilt_images: true`, it pulls the newest `latest`) and
   restarts each service whose image changed.
4. Apply schema changes by hand. The playbook loads the schema only
   into an empty database. On the control host:

   ```
   git -C ~/janitor diff <old commit> HEAD -- schema/   # what changed
   sudo -u postgres psql janitor                        # apply it
   ```

- `janitor.conf` is a fixed template,
  `ansible/roles/janitor_config/templates/janitor.conf.j2`. Edit it for
  your own campaigns ([Campaigns](#campaigns)).
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
reach those ports and `node_exporter_port`, with monitoring on or off.
It allows each control host's `ansible_host` (or `janitor_domain`), on
every IPv4 and IPv6 address the name resolves to. Set
`worker_firewall_sources` if the runner reaches the workers from another
address, or `enable_worker_firewall: false` to have the next run remove
the table. A drop-in for `nftables.service` loads the table again
whenever nftables starts or reloads. No other firewall rule is touched.

With monitoring, a worker-only host's node exporter listens on its
`janitor_worker_address`. Prometheus on the control host scrapes it and
checks each worker's `/health`, on the addresses in
`monitoring_worker_addresses`. The default reads each worker-only host's
`janitor_worker_address`. Set it yourself when the control host's run
does not see those, as `Vagrantfile.multi-host` does. The run stops if
one of these addresses is a loopback address: set
`janitor_worker_address` for each worker-only host in
`host_vars/<host>.yml`, or set `monitoring_worker_addresses`.

### IPv4 and IPv6

`janitor_ip_family` names the address families the hosts have. These
listen on loopback only: PostgreSQL, Redis, Gitea, Garage, Jaeger,
Prometheus, Grafana, the exporters (the node exporter only on a control
host), the Pushgateway, Postfix, Caddy's tracing port, every janitor
service except janitor-worker, and apt-cacher-ng unless
`apt_cacher_ng_listen_addresses` adds other addresses. Caddy's site
listens on every address of the host. The others use these loopback
addresses:

| `janitor_ip_family` | Listen on | Connect to |
|---|---|---|
| `dual` (default) | `127.0.0.1` and `::1`, except Gitea, Garage, Jaeger, Prometheus, Grafana, the exporters and the Pushgateway, which use only `127.0.0.1` | `127.0.0.1` |
| `ipv4` (hosts without IPv6) | `127.0.0.1` | `127.0.0.1` |
| `ipv6` (no IPv4 used) | `::1` | `::1` |

`janitor_domain` may be a name, an IPv4 or an IPv6 address, and
`janitor_worker_address` an IPv4 or an IPv6 address. URLs get an IPv6
address in brackets. The commands and settings in this README reach the
control host's services on `127.0.0.1`. With `ipv6`, use `[::1]`
instead. Caddy's admin API listens on `localhost:2019` in every mode.
[Every service](#every-service) gives each port's address.

## Customise it

`group_vars/all.yml` holds the defaults. Keep your own settings in a
vars file outside the repository and pass it with `-e`, next to the
secrets file. Then the repository stays as it is, and a new commit of
the playbook does not clash with your edits:

```
ansible-playbook site.yml -e @/path/to/my-instance.yml -e @/path/to/secrets.yml
```

A value passed with `-e` wins over every other place a variable is set:
`group_vars/*.yml`, `host_vars/*.yml` and the values the playbook sets
during a run. So:

- Keep per-host settings, such as `janitor_worker_address`,
  `worker_name` and `janitor_worker_concurrency`, in
  `host_vars/<host>.yml`, not in this file.
- Leave out what the playbook sets itself: `oauth_client_id`,
  `oauth_client_secret` and `oauth_base_url` with
  `gitea_site_login: true`, `janitor_s3_access_key_id` and
  `janitor_s3_secret_access_key` with `enable_garage`, and
  `auto_upload_debsign_keyid` with `enable_auto_upload_target`.

An example: one test host that runs everything, with every optional
service on. It serves plain HTTP, and you open it through an SSH tunnel.
Caddy still listens on port 80 on every address, so keep port 80 closed
to other hosts:

```
# my-instance.yml
janitor_repo: https://github.com/example/janitor.git
janitor_branch: my-branch
use_prebuilt_images: false
janitor_github_org: example-bot

# Plain HTTP. The site is opened through an SSH tunnel on port 8000.
janitor_domain: 127.0.0.1
janitor_scheme: http
janitor_site_external_url: http://127.0.0.1:8000/

# The codemods the campaigns run.
janitor_worker_extra_packages: [lintian-brush]

# Gitea, also the site's OpenID provider, opened through the tunnel on its own port.
enable_gitea: true
gitea_domain: gitea.example.org
gitea_root_url: http://127.0.0.1:3000/
gitea_site_login: true
gitea_test_users: [maintainer]
gitea_test_org: packages
oauth_qa_reviewer_group: packages
oauth_admin_group: "packages:owners"

enable_auto_upload: true
enable_auto_upload_target: true
auto_upload_dput_host: janitor-test

enable_ognibuild_dep: true
enable_generic_campaign: true

# Needs trixie-backports in the host's apt sources (see Mail filter).
enable_mail_filter: true
enable_mta: true
mta_domains: [gitea.example.org]
mta_mail_filter_recipients: [janitor-bot]

enable_garage: true
janitor_logs_location: s3://debian-janitor/logs
janitor_s3_endpoint_url: http://127.0.0.1:3900
janitor_s3_region: garage

enable_tracing: true

enable_apt_cacher_ng: true

enable_monitoring: true
enable_pushgateway: true
# Another service on this host has 3300.
grafana_port: 3400
```

Its secrets file also holds `grafana_admin_password`. Open the site,
Gitea and Grafana in one tunnel, then `http://127.0.0.1:8000/`:

```
ssh -N -L 8000:127.0.0.1:80 -L 3000:127.0.0.1:3000 -L 3400:127.0.0.1:3400 <user>@<control host>
```

### Optional services

Turn each one on with its `enable_*` switch in your vars file.
[Every switch, on and off](#every-switch-on-and-off) says what each one
adds, how to use it and what happens without it.

### Ports

The ports of Gitea, Garage, Jaeger, Caddy's tracing port, Prometheus,
Grafana, the exporters, the Pushgateway and apt-cacher-ng are variables,
e.g. `grafana_port` ([Every variable](#every-variable)). The next run
moves the service and Prometheus's target with it. For apt-cacher-ng,
also change `apt_cacher_ng_url` if you set it, because the tarballs, the
builds and Prometheus reach it there. Caddy's 80, 443 and 2019,
PostgreSQL's 5432, Redis's 6379, Postfix's 25 and the ports of the
janitor services are fixed ([Every service](#every-service)).

### Log storage

The runner and the site keep the build logs in `janitor_logs_location`.
The default, `/mnt/janitor/data/logs`, is a path inside the containers,
`janitor_home/data/logs` on the control host. For an S3 bucket, set it
to `s3://BUCKET` or `s3://BUCKET/PREFIX`
([S3 log storage](#s3-log-storage-optional)). Artifacts always stay in
`janitor_home/data/artifacts`.

### Campaigns

`janitor.conf` is made from
`ansible/roles/janitor_config/templates/janitor.conf.j2`. Edit the
template to add or change campaigns. It has `lintian-fixes`, `control`
and, with `enable_generic_campaign`, `generic`. For a new campaign with
a Debian build:

- add the campaign and its build distribution to
  `janitor_sbuild_campaigns`, which gives it the sbuild tarball
  `<build_distribution>-amd64-sbuild`,
- add the packages its command needs to `janitor_worker_extra_packages`.

A changed `janitor.conf` restarts the control host's services on the
next run. The format is in [docs/production.md](../docs/production.md).

### Monitoring

Set `enable_monitoring: true`, and `grafana_admin_password` in the
secrets file. The exporters come with it, the Pushgateway only with
`enable_pushgateway: true`. See [Monitoring](#monitoring-optional).

### A worker on another host

1. Add the host to `[worker]` in the inventory.
2. Create `host_vars/<host>.yml` with its `janitor_worker_address` and
   `worker_name` (and its own `janitor_worker_concurrency` if it
   differs).
3. Put every worker, with its own password, in `janitor_workers` in the
   secrets file.
4. Run the playbook for every host. The control host registers the new
   worker, and the worker host gets its firewall table.

See [Several hosts](#several-hosts).

### Extra apt sources and the apt cache

The sbuild chroot tarballs have one apt source, Debian's `unstable`. To
build against another archive too, list it in
`janitor_sbuild_extra_sources` with the key that signs it. To keep the
packages the tarballs and their builds download, turn on the apt cache.
For example, Debian's debug archive and the cache:

```
# my-instance.yml
janitor_sbuild_extra_sources:
  - name: unstable-debug
    uris: http://deb.debian.org/debian-debug
    suites: unstable-debug
    components: main
    key: /usr/share/keyrings/debian-archive-keyring.gpg
enable_apt_cacher_ng: true
```

The key is a file on the machine you run the playbook from. The next
run remakes the tarballs with the new source and through the cache.
Sources for only some campaigns and the build command are in
[What runs where](#what-runs-where), the cache's settings in
[apt cache (optional)](#apt-cache-optional).

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
    `apt_cacher_ng` (optional), `janitor_config`, `janitor_quadlet`,
    `mail_filter` (optional), `mta` (optional), `host_metrics` (the node
    exporter, with monitoring), `monitoring` (optional).
  - `worker` (hosts not in `[control]`): `base`, `worker_firewall`,
    `host_metrics` (the node exporter, with monitoring),
    `janitor_source`, `janitor_config`, `janitor_quadlet`.

  The optional roles are described under
  [Optional features](#optional-features).
- `ansible/inventory.ini`: the hosts. One host in both groups for a single
  host.
- `ansible/group_vars/all.yml`: the defaults of every variable, with the
  secrets listed at the top ([Every variable](#every-variable)).
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
this order and waits for each health route. They listen on
`janitor_listen_addresses` ([IPv4 and IPv6](#ipv4-and-ipv6)), and the
workers on `janitor_worker_address`:

| Unit | Command | Ports |
|---|---|---|
| `janitor-git_store` | `janitor-git-store` | 9923 (admin), 9924 (public, `/git/`) |
| `janitor-bzr_store` | `janitor-bzr-store` | 9929 (admin), 9930 (public, `/bzr/`) |
| `janitor-differ` | `janitor-differ` | 9920 |
| `janitor-archive` | `janitor-archive` | 9914 (admin), 9915 (public, `/dists/`) |
| `janitor-runner` | `janitor-runner` | 9911 (admin), 9919 (public) |
| `janitor-publish` | `janitor-publish` | 9912 |
| `janitor-site` | `janitor-site` | 8080 (admin), 8090 (public) |
| `janitor-auto_upload` (optional) | `janitor-auto-upload` | 9933 (`/metrics` only) |
| `janitor-ognibuild_dep` (optional) | `dep-server` | 9934 |
| `janitor-worker`, `janitor-worker-<n>` | `janitor-worker --loop` | 9821, 9820 + n |

Caddy serves `janitor_external_url` and proxies `/runner/` (without the
prefix) to the runner's public port, `/git/` and `/bzr/` to the stores'
public ports, `/dists/` to the archive's public port, `/ognibuild-dep/`
(optional) to the dependency server and everything else to the site.
No private port is proxied. Workers use only that URL.

With `janitor_scheme: https` (the default) plain HTTP serves only the
apt repository (`janitor_apt_archive_url`), which the build chroots read
without CA certificates. Every other path, worker logins included, is
redirected to HTTPS on `janitor_domain`. `caddy_self_signed: true`
creates a self-signed certificate for a control host without a real
domain. When the playbook runs, the worker hosts fetch it from the
control host and add it to their CA bundle, which the worker mounts.
With Gitea, every host does, and publish, the runner and the site mount
the bundle too.

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
Every unit except the worker's, whose image has its own init, sets
`RunInit=true`.

The worker builds in an unshare session (`chroot: "unshare:<suite>"` in
janitor.conf) from the image `ogni cache-env` makes, and sbuild runs in
unshare mode from a tarball made with `mmdebstrap`. Both are kept under
`janitor_home/data`. A user timer (`janitor-sbuild-chroot.timer`)
remakes the tarballs every Monday and Thursday, and a failed run retries
hourly.

By default all campaigns share one tarball, `janitor_sbuild_chroot`, and
each campaign's `<build_distribution>-amd64-sbuild.tar.xz` (from
`janitor_sbuild_campaigns`) is a link to it. Its only apt source is
Debian's `unstable`. Each entry of `janitor_sbuild_extra_sources`
(default `[]`) adds a source to every tarball
([example](#extra-apt-sources-and-the-apt-cache)):

- `name`: a-z, 0-9 and `-`, not starting with `-`, and different for
  each entry, in this list and in
  `janitor_sbuild_campaign_extra_sources`.
- `uris`, `suites`, `components`: as in an apt `.sources` file, a string
  or a list.
- `key`: the OpenPGP key file that signs the source, on the Ansible
  controller, `.asc` (armored) or `.gpg` (binary).

For sources only some campaigns should see, list them under the
campaign in `janitor_sbuild_campaign_extra_sources` (default `{}`, each
key a campaign in `janitor_sbuild_campaigns`). A listed campaign gets a
tarball of its own instead of the link, with the shared sources and its
own. The other campaigns keep the shared tarball. For example,
lintian-fixes with experimental:

```yaml
janitor_sbuild_campaign_extra_sources:
  lintian-fixes:
    - name: experimental
      uris: http://deb.debian.org/debian
      suites: experimental
      components: main
      key: /usr/share/keyrings/debian-archive-keyring.gpg
```

The control campaign then builds without experimental, so the differ can
also show what experimental changes. List control too if that is not
wanted. Each tarball of its own takes about as long to make as the
shared one, on every run of the timer. Removing a campaign from the list
turns its tarball back into the link.

Each source becomes `/etc/apt/sources.list.d/janitor-<name>.sources` in
its tarballs. Its `Signed-By:` names that entry's copy of `key` in
`/etc/apt/keyrings`, so the source accepts only that key, and the key is
not added to the chroot's global keyring. The worker host keeps the
copies in `janitor_home/sbuild-apt`. When the entries change, the
playbook remakes the tarballs, and tries again on the next run if that
fails.

When the main suite has an older version, apt does not take a newer one
from a suite marked NotAutomatic (experimental, a backports suite), even
when a build dependency needs it. sbuild's aptitude resolver does.
`janitor_sbuild_build_command` (default `sbuild -Asv --no-clean-source`)
is the build command of janitor.conf's distribution and of the
lintian-fixes campaign. For builds that need such a suite, set it to
`sbuild -Asv --no-clean-source --build-dep-resolver=aptitude`.

With the [apt cache](#apt-cache-optional), the tarballs and the builds
in them download their http sources through apt-cacher-ng.

The published worker image has no codemods: list the packages your
campaigns need in `janitor_worker_extra_packages` and the playbook
builds a derived image with them.

Builds run with the host name `janitor-worker`, and the changelog entry
each build adds has `janitor_worker_maintainer` (`Name <email>`, by
default the same as janitor.conf's `committer`) as its author. This
keeps the worker host's name out of the `.changes` files and the build
logs.

## Optional features

Each feature is off by default, except the exporters and the worker
firewall. Each section says what it is, then gives the **Switch**, what
it **Needs**, and how to **Check** it (on the control host, as
`ansible_user`, unless the line says otherwise). The Vagrant VMs turn
them all on except the Pushgateway and the apt cache.

### Every switch, on and off

Each switch, what it adds, how to use it, and what happens when it is
off. The last column says what the next run does after you turn it off
on a host where an earlier run had it on. Most roles then leave what
they installed in place. The monitoring role stops the exporters that
are turned off and removes the Janitor mail dashboard when neither the
mail filter nor `enable_mta` with the Postfix or the node exporter is
on, and the Janitor apt cache dashboard unless `enable_apt_cacher_ng`
and the blackbox or the node exporter are on. The `host_metrics` role
stops the node exporter and its timer when monitoring or the node
exporter is off. To remove a container that stays, stop its unit as
`ansible_user`, delete its file in `~/.config/containers/systemd/` and
run `systemctl --user daemon-reload`.

| Switch (default) | What it adds | How to use it | When it is off | Turned off after a run |
|---|---|---|---|---|
| [`enable_gitea`](#gitea-optional) (`false`), with `gitea_domain` | Gitea (the user unit `gitea.service`) on `gitea_http_port` and its SSH server on `gitea_ssh_port`, served by Caddy at `https://<gitea_domain>/`. An admin, the bot and the test users, with passwords and tokens in `gitea_home/secrets/`. publish and the runner get the bot's token, publish its SSH key | log in as `gitea_admin_user`, add codebases from its repositories. With `gitea_site_login: true`, it is the site's login | no Gitea. publish and the runner have only the GitHub login (`breezy_github_token`). The site login needs your own OpenID Connect provider, or the site has none | Gitea stays and keeps running with its data. Caddy stops serving `gitea_domain`, the runner and publish lose the Gitea login, and with `gitea_site_login` the site has no login unless the `oauth_*` secrets are set |
| `gitea_site_login` (`false`), with `enable_gitea` | registers the site with Gitea as an OAuth2 application once, in `gitea_home/secrets/oauth2-site.json`, and sets `oauth_client_id`, `oauth_client_secret` and `oauth_base_url` from it | log in at `<url>login` with a Gitea account. Set `oauth_qa_reviewer_group` and `oauth_admin_group` to Gitea organisations or teams | the site logs in with the `oauth_*` secrets, or has no login | the application stays in Gitea and in `oauth2-site.json`. `janitor.conf` loses the Gitea login, so the site has no login unless the `oauth_*` secrets are set, and the control host's services restart |
| [`enable_auto_upload`](#auto-upload-optional) (`false`), with `auto_upload_dput_host`, and `auto_upload_debsign_keyid` and `auto_upload_dput_cf` or the test target | `janitor-auto_upload` on 9933 (`/metrics` only). It signs and uploads every successful Debian build of `auto_upload_distributions` (all if empty) | `journalctl --user -u janitor-auto_upload` | builds are not uploaded. They stay in the site's apt repository | `janitor-auto_upload` stays and keeps uploading. The Auto upload panels leave the dashboards |
| [`enable_auto_upload_target`](#auto-upload-optional) (`false`), with `enable_auto_upload` | a test-only signing key in `auto_upload_gnupg_home`, and a `dput.cf` with the host `auto_upload_dput_host` that copies uploads into `auto_upload_incoming` | `ls <auto_upload_incoming>` | auto-upload needs your own key (`auto_upload_debsign_keyid`) and `dput.cf`. `site.yml` checks for the key | the test key, the incoming directory and `dput.cf` stay. Write your own `dput.cf` |
| [`enable_mail_filter`](#mail-filter-optional) (`false`), with an MTA on the control host | `mail_filter_command`, the user units `janitor-mail-filter.socket` and `janitor-mail-filter@.service`, the `mail_filter` image, socat, and passt from trixie-backports | pipe the forge's mail for the bot into `mail_filter_command`, as `ansible_user` | publish sees a proposal's new status on its own next check of the proposal, not at once | the command, the units and the image stay, and the socket keeps listening. Without `enable_mta`, the Janitor mail dashboard is removed |
| [`enable_mta`](#mail-filter-optional) (`false`), with `mta_domains` | Postfix on port 25 on loopback, for `mta_domains` only, delivering locally and sending nothing out. Mail for `mta_mail_filter_recipients` goes into the mail filter, the rest into `ansible_user`'s mailbox. With Gitea, Gitea sends its mail there | `sendmail`, then `/var/mail/<ansible_user>` | no MTA. Bring your own on the control host for the mail filter. Gitea sends no mail, and there is no Postfix exporter | Postfix stays and keeps running with its configuration, the aliases and `~/.forward+mail-filter`. Gitea stops sending mail. The Postfix exporter is stopped. Without the mail filter, the Janitor mail dashboard is removed |
| [`enable_ognibuild_dep`](#ognibuild-dependency-server-optional) (`false`) | `janitor-ognibuild_dep` on 9934, served by Caddy at `<url>ognibuild-dep/` to `ognibuild_dep_clients`. The runner passes that URL to generic builds | generic builds use it | generic builds get no dependency server URL | the unit stays and keeps running on loopback. Caddy stops serving it, the runner stops passing its URL and Prometheus stops checking it |
| [`enable_generic_campaign`](#generic-campaign-optional) (`false`) | the campaign `generic` in `janitor.conf` | add codebases and `generic` candidates | no `generic` campaign | the campaign leaves `janitor.conf`, and the control host's services restart. Its runs stay in the database |
| [`enable_garage`](#s3-log-storage-optional) (`false`), with `janitor_logs_location` and `janitor_s3_endpoint_url` as in `group_vars/vagrant.yml` | Garage (the user unit `garage.service`) on `garage_s3_port`, `garage_rpc_port` and `garage_admin_port`, with the bucket `garage_bucket` and a key in `garage_home/secrets/`, which the playbook writes to `janitor_home/s3.env` | the build logs go there | the logs go to `janitor_logs_location`: the local directory by default, or a bucket with the key you set | Garage stays and keeps running with the logs in it. The runner and site no longer get its key, so set `janitor_logs_location` back to a path or to another bucket and its key |
| [`enable_tracing`](#tracing-optional) (`false`) | Jaeger (the user unit `jaeger.service`) on loopback, Caddy on `tracing_port`, and `zipkin_address` in `janitor.conf` | the UI at `http://127.0.0.1:16686/` | the services send no spans (`janitor.conf` has no `zipkin_address`) | Jaeger stays and keeps running. Caddy stops listening on `tracing_port` and the services stop sending spans |
| [`enable_monitoring`](#monitoring-optional) (`false`), with `grafana_admin_password` | Prometheus and Grafana (user units) on loopback, the exporters below, the alert rules and the dashboards. Caddy, Gitea, Garage and Jaeger serve their metrics | the Grafana and Prometheus web UIs | no metrics, dashboards or alerts | Prometheus, Grafana, the other exporters and the Pushgateway stay and keep running. The worker stops pushing. The node exporter and `janitor-unit-metrics.timer` stop on every host where the playbook set them up. `janitor-apt-cacher-ng-metrics.timer` stays and keeps running. Caddy, Gitea and Jaeger stop serving metrics, and Garage's need a token |
| `enable_node_exporter` (`true`), with monitoring | Debian's node exporter on every host, on loopback on a control host and on `janitor_worker_address` on a worker-only host. It starts after `network-online.target`, and systemd starts it again 5 seconds after it exits. Also `janitor-unit-metrics.timer`, with the unit metrics of `ansible_user`'s units and of the system units in `host_metrics_system_units` | the job `node`, its panels on Janitor host, the memory and CPU panels on the other dashboards, Units failed and Disk free on the overview, the alerts UnitFailed, UnitNotActive, UnitMetricsStale, SbuildChrootStuck and DiskAlmostFull, and with the mail filter MailFilterSocketDown and the Mail filter socket panel | no host or unit metrics, none of those alerts, and the panels that need them are left out | the node exporter and the timer stop on every host where the playbook set them up. A node exporter it did not set up is left alone. The package stays. `janitor-apt-cacher-ng-metrics.timer` stays and keeps running |
| `enable_postgres_exporter` (`true`), with monitoring | Debian's PostgreSQL exporter on `postgres_exporter_port`, logged in as the role `prometheus` with `pg_monitor` | the job `postgres`, its panels on Janitor database, the alert PostgresDown | no PostgreSQL metrics. The SQL panels stay | stopped and disabled. The package and the role `prometheus` stay |
| `enable_redis_exporter` (`true`), with monitoring | Debian's Redis exporter on `redis_exporter_port` | the job `redis`, its panels on Janitor database, the alert RedisDown | no Redis metrics | stopped and disabled |
| `enable_postfix_exporter` (`true`), with monitoring and `enable_mta` | Debian's Postfix exporter on `postfix_exporter_port`, reading `/var/log/mail.log`, and rsyslog, which copies the whole journal into `/var/log` | the job `postfix`, its panels on Janitor mail, the alert PostfixExporterFailing | no Postfix exporter metrics or panels | stopped and disabled. rsyslog stays |
| `enable_blackbox_exporter` (`true`), with monitoring | Debian's blackbox exporter on `blackbox_exporter_port`, checking the `/health` of each worker and of the dependency server, and apt-cacher-ng's report page | the jobs `worker`, `ognibuild_dep` and `apt_cacher_ng`, the alert HealthCheckFailing | the workers, the dependency server and the apt cache are not checked, and their panels are left out | stopped and disabled |
| `enable_pushgateway` (`false`), with monitoring | Debian's Pushgateway on `pushgateway_port`. A worker on a `[control]` host pushes to it after each run | the job `pushgateway`, its web UI, the time since the worker last pushed on Janitor overview | the worker pushes nothing | stopped and disabled. The worker restarts without `--prometheus` |
| [`enable_apt_cacher_ng`](#apt-cache-optional) (`false`) | apt-cacher-ng (the system unit `apt-cacher-ng.service`) on the control host on `apt_cacher_ng_port`, and its nftables table `inet janitor_apt_cacher_ng`, loaded by `janitor-apt-cacher-ng-firewall.service`. The sbuild chroot tarballs and their builds download their http sources through it. With monitoring, the job `apt_cacher_ng` with the blackbox exporter, `janitor-apt-cacher-ng-metrics.timer` with the node exporter, and with either one the Janitor apt cache dashboard | nothing to do. Its report page is at `http://127.0.0.1:3142/acng-report.html` | the tarballs and the builds download straight from the archives | apt-cacher-ng, its cache, its table and unit, and the metrics timer stay and keep running. The next run removes the cache settings, `janitor_home/sbuild-apt/proxy.conf`, and remakes the tarballs without them. Prometheus stops checking it, the Janitor apt cache dashboard is removed, and the unit metrics leave out `apt-cacher-ng.service` and `janitor-apt-cacher-ng-*.service` |
| [`enable_worker_firewall`](#several-hosts) (`true`), on hosts only in `[worker]` | the nftables table `janitor_worker`, loaded by `janitor-worker-firewall.service`: the worker ports and `node_exporter_port`, only from the control host and the host itself | nothing to do | any host that reaches `janitor_worker_address` reaches those ports | the next run stops the unit, which deletes the table, and removes its files |
| `caddy_self_signed` (`false`, an input) | a self-signed certificate for `janitor_domain` (and `gitea_domain` with Gitea) in `/etc/caddy/janitor.crt`, which Caddy serves on port 443 for any name. Each worker host, and with Gitea every host, adds it to its CA bundle | a test host without a DNS name. Accept the certificate in your browser | Caddy gets a certificate for `janitor_domain` itself, which needs a DNS name with ports 80 and 443 open to the internet ([Before you start](#before-you-start)) | Caddy gets its own certificate on the next run. The self-signed certificate stays in `/etc/caddy` and in the hosts' CA bundles |
| `use_prebuilt_images` (`false`) | `true` pulls `ghcr.io/jelmer/janitor/<name>:latest` on every run instead of building | a faster first run. `latest` may differ from `janitor_branch` | each image is built from the checkout of `janitor_branch` with `make build-<name>`, once per commit | either way, the next run builds or pulls, and restarts the services whose image changed |

### Gitea (optional)

Gitea on the control host: the playbook creates an admin, the bot
(`gitea_bot_user`, whose token publish and the runner use and whose SSH
key publish pushes with), the `gitea_test_users`, the `gitea_test_org`,
and copies `gitea_test_repos` into it. Gitea's SSH server listens on
`gitea_ssh_port` on loopback only, for publish. People clone over HTTPS.
With `gitea_site_login: true` the site login uses Gitea (OAuth2). Set
`oauth_qa_reviewer_group` and `oauth_admin_group` to Gitea organisations
or teams (`org:team`).

Gitea's weekly check for a new version is off. Gitea's URL,
`gitea_root_url`, is also the one the runner, publish and the site
login use, and Gitea's clone URLs start with it. So the browser, the
control host and every worker host that builds from this Gitea must
reach it. It defaults to `https://<gitea_domain>/`. With
`janitor_scheme: http`, Caddy does not serve Gitea, and Gitea listens
only on loopback, so the workers that build from this Gitea must run
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
- **Check**: after a successful build,
  `journalctl --user -u janitor-auto_upload` shows `received` with its
  run id. With the test target, `ls <auto_upload_incoming>` lists its
  `.changes`.

### ognibuild dependency server (optional)

ognibuild's `dep-server` (image `ognibuild_dep`) runs on the control
host as nobody, with no capabilities and a read-only root. The runner
passes its URL to generic builds (`--public-dep-server-url`).

- **Switch**: `enable_ognibuild_dep: true`.
- **Needs**: nothing else. It has no login: Caddy serves it under
  `/ognibuild-dep/` only to `ognibuild_dep_clients` (private addresses by
  default) and answers 403 to the rest.
- **Check**:
  `curl -s -o /dev/null -w '%{http_code}\n' https://<janitor_domain>/ognibuild-dep/health`
  prints 200 from a worker and 403 from an address outside
  `ognibuild_dep_clients`.

### Generic campaign (optional)

A campaign named `generic` that builds a codebase's branch as it is, on
the worker, with ognibuild (for example by running `make`). It uses no
chroot and makes no Debian package. If the
[dependency server](#ognibuild-dependency-server-optional) is on, its
builds are given that server's URL.

- **Switch**: `enable_generic_campaign: true`.
- **Needs**: nothing else. Add codebases and `generic` candidates as for
  any campaign ([docs/scheduling.md](../docs/scheduling.md)). The
  campaign itself is in `janitor.conf.j2`.
- **Check**: open `<url>generic/`: it lists the campaign. A codebase
  with no missing dependencies gets a run with result `success`.

### S3 log storage (optional)

Build logs in an S3 bucket instead of a local directory
(`janitor_home/data/logs`). Artifacts stay in a local directory.

Set `janitor_s3_region` for a server outside `us-east-1`.

- **Switch**: set `janitor_logs_location` to the bucket, as
  `s3://BUCKET` or `s3://BUCKET/PREFIX`, and for a server other than AWS
  `janitor_s3_endpoint_url` to its URL (e.g. `https://s3.example.com`).
  There is no `enable_*` switch. To try it out, `enable_garage: true`
  instead, with `janitor_logs_location` set to
  `s3://debian-janitor/logs` and `janitor_s3_endpoint_url` to
  `http://127.0.0.1:3900` (`garage_s3_port`) as in
  `group_vars/vagrant.yml`.
- **Needs**: the bucket, and a key for it in `janitor_s3_access_key_id`
  and `janitor_s3_secret_access_key` (written to `janitor_home/s3.env`,
  mode 0600, which the runner and site sign their requests with).
  `enable_garage` instead runs Garage, an S3 server, on the control
  host's loopback with the bucket `debian-janitor` (`garage_bucket`)
  and a key of its own, created once in `garage_home/secrets`. Leave the
  S3 key unset then: a key passed with `-e` overrides Garage's.
- **Check**: `curl -s http://127.0.0.1:9911/health` lists `logs` as
  healthy. With Garage,
  `podman exec garage /garage bucket info debian-janitor` counts more
  objects after each build.

### Tracing (optional)

Jaeger on the control host's loopback, with the spans in memory (at most
`jaeger_max_traces` traces). The runner, git_store and archive send OTLP
to janitor.conf's `zipkin_address`, and the site sends Zipkin JSON to
`/api/v2/spans` on the same server. Caddy passes each to the matching
Jaeger receiver. One in 10 traces is sampled.

- **Switch**: `enable_tracing: true`.
- **Needs**: nothing else.
- **Check**: `curl -s http://127.0.0.1:16686/api/v3/services` lists the
  services that sent spans, such as `janitor.runner` and `janitor.site`
  once they have had traffic. The UI is at `http://127.0.0.1:16686/` on
  the control host (e.g. through `ssh -L 16686:127.0.0.1:16686`).

### Monitoring (optional)

Prometheus and Grafana on the control host's loopback, with Debian's
exporters: the node exporter on every host, and on the control host the
PostgreSQL, Redis and blackbox exporters, and the Postfix exporter with
`enable_mta`. The Pushgateway is off by default. Prometheus scrapes
every 30 seconds and keeps `prometheus_retention` of metrics. Grafana
gets two datasources, Prometheus and the janitor database (read only,
as `ansible_user` over PostgreSQL's socket).

What Prometheus scrapes:

| Job | Target | What |
|---|---|---|
| each service in `prometheus_jobs` | its admin port ([Every service](#every-service)) | `/metrics` of every janitor service on the control host except the worker and the dependency server, which have none |
| `prometheus`, `grafana` | `prometheus_port`, `grafana_port` | their own metrics |
| `caddy` | `127.0.0.1:2019` | Caddy's admin API, with the requests of every site |
| `gitea` | `gitea_http_port` | with `enable_gitea`, read with the token in `gitea_home/secrets/metrics_token` |
| `garage` | `garage_admin_port` | with `enable_garage` |
| `jaeger` | `jaeger_metrics_port` | with `enable_tracing` |
| `node` | `node_exporter_port` on the control host, and on each host in `monitoring_worker_addresses` | each host, with a `host` label: CPU, load, memory, disks, network and the unit metrics (`enable_node_exporter`) |
| `postgres` | `postgres_exporter_port` | PostgreSQL, as the role `prometheus` with `pg_monitor` (`enable_postgres_exporter`) |
| `redis` | `redis_exporter_port` | Redis (`enable_redis_exporter`) |
| `postfix` | `postfix_exporter_port` | Postfix, with `enable_mta` and `enable_postfix_exporter` |
| `blackbox` | `blackbox_exporter_port` | the blackbox exporter's own metrics (`enable_blackbox_exporter`) |
| `worker` | through the blackbox exporter | the `/health` of each worker on this host and on each host in `monitoring_worker_addresses`, ports 9821 up to 9820 + its `janitor_worker_concurrency` |
| `ognibuild_dep` | through the blackbox exporter | the dependency server's `/health`, with `enable_ognibuild_dep` |
| `apt_cacher_ng` | through the blackbox exporter | `acng-report.html` under `apt_cacher_ng_url`, with `enable_apt_cacher_ng` |
| `pushgateway` | `pushgateway_port` | what a worker on a `[control]` host pushed after its last run, under its own job label, `janitor.worker` (`enable_pushgateway`) |

The unit metrics: on every host with the node exporter, the user timer
`janitor-unit-metrics.timer` writes the state, memory and CPU time of
each service, socket and timer of `ansible_user`'s systemd every 30
seconds into `host_metrics_textfile_dir`. The node exporter serves them
as `janitor_unit_state{unit="...",state="..."}`,
`janitor_unit_memory_bytes{unit="..."}` and
`janitor_unit_cpu_seconds_total{unit="..."}`. On a `[control]` host the
timer also writes those of the system units in
`host_metrics_system_units`: `caddy.service`, `redis-server.service`,
`postgresql@*.service`, the exporters (`prometheus-*.service`), with
`enable_mta` `postfix.service`, and with `enable_apt_cacher_ng`
`apt-cacher-ng.service` and `janitor-apt-cacher-ng-*.service` (the
cache's firewall unit and its metrics unit). On a worker-only host it
writes those of the node exporter and, with `enable_worker_firewall`, of
`janitor-worker-firewall.service`. So the dashboards show the memory and
CPU of every janitor unit and of Gitea, Garage, Jaeger, Prometheus,
Grafana, Caddy, PostgreSQL, Redis, Postfix, apt-cacher-ng and the
exporters. UnitNotActive watches the long-running services among them
and the two firewall units. On a `[control]` host with
`enable_apt_cacher_ng`, the node exporter also reads a second textfile
directory, `/var/lib/janitor-apt-cacher-ng-metrics`, where the apt
cache's timer writes its metrics ([apt cache](#apt-cache-optional)).

Each service, the job that watches it, its dashboard and its alerts.
A job's targets also count in "Services down" on Janitor overview:

| Service | Job | Dashboard | Alerts |
|---|---|---|---|
| `janitor-git_store`, `janitor-bzr_store` | `git_store`, `bzr_store` | Janitor services, Janitor forge and storage | TargetDown, UnitFailed, UnitNotActive |
| `janitor-archive` | `archive` | Janitor services, Janitor forge and storage | TargetDown, UnitFailed, UnitNotActive |
| `janitor-differ` | `differ` | Janitor services | TargetDown, UnitFailed, UnitNotActive |
| `janitor-runner` | `runner` | Janitor overview, Janitor services, Runner | RunsLost, TargetDown, UnitFailed, UnitNotActive, WorkerFailures |
| `janitor-publish` | `publish` | Janitor services, Janitor mail (the refreshes from the mail filter), Publishing | TargetDown, UnitFailed, UnitNotActive |
| `janitor-site` | `site` | Janitor services | TargetDown, UnitFailed, UnitNotActive |
| `janitor-auto_upload` | `auto_upload` | Janitor services | TargetDown, UnitFailed, UnitNotActive |
| `janitor-ognibuild_dep` | `ognibuild_dep` (its `/health`) | Janitor services | HealthCheckFailing, UnitFailed, UnitNotActive |
| `janitor-worker`, `janitor-worker-<n>` | `worker` (its `/health`), and `pushgateway` with `enable_pushgateway` | Janitor overview, Janitor services | HealthCheckFailing, SbuildChrootStuck, UnitFailed, UnitNotActive, WorkerFailures |
| apt-cacher-ng, and its firewall unit `janitor-apt-cacher-ng-firewall` | `apt_cacher_ng` (its report page) | Janitor apt cache | HealthCheckFailing, UnitFailed, UnitNotActive |
| The mail filter | none: its units' `janitor_unit_state{unit=~"janitor-mail-filter.*"}` through `node` | Janitor mail | MailFilterSocketDown, UnitFailed |
| Caddy | `caddy` | Janitor host | TargetDown, UnitFailed, UnitNotActive |
| PostgreSQL | `postgres`, and Grafana's PostgreSQL datasource for the SQL panels | Janitor database, Janitor overview | PostgresDown, TargetDown, UnitFailed, UnitNotActive |
| Redis | `redis` | Janitor database | RedisDown, TargetDown, UnitFailed, UnitNotActive |
| Gitea | `gitea` | Janitor forge and storage | TargetDown, UnitFailed, UnitNotActive |
| Garage | `garage` | Janitor forge and storage | TargetDown, UnitFailed, UnitNotActive |
| Jaeger | `jaeger` | Janitor host | TargetDown, UnitFailed, UnitNotActive |
| Postfix | `postfix` | Janitor mail | PostfixExporterFailing, TargetDown, UnitFailed, UnitNotActive |
| Prometheus | `prometheus` | Janitor host | TargetDown, UnitFailed, UnitNotActive |
| Grafana | `grafana` | Janitor host | TargetDown, UnitFailed, UnitNotActive |
| Each host, through the node exporter | `node` | Janitor host, Janitor overview | DiskAlmostFull, TargetDown, UnitFailed, UnitMetricsStale |
| The PostgreSQL, Redis and Postfix exporters | `postgres`, `redis`, `postfix` | Janitor database, Janitor mail, Janitor host | TargetDown, UnitFailed |
| The blackbox exporter | `blackbox` | Janitor host | TargetDown, UnitFailed |
| The Pushgateway | `pushgateway` | Janitor host, Janitor overview | TargetDown, UnitFailed |

The dashboards. The playbook's own dashboards leave out each panel whose
service is turned off. Janitor mail comes only with the mail filter, or
`enable_mta` with the Postfix or the node exporter, and Janitor apt
cache only with `enable_apt_cacher_ng` and the blackbox or the node
exporter:

| Dashboard | What it shows |
|---|---|
| Janitor overview | queue length, active runs, services down, units failed, firing alerts, worker failures and runs the watchdog ended in the last 24 hours, disk free on `/`, which workers are busy, runs per hour by result code, active runs per worker, the time since the worker last pushed (with `enable_pushgateway`) |
| Janitor services | each janitor service up or down, the health checks of the workers and the dependency server, requests and server errors per service, request time, assignments, results, build time, runs the watchdog ended, merge proposals, publish's delays and loops, the differ, auto-upload failures, the memory and CPU of every janitor unit |
| Janitor host | the node exporter, Caddy, Prometheus, Grafana, the blackbox exporter, Jaeger and the Pushgateway up or down, then CPU, load, memory, disks and network per host, the unit states, Caddy's requests, Prometheus's series, rules, requests and mean request time, Grafana's requests and request time, the failing health checks, Jaeger's spans, the Pushgateway's groups, and the memory and CPU of Caddy, Prometheus, Grafana, Jaeger and the exporters |
| Janitor database | PostgreSQL, Redis and their exporters: up or down, the queue by bucket and the merge proposals by status (SQL), connections, transactions, cache hits, size, deadlocks, Redis's clients, memory, commands, channels and keys, and the memory and CPU of PostgreSQL's and Redis's units |
| Janitor forge and storage | the git and bzr stores, the archive, Gitea and Garage: up or down, memory and CPU, the stores' and Garage's requests, errors and request time, Gitea's repositories, users, organisations, issues and webhook deliveries, and Garage's free disk |
| Janitor mail | the Postfix exporter up, whether it reaches Postfix, the mail filter's socket, the mail queue, connections, mail received, rejected, queued and removed, bounce notices, the status refreshes from the mail filter, and Postfix's memory and CPU |
| Janitor apt cache | apt-cacher-ng up or down and its report page's response time (with the blackbox exporter), then with the node exporter the hit ratio of files in the last hour, the bytes sent to clients and fetched from the archives, the bytes sent to each client, the files sent to each client per minute, the files served and the misses per minute, the size of the cache, and apt-cacher-ng's memory and CPU |
| Publishing, Runner | janitor.debian.net's dashboards, which the playbook fetches from `grafana_dashboards_url` (a fixed commit of https://salsa.debian.org/janitor-team/janitor.debian.net) |

The alert rules, in `prometheus_home/rules/janitor.rules`. Prometheus
shows them on `/alerts` and Grafana on its Alerting pages. Nothing
sends them on:

| Alert | Fires when | With |
|---|---|---|
| TargetDown | Prometheus cannot scrape a target for 5 minutes | always |
| HealthCheckFailing | a worker's or the dependency server's `/health`, or apt-cacher-ng's report page, does not answer 200 for 5 minutes | always. It fires only with the blackbox exporter |
| UnitFailed | a unit in the unit state file has failed for 5 minutes, on any host: a systemd unit of `ansible_user`, or one of the system units in `host_metrics_system_units` | `enable_node_exporter` |
| UnitNotActive | a long-running service unit is not active for 5 minutes, on any host: a janitor service or worker, Gitea, Garage, Jaeger, Prometheus, Grafana, Caddy, Redis, PostgreSQL, Postfix, apt-cacher-ng, the apt cache's firewall (`janitor-apt-cacher-ng-firewall`) or the worker firewall | `enable_node_exporter` |
| UnitMetricsStale | the unit state file of a host is older than 5 minutes or was never written, or the node exporter cannot read it or another textfile | `enable_node_exporter` |
| SbuildChrootStuck | a rebuild of the sbuild chroot tarballs (`janitor-sbuild-chroot.service`) has not finished for 2 hours, which is what a failing rebuild that keeps retrying looks like | `enable_node_exporter` |
| MailFilterSocketDown | `janitor-mail-filter.socket` is not active for 5 minutes | `enable_mail_filter` and `enable_node_exporter` |
| DiskAlmostFull | a disk of a host has less than 10% free for 15 minutes | `enable_node_exporter` |
| PostgresDown | the PostgreSQL exporter cannot reach PostgreSQL for 5 minutes | `enable_postgres_exporter` |
| RedisDown | the Redis exporter cannot reach Redis for 5 minutes | `enable_redis_exporter` |
| PostfixExporterFailing | the Postfix exporter cannot read a file or socket it needs for 5 minutes (the alert names it) | `enable_mta` and `enable_postfix_exporter` |
| WorkerFailures | a run ended in `worker-failure` in the last hour | always |
| RunsLost | the runner's watchdog ended a run in the last hour | always |

These web UIs are on the control host's loopback, except a worker's
status page, which is on `janitor_worker_address`. To open one, forward
its port over SSH ([Every service](#every-service) has a line for each):

| Web UI | Where | Login |
|---|---|---|
| Grafana | `http://127.0.0.1:3300/` | `admin` ([Where the logins are](#where-the-logins-are)) |
| Prometheus | `http://127.0.0.1:9090/`, with `/targets` and `/alerts` | none |
| Pushgateway, with `enable_pushgateway` | `http://127.0.0.1:9091/` | none |
| Jaeger, with `enable_tracing` | `http://127.0.0.1:16686/` | none |
| A worker's status page | `http://<janitor_worker_address>:9821/` | none |

- **Switch**: `enable_monitoring: true`. The exporters come with it,
  unless their `enable_*` is false. The Pushgateway comes only with
  `enable_pushgateway: true`. The Postfix exporter comes only with
  `enable_mta`. Debian's package reads `/var/log/mail.log`, so it also
  installs a syslog daemon (rsyslog), which copies the whole journal
  into `/var/log`.
- **Needs**: the secret `grafana_admin_password`, without quotes or
  whitespace. Only the control host's run needs it, and `site.yml`
  checks it there. Grafana reads it only when it first creates its
  database. Change it later in Grafana. With worker-only hosts, their
  `janitor_worker_address` ([Several hosts](#several-hosts)).
- **Check**: `curl -s http://127.0.0.1:9090/api/v1/targets` shows every
  target `"health":"up"`, and
  `curl -s -u admin http://127.0.0.1:3300/api/search` (it asks for
  `grafana_admin_password`) lists the dashboards.

### apt cache (optional)

apt-cacher-ng on the control host keeps the packages and index files
that the sbuild chroot tarballs and the builds in them download, so a
package is fetched from the archive only once. The tarballs get the
cache through mmdebstrap's `--aptopt`, which also stays in them for the
builds. https sources are not cached: apt reaches them directly, or
through the cache without caching for
`apt_cacher_ng_pass_through_hosts`. apt reaches the hosts in
`apt_cacher_ng_direct_hosts` (by default the host of
`janitor_apt_archive_url`) without the cache, so janitor's own archive
is never served old. The `ogni cache-env` session image does not use the
cache. squid with Debian's `prometheus-squid-exporter` is an alternative
with more metrics. The playbook installs apt-cacher-ng.

It listens on loopback. For workers on other hosts, add an address of
the control host to `apt_cacher_ng_listen_addresses` and set
`apt_cacher_ng_url` to it. Its nftables table,
`inet janitor_apt_cacher_ng`, lets in loopback and
`apt_cacher_ng_clients` (by default the `ansible_host` of each host only
in `[worker]`) and resets every other connection to the port. Set
`apt_cacher_ng_clients` if the workers reach it from other addresses.
For example, a control host at 192.0.2.10 on the internal network, and
https sources on deb.debian.org passed through the cache:

```yaml
enable_apt_cacher_ng: true
apt_cacher_ng_listen_addresses: [127.0.0.1, "::1", 192.0.2.10]
apt_cacher_ng_url: http://192.0.2.10:3142/
apt_cacher_ng_pass_through_hosts: [deb.debian.org]
```

With `enable_monitoring` and `enable_node_exporter`, a timer
(`janitor-apt-cacher-ng-metrics.timer`) reads apt-cacher-ng's transfer
log every minute, as the user `apt-cacher-ng`, and writes
`apt_cacher_ng_*` metrics into
`/var/lib/janitor-apt-cacher-ng-metrics/apt_cacher_ng.prom`, which the
node exporter reads: files sent to clients and their bytes (in total and
for each client), downloads from the archives (misses) and their bytes,
and the size of the cache. The counts start again from zero when
logrotate rotates the log.

Changing the switch, `apt_cacher_ng_url`, `apt_cacher_ng_direct_hosts`
or `apt_cacher_ng_pass_through_hosts` remakes the sbuild chroot
tarballs.

- **Switch**: `enable_apt_cacher_ng: true`.
- **Needs**: `janitor_domain` must be a host name or an IPv4 address, as
  it is in `apt_cacher_ng_direct_hosts`, which takes no IPv6 address.
  With hosts only in `[worker]`, each needs an `ansible_host` (or set
  `apt_cacher_ng_clients`), and `apt_cacher_ng_url` must name an address
  in `apt_cacher_ng_listen_addresses` that is not loopback. The builds
  resolve its host name with DNS only, not with `/etc/hosts`, so give an
  IP address or a name in DNS. `apt_cacher_ng_clients` must not list a
  loopback name or address.
- **Check**:
  `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:3142/acng-report.html`
  prints 200, and `sudo tail /var/log/apt-cacher-ng/apt-cacher.log`
  shows the tarballs' and the builds' downloads.

### Mail filter (optional)

`janitor-mail-filter` reads a forge's notification mail about a proposal
and has publish refresh that proposal's status at once, through
`mail_filter_refresh_url` (publish's own `/refresh-status`). The
playbook installs `mail_filter_command`. It reads one mail from stdin
and passes it to a socket of `ansible_user`'s systemd, which runs the
filter on it. The filter's container can reach only the refresh URL's
port on the control host (`mail_filter_network`), runs read-only as
nobody with no capabilities, under memory and time caps. Each mail gets
its own instance of `janitor-mail-filter@.service`. systemd drops an
instance once it ends, even a failed one, so a failed run leaves no
failed unit behind. Its output stays in the journal.

- **Switch**: `enable_mail_filter: true` (to try it out, also
  `enable_mta: true`).
- **Needs**: an MTA that pipes the forge mail for the bot's address into
  `mail_filter_command`, as `ansible_user`, with fewer deliveries at
  once than `mail_filter_max_instances` (`sieve/README` shows a Dovecot
  setup). `enable_mta` instead runs Postfix on the control host for
  `mta_domains`, taking mail from that host only and sending none to
  other hosts. Mail for `mta_mail_filter_recipients` goes to the filter,
  with at most half of `mail_filter_max_instances` running at once
  (`local_destination_concurrency_limit`). All other mail goes to
  `ansible_user`'s mailbox (up to `mta_mailbox_size_limit`). With Gitea
  on, Gitea sends its mail there. The control host also needs
  trixie-backports in its apt sources: the playbook installs passt from
  there for pasta's `--splice-only`, which `mail_filter_network` uses.
- **Check**: after the forge mails the bot about a proposal,
  `journalctl --user -u 'janitor-mail-filter@*'` shows
  `Found merge proposal URL`.

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

The site is at `https://192.168.56.10/` for one VM and at
`https://control.janitor.test/` (`192.168.60.11`) for several, with the
self-signed certificate. Both Vagrantfiles turn on every optional
feature except the Pushgateway and the apt cache, with the test servers:
Gitea at `https://gitea.janitor.test/`, auto-upload of lintian-fixes
builds to the local test target, the mail filter with Postfix (which
gets Gitea's mail for the bot), the build logs in Garage, the spans in
Jaeger, the dependency server, the generic campaign, and Prometheus and
Grafana with every exporter. Each VM has an /etc/hosts entry for Gitea,
and the machine your browser runs on needs one too
(`<control IP> gitea.janitor.test`, and `control.janitor.test` for
several VMs), or the site login cannot reach Gitea.

With several VMs, each worker gets its own password, created once in
`.vagrant/worker-passwords.json`. Each VM runs the playbook for itself
only, so `Vagrantfile.multi-host` passes control the workers'
addresses in `monitoring_worker_addresses`.

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
- Several VMs: `JANITOR_NET` (`dual`) sets the test network's address
  families and `janitor_ip_family`: `ipv4` (IPv6 turned off in the VMs),
  `ipv6` (the VMs reach each other over IPv6 only) or `dual`.
  `JANITOR_NETWORK6` (`fd6a:6e69:7400:60`) is its IPv6 /64. Each VM's
  /etc/hosts has `<VM>.janitor.test` for every VM, and vagrant's own
  IPv4 link stays for ssh and downloads. `vagrant up probe` adds a small
  client VM on the same network. With `ipv6` the machine running vagrant
  has no address on the test network, so check the site from probe.

## Every variable

Every variable the playbook defines or reads, with its default. Set them
in your own vars file ([Customise it](#customise-it)), the secrets file
or `host_vars/<host>.yml`. "Derived, do not set" marks a value the
playbook computes from others. "Secret" marks a value for the secrets
file. "Input" marks a variable with no line in `group_vars/all.yml`.
Variables from `roles/*/vars/main.yml` win over `group_vars/*.yml` and
`host_vars/*.yml`, so only `-e` changes them.

### Core

| Variable | Default | What it does |
|---|---|---|
| `janitor_repo` | `https://github.com/janitor-team/janitor.git` | the janitor repository each host clones |
| `janitor_branch` | `main` | the branch, tag or commit each host checks out and builds |
| `janitor_home` | `/home/<ansible_user>/janitor` | the checkout, `janitor.conf`, the data directories and the keys |
| `janitor_domain` | `CHANGEME` | the control host's DNS name or address. Caddy serves it, and the workers reach the control host on it |
| `janitor_github_org` | `CHANGEME` | the GitHub user or organisation the bot works as. Also janitor.conf's `origin`, and the name of the keys the playbook makes |
| `janitor_scheme` | `https` | `http` serves everything over plain HTTP, worker logins included |
| `caddy_self_signed` | `false` (input) | a self-signed certificate for `janitor_domain` instead of one Caddy gets. Every host that needs it trusts it |
| `caddy_hsts_max_age` | `31536000` | the max-age of `Strict-Transport-Security`, in seconds |
| `janitor_external_url` | `<janitor_scheme>://<janitor_url_host>/` | the URL the workers, the runner and publish use |
| `janitor_site_external_url` | `janitor_external_url` (input) | the URL the site makes its links and login redirect with, e.g. through an SSH tunnel |
| `janitor_apt_archive_url` | `http://<janitor_url_host>/` | the apt repository, over plain HTTP for the build chroots |
| `janitor_url_host` | `janitor_domain`, an IPv6 address in brackets | derived, do not set |
| `janitor_ip_family` | `dual` | the address families the hosts have: `ipv4`, `ipv6` or `dual` |
| `janitor_loopback_addresses` | the loopback addresses of `janitor_ip_family` | derived, do not set. `127.0.0.1` and `::1` in `dual` |
| `janitor_loopback` | `127.0.0.1`, or `[::1]` with `ipv6` | derived, do not set. The address the services reach each other on |
| `janitor_listen_addresses` | `janitor_loopback_addresses` | the addresses the janitor services listen on, except the worker |
| `janitor_extra_hosts` | `{}` | extra `/etc/hosts` entries on every host, as `{name: address}` |
| `use_prebuilt_images` | `false` | `true` pulls `ghcr.io/jelmer/janitor/<name>:latest` instead of building `janitor_branch` |
| `janitor_publish_policies` | `propose` and `build-only`, each for the branch role `main` | the named publish policies the playbook makes in publish |
| `janitor_lintian_fixes_build_distribution` | `lintian-fixes` | the lintian-fixes campaign's build distribution, also in its sbuild chroot name |
| `postgres_max_connections` | `300` | PostgreSQL's connection limit |
| `janitor_logs_location` | `/mnt/janitor/data/logs` | where the build logs go: a path inside the containers, or `s3://BUCKET[/PREFIX]` ([Log storage](#log-storage)) |
| `janitor_control_services` | `git_store`, `bzr_store`, `differ`, `archive`, `runner`, `publish`, `site` | the services of a `[control]` host, in start order. Do not change |
| `janitor_services` | the services of this host's groups | derived, do not set |
| `janitor_units` | one unit per service, and one per worker instance | derived, do not set |
| `janitor_images` | the images this host needs | derived, do not set |
| `janitor_health_urls` | each service's `/health` on loopback (`/metrics` for auto-upload) | derived, do not set. The playbook waits for these, and Prometheus scrapes their host and port |
| `janitor_breezy_dir` | `/home/<ansible_user>/.config/janitor-breezy` | a breezy config directory per service |
| `janitor_breezy_services` | the services of this host that use breezy | derived, do not set |
| `janitor_ssh_dir` | `/home/<ansible_user>/.config/janitor-ssh` | the bot's SSH key and pinned host keys. Only publish mounts it |

### Secrets and the site login

| Variable | Default | What it does |
|---|---|---|
| `janitor_db_password` | none, required (secret, input) | the password of the PostgreSQL role `janitor` |
| `worker_password` | none (secret, input) | the password of the default `janitor_workers` entry. Not needed when you set `janitor_workers` |
| `breezy_whoami` | none, required (secrets file, input) | `Name <email>`, the commit author of publish and the worker |
| `breezy_github_token` | none (secret, input) | the GitHub token publish pushes and opens pull requests with, and the runner lists proposals with |
| `oauth_client_id` | none (secret, input) | the site's OAuth2 client ID. Without it the site has no login. With `gitea_site_login`, the playbook sets it |
| `oauth_client_secret` | none (secret, input) | the site's OAuth2 client secret. With `gitea_site_login`, the playbook sets it |
| `oauth_base_url` | none (input). With `gitea_site_login`, `gitea_root_url` | the OpenID Connect provider |
| `oauth_qa_reviewer_group` | none (input) | the group whose reviews change what publishes |
| `oauth_admin_group` | none (input) | the group that gets the admin pages and buttons |

### Worker

| Variable | Default | What it does |
|---|---|---|
| `janitor_worker_address` | the first loopback address of `janitor_ip_family` | the address the worker listens on and the runner reaches it on. Set it per worker-only host |
| `janitor_worker_url_host` | `janitor_worker_address`, an IPv6 address in brackets | derived, do not set |
| `janitor_worker_concurrency` | `1` | builds this worker host runs at once, one container each |
| `janitor_worker_instances` | `{worker: 9821, worker-2: 9822, ...}` | derived, do not set |
| `janitor_worker_ports` | `9821`, or `9821-<9820 + concurrency>` | derived, do not set |
| `worker_name` | `worker-1` | this worker host's entry in `janitor_workers` |
| `janitor_workers` | one entry, `worker_name` with `worker_password` | every worker's `name` and `password` (secret). The control host registers them all |
| `janitor_worker_password` | this host's password in `janitor_workers` | derived, do not set |
| `janitor_worker_session_suite` | `sid` | the suite of the build session image `ogni cache-env` makes |
| `janitor_sbuild_chroot` | `unstable-amd64-sbuild` | the name of the shared sbuild tarball, for janitor.conf's distribution |
| `janitor_sbuild_campaigns` | `{lintian-fixes: <janitor_lintian_fixes_build_distribution>, control: unchanged}` | the campaigns that build with sbuild, each with its build distribution ([Campaigns](#campaigns)) |
| `janitor_sbuild_chroot_aliases` | `<build_distribution>-amd64-sbuild` for each entry of `janitor_sbuild_campaigns` | derived, do not set. The campaigns' tarball names, each a link to the shared tarball unless the campaign has its own |
| `janitor_sbuild_extra_sources` | `[]` | extra apt sources in every sbuild tarball, as `{name, uris, suites, components, key}` ([What runs where](#what-runs-where)) |
| `janitor_sbuild_campaign_extra_sources` | `{}` | extra apt sources for some campaigns, as `{campaign: [sources]}`. Each listed campaign gets a tarball of its own |
| `janitor_sbuild_build_command` | `sbuild -Asv --no-clean-source` | the build command of janitor.conf's distribution and of the lintian-fixes campaign |
| `janitor_quadlet_sbuild_apt_dir` | `<janitor_home>/sbuild-apt` | the extra sources, their keys and the apt cache settings on the worker host, which the worker mounts (role variable) |
| `janitor_quadlet_sbuild_all_sources` | `janitor_sbuild_extra_sources`, then the sources of each campaign | derived, do not set (role variable) |
| `janitor_quadlet_sbuild_campaign_chroots` | the tarball names of the campaigns in `janitor_sbuild_campaign_extra_sources` | derived, do not set (role variable) |
| `janitor_quadlet_sbuild_apt_files` | the source and key files that belong in `janitor_quadlet_sbuild_apt_dir` | derived, do not set (role variable). The run removes the others |
| `janitor_worker_extra_packages` | `[]` | packages for a derived worker image, e.g. the codemods |
| `janitor_worker_image` | `localhost/janitor-worker-custom:latest` with extra packages, else `ghcr.io/jelmer/janitor/worker:latest` | derived, do not set |
| `janitor_worker_maintainer` | `Janitor Bot <noreply@<janitor_domain>>` | the author of the changelog entry each build adds |
| `enable_worker_firewall` | `true` | the nftables table on worker-only hosts ([Several hosts](#several-hosts)) |
| `worker_firewall_sources` | each `[control]` host's `ansible_host`, or `janitor_domain` | the names or addresses the table lets in |
| `worker_firewall_ports` | the worker ports and `node_exporter_port` | derived, do not set |

### Gitea

| Variable | Default | What it does |
|---|---|---|
| `enable_gitea` | `false` | [Gitea](#gitea-optional) on the control host |
| `gitea_domain` | `CHANGEME` | Gitea's host name |
| `gitea_root_url` | `https://<gitea_domain>/` (input) | Gitea's URL for the browser, the runner, publish and the site login |
| `gitea_image` | `docker.io/gitea/gitea:1.27.3-rootless` | Gitea's image |
| `gitea_home` | `/home/<ansible_user>/gitea` | Gitea's data and secrets |
| `gitea_http_port` | `3000` | Gitea's HTTP port |
| `gitea_ssh_port` | `2222` | Gitea's SSH port, for publish |
| `gitea_admin_user` | `gitea-admin` | the admin account |
| `gitea_bot_user` | `janitor-bot` | the bot's account |
| `gitea_bot_token_scopes` | `write:repository,write:issue,read:user,read:organization` | the scopes of the bot's token |
| `gitea_test_users` | `[]` | test accounts |
| `gitea_test_token_scopes` | `write:repository,write:issue,write:organization,read:user` | the scopes of the test accounts' tokens |
| `gitea_test_org` | `""` | an organisation owned by the first test user |
| `gitea_test_repos` | `[]` | repositories copied into `gitea_test_org` once, as `{name, clone_url}` |
| `gitea_site_login` | `false` | registers the site with Gitea and logs in to the site there |
| `gitea_users` | the admin, the bot and the test users | derived, do not set (role variable) |

### Auto-upload

| Variable | Default | What it does |
|---|---|---|
| `enable_auto_upload` | `false` | [auto-upload](#auto-upload-optional) on the control host |
| `auto_upload_dput_host` | `""` | the section of `auto_upload_dput_cf` it uploads to |
| `auto_upload_debsign_keyid` | `""` | the signing key's id. With the test target, the playbook sets it |
| `auto_upload_gnupg_home` | `<janitor_home>/gnupg-auto-upload` | the signing key's GNUPG home |
| `auto_upload_dput_cf` | `<janitor_home>/dput.cf` | the `dput.cf` mounted into the container |
| `auto_upload_distributions` | `[]` | only builds for these build distributions. Empty uploads them all |
| `enable_auto_upload_target` | `false` | a test key and a local upload target |
| `auto_upload_incoming` | `<janitor_home>/data/upload-incoming` | the test target's incoming directory |

### Dependency server and generic campaign

| Variable | Default | What it does |
|---|---|---|
| `enable_ognibuild_dep` | `false` | the [dependency server](#ognibuild-dependency-server-optional) on the control host |
| `ognibuild_dep_clients` | `[private_ranges]` | the client address ranges Caddy lets in under `/ognibuild-dep/`. The default is Caddy's private address ranges |
| `enable_generic_campaign` | `false` | the [generic campaign](#generic-campaign-optional) |

### S3 and Garage

| Variable | Default | What it does |
|---|---|---|
| `janitor_s3_endpoint_url` | `""` | the S3 server's URL, for a server other than AWS |
| `janitor_s3_access_key_id` | `""` | the S3 key's id. With Garage, the playbook sets it |
| `janitor_s3_secret_access_key` | `""` | the S3 key's secret (secret). With Garage, the playbook sets it |
| `janitor_s3_region` | `us-east-1` | the S3 region |
| `enable_garage` | `false` | [Garage](#s3-log-storage-optional) on the control host |
| `garage_image` | `docker.io/dxflrs/garage:v2.4.1` | Garage's image |
| `garage_home` | `/home/<ansible_user>/garage` | Garage's data and secrets |
| `garage_s3_port` | `3900` | Garage's S3 API |
| `garage_rpc_port` | `3901` | Garage's RPC port |
| `garage_admin_port` | `3903` | Garage's admin API |
| `garage_capacity` | `10G` | the capacity of Garage's one node |
| `garage_bucket` | `debian-janitor` | the bucket Garage makes (role variable) |

### Tracing

| Variable | Default | What it does |
|---|---|---|
| `enable_tracing` | `false` | [Jaeger](#tracing-optional) on the control host |
| `jaeger_image` | `docker.io/jaegertracing/jaeger:2.22.0` | Jaeger's image |
| `jaeger_home` | `/home/<ansible_user>/jaeger` | Jaeger's configuration |
| `tracing_port` | `9411` | Caddy's port for the spans, janitor.conf's `zipkin_address` |
| `jaeger_otlp_port` | `4318` | Jaeger's OTLP receiver |
| `jaeger_zipkin_port` | `9412` | Jaeger's Zipkin receiver |
| `jaeger_ui_port` | `16686` | Jaeger's UI and HTTP API. Its gRPC API listens one port below |
| `jaeger_health_port` | `13133` | Jaeger's `/status` |
| `jaeger_metrics_port` | `8888` | Jaeger's own metrics, with monitoring |
| `jaeger_max_traces` | `10000` | the traces Jaeger keeps in memory |
| `jaeger_memory` | `512m` | Jaeger's memory cap |

### apt cache

| Variable | Default | What it does |
|---|---|---|
| `enable_apt_cacher_ng` | `false` | the [apt cache](#apt-cache-optional) on the control host |
| `apt_cacher_ng_port` | `3142` | its port |
| `apt_cacher_ng_listen_addresses` | `janitor_loopback_addresses` | where it listens. Add an address of the control host for workers on other hosts |
| `apt_cacher_ng_clients` | the `ansible_host` of each host in `[worker]` and not in `[control]` | names or addresses, resolved on the control host, that its nftables table lets in besides loopback |
| `apt_cacher_ng_url` | `http://<janitor_loopback>:<apt_cacher_ng_port>/` | the URL the sbuild tarballs and builds use. A worker on another host needs an address of `apt_cacher_ng_listen_addresses` here |
| `apt_cacher_ng_direct_hosts` | the host of `janitor_apt_archive_url` | http hosts apt reaches without the cache. Host names or IPv4 addresses |
| `apt_cacher_ng_pass_through_hosts` | `[]` | https hosts apt reaches through the cache, without caching (`PassThroughPattern`). Other https hosts are reached directly |

### Monitoring

| Variable | Default | What it does |
|---|---|---|
| `enable_monitoring` | `false` | [Prometheus and Grafana](#monitoring-optional) on the control host |
| `prometheus_image` | `docker.io/prom/prometheus:v3.15.0` | Prometheus's image |
| `prometheus_home` | `/home/<ansible_user>/prometheus` | its configuration, rules and data |
| `prometheus_port` | `9090` | its port |
| `prometheus_retention` | `15d` | how long it keeps metrics |
| `prometheus_jobs` | `janitor_services` without the worker and the dependency server | the janitor services Prometheus scrapes on `/metrics` |
| `grafana_image` | `docker.io/grafana/grafana:13.2.4` | Grafana's image |
| `grafana_home` | `/home/<ansible_user>/grafana` | its data, provisioning and dashboards |
| `grafana_port` | `3300` | its port |
| `grafana_admin_password` | none (secret, input) | the password of Grafana's `admin`, read when Grafana first creates its database |
| `grafana_dashboards_url` | a fixed commit of janitor.debian.net's `grafana` directory | where the playbook fetches the upstream dashboards from |
| `grafana_dashboards` | `publisher` and `runner`, each with its sha256 | the upstream dashboards and their checksums |
| `enable_node_exporter` | `true` | the node exporter and the unit metrics on every host |
| `node_exporter_port` | `9100` | its port |
| `enable_postgres_exporter` | `true` | the PostgreSQL exporter |
| `postgres_exporter_port` | `9187` | its port |
| `enable_redis_exporter` | `true` | the Redis exporter |
| `redis_exporter_port` | `9121` | its port |
| `enable_postfix_exporter` | `true` | the Postfix exporter, with `enable_mta` |
| `postfix_exporter_port` | `9154` | its port |
| `enable_blackbox_exporter` | `true` | the blackbox exporter, which checks the `/health` of each worker and of the dependency server, and apt-cacher-ng's report page |
| `blackbox_exporter_port` | `9115` | its port |
| `enable_pushgateway` | `false` | the Pushgateway, which a worker on a `[control]` host pushes to |
| `pushgateway_port` | `9091` | its port |
| `janitor_worker_pushgateway_url` | the Pushgateway's URL on a `[control]` host with `enable_monitoring` and `enable_pushgateway`, else `""` | derived, do not set. The worker's `--prometheus` |
| `monitoring_worker_addresses` | every host only in `[worker]`, with its `janitor_worker_address` | the worker-only hosts Prometheus scrapes, as `{inventory name: address}` |
| `monitoring_exporters` | the exporters turned on | derived, do not set (role variable) |
| `monitoring_exporter_settings` | each exporter's package, port and arguments | derived, do not set (role variable) |
| `monitoring_dashboards` | the playbook's own dashboards, Janitor mail only with the mail filter, or `enable_mta` with the Postfix or the node exporter, Janitor apt cache only with `enable_apt_cacher_ng` and the blackbox or the node exporter | derived, do not set (role variable) |
| `host_metrics_address` | loopback on a `[control]` host, else `janitor_worker_address` | derived, do not set (role variable). The node exporter's address |
| `host_metrics_textfile_dir` | `/var/lib/janitor-unit-metrics` | where the unit metrics file goes (role variable). On a `[control]` host with `enable_apt_cacher_ng`, the node exporter also reads `/var/lib/janitor-apt-cacher-ng-metrics` |
| `host_metrics_system_units` | on a `[control]` host `caddy.service`, `redis-server.service`, `postgresql@*.service`, `prometheus-*.service`, with `enable_mta` `postfix.service` and with `enable_apt_cacher_ng` `apt-cacher-ng.service` and `janitor-apt-cacher-ng-*.service`, on a worker-only host `prometheus-node-exporter.service` and, with `enable_worker_firewall`, `janitor-worker-firewall.service` | derived, do not set (role variable). The system units the unit metrics also cover |

### Mail

| Variable | Default | What it does |
|---|---|---|
| `enable_mail_filter` | `false` | the [mail filter](#mail-filter-optional) on the control host |
| `mail_filter_command` | `/usr/local/bin/janitor-mail-filter` | the command an MTA pipes each mail into |
| `mail_filter_refresh_url` | `http://<janitor_loopback>:9912/refresh-status` | publish's route the filter calls |
| `mail_filter_network` | `pasta:--splice-only,-T,<the refresh URL's port>` | the filter container's network: only the refresh URL's port |
| `mail_filter_memory` | `128m` | the filter's memory cap |
| `mail_filter_timeout` | `60` | seconds before the filter is killed |
| `mail_filter_max_instances` | `8` | filter runs at once, the socket's connection limit |
| `enable_mta` | `false` | the test Postfix on the control host |
| `mta_domains` | `[]` | the domains Postfix takes mail for |
| `mta_mail_filter_recipients` | `[]` | the local parts whose mail goes into the filter |
| `mta_mailbox_size_limit` | `268435456` | the cap on `ansible_user`'s mailbox, in bytes |
