# Deployment

An Ansible playbook that deploys a Janitor instance onto one Debian 13
host. Every service runs as a rootless podman container under systemd,
next to PostgreSQL and Redis. Caddy serves them on the host's public
addresses.

## Before you start

- A Debian 13 host with an `ansible_user` that can use sudo without a
  password (or add `--ask-become-pass`), reachable over SSH with its
  host key in your `known_hosts`.
- A DNS name for it (`janitor_domain`), with ports 80 and 443 open so
  Caddy can get a certificate. For a test host without one, set
  `caddy_self_signed: true`.
- On the machine you run the playbook from: `ansible-core` and the
  collections in `ansible/requirements.yml`.
- The first run builds every image from the checkout.
  `use_prebuilt_images: true` pulls the published images instead.

## Deploying

In `deploy/ansible`:

1. Install the collections:

   ```
   ansible-galaxy collection install -r requirements.yml
   ```

2. Put your host into `inventory.ini`.

3. Write a vars file with at least `janitor_domain` and
   `janitor_github_org`. The defaults and their comments are in
   `group_vars/all.yml`.

4. Write a secrets file outside the repository, encrypted with
   ansible-vault if you like:

   ```
   janitor_db_password: "..."
   worker_password: "..."
   breezy_whoami: "Janitor Bot <bot@example.com>"
   ```

   The optional secrets (a GitHub token, the site login) are listed at
   the top of `group_vars/all.yml`.

5. Run the playbook (add `--ask-vault-pass` for an encrypted file):

   ```
   ansible-playbook site.yml -e @my-instance.yml -e @~/janitor-secrets.yml
   ```

With `breezy_github_token` set, each run prints the bot's SSH public key.
Add it to the bot's GitHub account before the first pull request is
opened.

The packages your campaigns need on the worker go in
`janitor_worker_extra_packages`, for example `[lintian-brush]` for the
lintian-fixes campaign, and your merge proposal templates in
`janitor_proposal_templates`.

## Checking it

As `ansible_user` on the host:

```
systemctl --user list-units 'janitor-*'
curl -s http://127.0.0.1:9911/health
```

The site is at `https://<janitor_domain>/`. To build a codebase, add it
and a candidate to the runner and schedule a run (lintian-fixes needs
`lintian-brush`, see above). The candidate's
`publish-policy` names one of `janitor_publish_policies`: `build-only`
only builds, and `propose` opens a pull request once the run is
approved (see [Review and publishing](../docs/runs.md#review-and-publishing)).

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

## Upgrading

Run the playbook again. It checks out the newest commit of
`janitor_branch`, builds its images (or pulls them) and restarts each
service whose image changed.

The playbook loads the schema only into an empty database.

## Backing up

Running the playbook again makes everything else from your vars. Back
up the database and these directories on the host:

```
sudo -u postgres pg_dump -Fc janitor > janitor.dump
tar -C ~ -czf janitor-files.tar.gz janitor/data janitor/gnupg-archive .config/janitor-ssh
```

`janitor/gnupg-archive` holds the key the apt repository is signed with,
and `.config/janitor-ssh` the bot's SSH key.

## Trying it in a VM

`vagrant up` in `deploy/` makes one VM with the libvirt provider and
runs the playbook in it with the fake secrets of
`ansible/group_vars/vagrant.yml`. The site is then at
`https://192.168.56.10/`, with a self-signed certificate. The
`JANITOR_*` variables at the top of the Vagrantfile pick the checkout or
branch to deploy, pre-built images, and the VM's size and address.
