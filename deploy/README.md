# Deployment

An Ansible playbook that deploys a janitor instance: every service as a
rootless podman container under systemd (quadlet), PostgreSQL with the
`debversion` extension, Redis and Caddy. It deploys onto one Debian 13
host or onto several, and comes with Vagrant VMs to try it locally.

## Layout

- `ansible/site.yml`: a play that checks the secrets, then one play per
  inventory group:
  - `control`: `base`, `janitor_source`, `postgres`, `redis`, `caddy`,
    `archive_signing_key`, `gitea` (optional), `garage` (optional),
    `auto_upload_target` (optional), `janitor_config`, `janitor_quadlet`,
    `mail_filter` (optional), `mta` (optional).
  - `worker`: `base`, `janitor_source`, `janitor_config`,
    `janitor_quadlet`.
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
tagged with the checkout's commit. The build logs are kept in
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
| `janitor-worker` | `janitor-worker --loop` | `janitor_worker_address`:9821 |

Caddy serves `janitor_external_url` and proxies `/runner/` (without the
prefix) to the runner's public port, `/git/` and `/bzr/` to the stores'
public ports, `/dists/` to the archive, `/ognibuild-dep/` (optional) to
the dependency server and everything else to the site.
No private port is proxied. Workers use only that URL, so a worker host
needs to reach the control host on port 443 (and 80 for the apt
repository), and the control host needs to reach each worker on 9821.

With `janitor_scheme: https` (the default) plain HTTP serves only the apt
repository (`janitor_apt_archive_url`), which the build chroots read
without CA certificates; every other path, worker logins included, is
redirected to HTTPS. `caddy_self_signed: true` (the Vagrant VMs) makes a
self-signed certificate for a control host without a real domain; every
host fetches it from the control host when the playbook runs, adds it to
its CA bundle and mounts that bundle into the worker, publish, runner and
site.

Each unit sets `Entrypoint=` and its whole command line: the images pass
`--listen-address=0.0.0.0` already, and the Rust services refuse a flag
given twice.

The worker builds in an unshare session (`chroot: "unshare:<suite>"` in
janitor.conf) from the image `ogni cache-env` makes, and sbuild runs in
unshare mode from a tarball made with `mmdebstrap`; both are kept under
`janitor_home/data`. The published worker image has no codemods: list the
packages your campaigns need in `janitor_worker_extra_packages` and the
playbook builds a derived image with them.

## Before you start

- Debian 13 hosts, each with an `ansible_user` that can use sudo.
- A DNS name for the control host (`janitor_domain`), with ports 80 and
  443 open to the internet so Caddy can get a certificate for it.
- Time: the first run builds every image from source, 60 to 80 minutes
  in the Vagrant VMs (`use_prebuilt_images: true` pulls them instead).

## Usage

```
cd ansible
# edit inventory.ini and group_vars/all.yml; pass the secrets with -e or ansible-vault
ansible-galaxy collection install -r requirements.yml
ansible-playbook site.yml
```

Set at least `janitor_domain`, `janitor_github_org` and the secrets at
the top of `group_vars/all.yml`. `janitor_github_org` is needed even
without GitHub: it names the archive signing key and janitor.conf's
`origin`.

With GitHub (`breezy_github_token` set), the first run prints the bot's
SSH public key (kept in `janitor_ssh_dir`, apart from the deploy user's
`~/.ssh`); add it to the bot account before publish opens its first
merge proposal.

### Check that it works

On the control host, as `ansible_user`:

```
systemctl --user list-units 'janitor-*'    # every unit active (running)
curl -s http://127.0.0.1:9911/health        # the runner; the ports are in the table above
journalctl --user -u janitor-runner         # a unit's log
```

The site is then at `janitor_external_url`. How codebases and
candidates get in is in [docs/flow.md](../docs/flow.md).

### Upgrades, own campaigns, backups

- Changing `janitor_branch` rebuilds the images but does not migrate the
  database schema.
- `janitor.conf` is a fixed template,
  `ansible/roles/janitor_config/templates/janitor.conf.j2`; edit it for
  your own campaigns.
- Back up PostgreSQL, `janitor_home/data` and `janitor_home/gnupg-archive`,
  with Gitea `gitea_home/secrets`, with Garage `garage_home`, and with
  auto-upload its key in `auto_upload_gnupg_home`.

### Several hosts

Put the hosts in `[control]` and `[worker]`, and set:

- `janitor_domain`: a name every worker can reach the control host on.
- `janitor_worker_address`, per worker host: its own address, which the
  runner pings.
- `janitor_workers`: every worker's `name` and its own `password`; the
  control host registers them all, and each worker uses the entry for its
  `worker_name`. The runner checks only that name and password match, so
  workers that share a password can act as each other.
  Build code runs as root in the worker container and can read that
  worker's password.

### Gitea (optional)

`enable_gitea: true` runs Gitea on the control host, served by Caddy at
`https://<gitea_domain>/` (its own host name: breezy's Gitea plugin only
handles `https://<host>/`). The playbook makes an admin, the bot
(`gitea_bot_user`, whose token publish and the runner use and whose SSH
key publish pushes with), the `gitea_test_users`, the `gitea_test_org`
and copies `gitea_test_repos` into it. `gitea_site_login: true` registers
the site as an OAuth2 application and logs in there; set
`oauth_qa_reviewer_group` and `oauth_admin_group` to Gitea organizations
or teams (`org:team`). Gitea's secrets and tokens are kept in
`gitea_home/secrets`. Proposals to Gitea need breezy 3.4, which is not
released yet: the images on main have breezy 3.3, which has no Gitea
support.

### Auto-upload (optional)

`enable_auto_upload: true` runs `janitor-auto-upload` on the control
host. For every successful Debian build (only `auto_upload_distributions`
if set) it signs the `.changes` with `debsign` as
`auto_upload_debsign_keyid` and uploads it with `dput` to
`auto_upload_dput_host`. Put that key in `auto_upload_gnupg_home`
without a passphrase, and the host's section in `auto_upload_dput_cf`,
which replaces the image's `/etc/dput.cf`. Use a signing-only subkey
there, not the primary key.
`enable_auto_upload_target: true` instead makes a test-only key and a
dput host that copies uploads into `auto_upload_incoming` on the control
host, to try it out.

### ognibuild dependency server (optional)

`enable_ognibuild_dep: true` runs ognibuild's `dep-server` (image
`ognibuild_dep`) on the control host, as nobody with no capabilities on
a read-only root. It has no login: Caddy serves it under
`/ognibuild-dep/` only to `ognibuild_dep_clients` (private addresses by
default; 403 for the rest), and the runner passes that URL
(`--public-dep-server-url`) to generic builds, which are meant to ask it
which Debian package provides a missing dependency. It is reachable but
unused for now: the server cannot resolve anything yet, and janitor
builds ognibuild without its `dep-server` feature, so the worker never
asks it. Leave it off unless you are testing it.

### S3 log storage (optional)

Set `janitor_logs_location` to an S3 endpoint URL to keep build logs in
its bucket `debian-janitor` instead of `janitor_home/data/logs`. The
runner and site sign their requests with `janitor_s3_access_key_id` and
`janitor_s3_secret_access_key`, written to `janitor_home/s3.env` (mode
0600). Artifacts stay on disk: janitor has no S3 artifact storage.
`enable_garage: true` runs Garage, an S3 server, on the control host's
loopback with that bucket and a key of its own, made once in
`garage_home/secrets`, to try it out. Leave the S3 key unset then: a
key passed with `-e` overrides Garage's.

### Mail filter (optional)

`enable_mail_filter: true` builds the `mail_filter` image and installs
`mail_filter_command`, which hands one mail read from stdin to a socket
of the deploy user's systemd; that runs `janitor-mail-filter` on it
(an MTA's own unit may not allow rootless podman). When the mail is a
forge's notification about a proposal, the filter has publish refresh
that proposal's status at once, through `mail_filter_refresh_url`
(publish's own `/refresh-status`; the site's
`/api/refresh-proposal-status` needs a login session). The filter's
container can reach only this host (`mail_filter_network`), runs
read-only as nobody with no capabilities, under memory and time caps.
The command waits for the filter, and at most `mail_filter_max_instances`
run at once: systemd drops any connection beyond that, so a mail can be
lost. Pipe the forge mail for the bot's address into the command from
your MTA, as the deploy user, with fewer deliveries at once than that
(`sieve/README` shows a Dovecot setup).
`enable_mta: true` instead runs Postfix on the control host for
`mta_domains`, taking mail from that host only and sending nothing out:
mail for `mta_mail_filter_recipients` goes into the filter, all of them
together at most half of `mail_filter_max_instances` at once
(`local_destination_concurrency_limit`), the rest to the deploy user's
mailbox (up to `mta_mailbox_size_limit`). With Gitea on, Gitea sends its
mail there.

## Vagrant

Needs vagrant with the libvirt provider. The VMs run the playbook
themselves (`ansible_local`).

```
vagrant up                                         # one VM with everything
VAGRANT_VAGRANTFILE=Vagrantfile.multi-host vagrant up   # control, worker1 and worker2
vagrant rsync && vagrant provision                 # copy changed files in, run the playbook again
vagrant ssh [control|worker1|worker2]
vagrant destroy -f
```

The site is at `https://<control IP>/` (`192.168.56.10` for one VM,
`192.168.60.11` for several), with the self-signed certificate. Both
Vagrantfiles turn on Gitea, at `https://gitea.janitor.test/`: each VM
has an /etc/hosts entry for it, and the machine your browser runs on
needs one too (`<control IP> gitea.janitor.test`), or the site login
cannot reach Gitea. They also turn on auto-upload of lintian-fixes
builds to the local test target, and the mail filter with Postfix,
which gets Gitea's mail for the bot, and keep the build logs in Garage.
With several VMs, each worker gets its own password, made once in
`.vagrant/worker-passwords.json`.

Environment variables:

- `JANITOR_SOURCE`: a local janitor checkout to deploy instead of cloning
  `JANITOR_REPO`; it is rsynced into each VM. `JANITOR_BRANCH` names the
  branch to check out (default `main`).
- `JANITOR_REPO`: the repository to clone (default upstream).
- `JANITOR_USE_PREBUILT` (default `false`): pull the published images
  instead of building `JANITOR_BRANCH`.
- One VM: `JANITOR_VM_CPUS` (8), `JANITOR_VM_MEMORY` (24576 MB),
  `JANITOR_VM_DISK` (200 GB), `JANITOR_VM_IP`.
- Several VMs: `JANITOR_NETWORK` (`192.168.60`), `JANITOR_STORAGE_POOL`
  (`default`); the sizes are in the `MACHINES` table at the top of the file.
