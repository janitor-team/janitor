# Deployment

An Ansible playbook that deploys a janitor instance: every service as a
rootless podman container under systemd (quadlet), PostgreSQL with the
`debversion` extension, Redis and Caddy. It deploys onto one Debian 13
host or onto several, and comes with Vagrant VMs to try it locally.

## Layout

- `ansible/site.yml`: a play that checks the secrets, then one play per
  inventory group:
  - `control`: `base`, `janitor_source`, `postgres`, `redis`, `caddy`,
    `archive_signing_key`, `gitea` (optional), `janitor_config`,
    `janitor_quadlet`.
  - `worker`: `base`, `worker_firewall`, `janitor_source`,
    `janitor_config`, `janitor_quadlet`.
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
| `janitor-worker` | `janitor-worker --loop` | `janitor_worker_address`:9821 |

Caddy serves `janitor_external_url` and proxies `/runner/` (without the
prefix) to the runner's public port, `/git/` and `/bzr/` to the stores'
public ports, `/dists/` to the archive and everything else to the site.
No private port is proxied. Workers use only that URL, so a worker host
needs to reach the control host on port 443 (and 80 for the apt
repository), and the control host needs to reach each worker on 9821.
On a host only in `[worker]` an nftables table, `janitor_worker`, lets
only the control host and the host itself reach 9821
(`enable_worker_firewall`, `worker_firewall_sources`).

With `janitor_scheme: https` (the default) plain HTTP serves only the apt
repository (`janitor_apt_archive_url`), which the build chroots read
without CA certificates; every other path, worker logins included, is
redirected to HTTPS. `caddy_self_signed: true` (the Vagrant VMs) makes a
self-signed certificate for a control host without a real domain; every
host fetches it from the control host when the playbook runs, adds it to
its CA bundle and mounts that bundle into the worker, publish, runner and
site.

With every page, file and redirect, Caddy's public sites send
`X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy` and,
unless the service sets its own, a `Content-Security-Policy` against
framing by other sites (inline scripts still run); they drop the
`Server` header. Over HTTPS they also send `Strict-Transport-Security`
(`caddy_hsts_max_age`).

Each unit sets `Entrypoint=` and its whole command line.
Every unit but the worker's, whose image has its own init, sets
`RunInit=true`.

The worker builds in an unshare session (`chroot: "unshare:<suite>"` in
janitor.conf) from the image `ogni cache-env` makes, and sbuild runs in
unshare mode from a tarball made with `mmdebstrap`; both are kept under
`janitor_home/data`. A user timer (`janitor-sbuild-chroot.timer`)
remakes the tarball every Monday and Thursday, and a failed run retries
hourly.
The published worker image has no codemods: list the packages your
campaigns need in `janitor_worker_extra_packages` and the playbook
builds a derived image with them.

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
the top of `group_vars/all.yml`. `janitor_github_org` names the archive
signing key and janitor.conf's `origin`.

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

- Changing `janitor_branch` rebuilds the images. The schema is loaded
  into an empty database; schema changes from a new `janitor_branch` are
  applied by hand.
- `janitor.conf` is a fixed template,
  `ansible/roles/janitor_config/templates/janitor.conf.j2`; edit it for
  your own campaigns.
- Back up PostgreSQL, `janitor_home/data` and `janitor_home/gnupg-archive`,
  and with Gitea `gitea_home/secrets`.

### Several hosts

Put the hosts in `[control]` and `[worker]`, and set:

- `janitor_domain`: a name every worker can reach the control host on.
- `janitor_worker_address`, per worker host: its own address, which the
  runner pings.
- `janitor_workers`: every worker's `name` and its own `password`; the
  control host registers them all, and each worker uses the entry for its
  `worker_name`.

### Gitea (optional)

`enable_gitea: true` runs Gitea on the control host, served by Caddy at
`https://<gitea_domain>/` (its own host name). The playbook makes an
admin, the bot
(`gitea_bot_user`, whose token publish and the runner use and whose SSH
key publish pushes with), the `gitea_test_users`, the `gitea_test_org`
and copies `gitea_test_repos` into it. Gitea's SSH server listens on
127.0.0.1:`gitea_ssh_port` only, for publish; people clone over HTTPS.
`gitea_site_login: true` registers
the site as an OAuth2 application and logs in there; set
`oauth_qa_reviewer_group` and `oauth_admin_group` to Gitea organizations
or teams (`org:team`). Gitea's secrets and tokens are kept in
`gitea_home/secrets`.

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
cannot reach Gitea. With several VMs, each worker gets its own password,
made once in `.vagrant/worker-passwords.json`.

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
