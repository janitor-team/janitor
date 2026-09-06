# Deployment

Ansible playbook for deploying a full janitor-team/janitor instance (all
services, Postgres with the debversion extension, Caddy, rootless
podman/Quadlet), plus a Vagrant VM for exercising the playbook locally
before running it against a real instance. Deploys either onto a single
Debian host, or with the worker split onto its own host talking to the
rest over the network - see [multi-host
deployment](../docs/multi-host-deployment.md) for the second case; the
single-host case below is unaffected either way, and is still what you
get by default.

## Layout

- `ansible/site.yml` - the playbook entry point: a validation play,
  then one deploy play per inventory group (`control`, `worker` - the
  same group for both in the single-host case). Roles run in order:
  `base`, `janitor_source` (clone + build/pull the service images),
  `postgres`, `redis`, `caddy`, `janitor_config`, `janitor_quadlet`
  (systemd user units for every service, plus sbuild/schroot setup inside
  the worker container).
- `ansible/inventory.ini` - fill in the real host/IP, SSH user, and key
  path for the target instance(s). One host in both `[control]` and
  `[worker]` for single-host; two distinct hosts for the split topology.
- `ansible/group_vars/all.yml` - deployment variables and the list of
  required secrets (see the comment block at the top of that file for
  exactly what to supply and how).
- `ansible/group_vars/control.yml` / `worker.yml` - overrides applied
  only to each respective group; only relevant for the split topology,
  see [multi-host deployment](../docs/multi-host-deployment.md).
- `ansible/group_vars/vagrant.yml` - fake, local-dev-only values applied
  only when running against the Vagrant VM below.
- `Vagrantfile` - a local single-host VM that runs the same playbook via
  `ansible_local`, for validating changes without needing a real instance.
  `Vagrantfile.multi-host` does the same for the two-host split topology.

## Usage

Against a real instance:

```
cd ansible
# edit inventory.ini and group_vars/all.yml (or supply -e/--ask-vault-pass)
ansible-galaxy install -r requirements.yml
ansible-playbook site.yml
```

The first run generates an SSH keypair for the bot account and prints its
public key (`roles/janitor_config`'s "Remind the operator to register the
SSH key" task) - add it to the bot account at
https://github.com/settings/keys before publish tries to open or resume
its first merge proposal, or that step will fail.

Real instances default to `use_prebuilt_images: false` (source builds).
`roles/janitor_source`'s image-build task tags each image with the
checkout's commit SHA and skips the rebuild when every expected image
already has a tag for the current `HEAD`, so only a genuine source change
triggers a fresh `make build-all` (`buildah build --no-cache`, several
hours) - a config/template-only re-run just redeploys quadlet units
against the images already built. Set `use_prebuilt_images: true` in
`group_vars/all.yml` (or pass `-e use_prebuilt_images=true`) to always
pull pre-built `ghcr.io` images instead of building from source at all.

Locally, via Vagrant (requires a Vagrant provider - libvirt is what this
Vagrantfile is set up for):

```
vagrant up                              # fast path: pull pre-built images from ghcr.io
JANITOR_USE_PREBUILT=false vagrant up   # slow path: build all images from source (~hours)
vagrant provision                       # re-run the playbook without recreating the VM
vagrant ssh
vagrant destroy
```

For the two-host split topology, use `Vagrantfile.multi-host` instead -
same commands, but `vagrant ssh control` / `vagrant ssh worker` - see
[multi-host deployment](../docs/multi-host-deployment.md) for its own
environment variables.

### Vagrant environment variables

- `JANITOR_USE_PREBUILT` (default `true`) - pull pre-built `ghcr.io`
  images instead of building from source. This VM's purpose is validating
  the playbook itself, not iterating on janitor's own source, so pre-built
  is the right default; switch to `false` only when actually testing a
  change to a Dockerfile or to `janitor_repo`/`janitor_branch`.
- `JANITOR_VM_CPUS` (default `4`) / `JANITOR_VM_MEMORY` (default `4096`,
  or `8192` when building from source) - VM sizing.
- `JANITOR_VM_IP` (default `192.168.56.10`) - the VM's private-network IP.
  Reach the site at `http://{JANITOR_VM_IP}/` (see the comment above the
  `forwarded_port` line in the Vagrantfile for why the forwarded port
  doesn't work under libvirt).
- `JANITOR_REPO` / `JANITOR_BRANCH` (default upstream `main`) - point the
  VM at a fork/branch carrying unmerged fixes, without editing
  `group_vars/all.yml`.
