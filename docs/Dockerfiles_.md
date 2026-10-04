## Containers (`Dockerfiles_*`)

_Stand-alone_

**Pull (Pre-Built)**:

```console
$ podman pull ghcr.io/jelmer/janitor/site:latest
```

**Build**:

```console
$ podman build -t ghcr.io/jelmer/janitor/site:latest -f Dockerfile_site .
$ buildah build -t ghcr.io/jelmer/janitor/site:latest -f Dockerfile_site .
```

**Run**:

```console
$ cp janitor.conf.example janitor.conf  # then edit janitor.conf for your setup
$ podman run --rm --network=host --name janitor-archive       --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/archive:latest       --config /mnt/janitor/janitor.conf --cache-directory /srv/cache --dists-directory /srv/dists
$ podman run --rm --network=host --name janitor-auto-upload   --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/auto_upload:latest   --config /mnt/janitor/janitor.conf
$ podman run --rm --network=host --name janitor-bzr-store     --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/bzr_store:latest     --config /mnt/janitor/janitor.conf --vcs-path /srv/bzr
$ podman run --rm --network=host --name janitor-differ        --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/differ:latest        --config /mnt/janitor/janitor.conf --cache-path /srv/cache
$ podman run --rm --network=host --name janitor-git-store     --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/git_store:latest     --config /mnt/janitor/janitor.conf --vcs-path /srv/git
$ podman run --rm --network=host --name janitor-ognibuild-dep ghcr.io/jelmer/janitor/ognibuild_dep:latest
$ podman run --rm --network=host --name janitor-mail-filter   ghcr.io/jelmer/janitor/mail_filter:latest                                              --refresh-url http://localhost/api/refresh-proposal-status
$ podman run --rm --network=host --name janitor-publish       --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/publish:latest       --config /mnt/janitor/janitor.conf --differ-url http://localhost:9920/ --external-url http://localhost/
$ podman run --rm --network=host --name janitor-runner        --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/runner:latest        --config /mnt/janitor/janitor.conf --public-vcs-location http://localhost:9924/
$ podman run --rm --network=host --name janitor-site          --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/site:latest          --config /mnt/janitor/janitor.conf --archiver-url http://localhost:9914/ --differ-url http://localhost:9920/ --external-url http://localhost/ --publisher-url http://localhost:9912/ --runner-url http://localhost:9911/
$ podman run --rm --network=host --name janitor-worker        ghcr.io/jelmer/janitor/worker:latest                                                   --base-url http://localhost/
```

**Custom worker tooling** - `janitor-worker` only bundles Debian's `sbuild`/`schroot`/`mmdebstrap`; add anything else your campaigns need by deriving your own image the same way:

```dockerfile
FROM ghcr.io/jelmer/janitor/worker:latest
RUN apt-get update && apt-get install -y my-other-build-tool && apt-get clean
```

**sbuild chroot** - the worker image runs `sbuild` in unshare mode, which looks for chroot tarballs in `~/.cache/sbuild` by file name. `janitor-create-sbuild-chroot`, shipped in the image, builds the tarball for a distribution and links it under a name for each campaign built on it, since `sbuild` picks the chroot from the campaign's `build_distribution`:

```console
$ janitor-create-sbuild-chroot --config janitor.conf unstable
```

On a host without `janitor.conf`, name everything on the command line:

```console
$ janitor-create-sbuild-chroot --suite unstable --mirror http://deb.debian.org/debian \
    --chroot unstable-amd64-sbuild --component main --build-distribution lintian-fixes
```

Run it again after adding a campaign; an existing tarball is kept unless `--force` is given. The image tells `sbuild` never to treat the tarball as too old, so run it with `--force` when the chroot should be refreshed. Keep `~/.cache/sbuild` on a volume so the chroot survives a restart.

`mmdebstrap` has to create a user namespace to build the tarball. If it stops with `unshare failed: Operation not permitted`, start the container with `--cap-add SYS_ADMIN`.

For a host that runs `sbuild` in schroot mode, `--mode schroot` creates the chroot with `sbuild-createchroot` and registers it with schroot, with an alias for each campaign. It must run as root, with an absolute `--base-directory` that only root can write to. Outside the worker image it needs `python3-breezy` installed, and `dpkg-dev` unless `--arch` is given. `--remove-old` first removes the chroot and definition from an earlier run, and refuses while a filesystem is mounted in the chroot or schroot has a session open for it; `--run-command` runs a command in the new chroot with `sbuild-shell`, which needs `sbuild` configured for schroot mode; `--dry-run` prints what would be done:

```console
# janitor-create-sbuild-chroot --mode schroot --config janitor.conf \
    --base-directory /srv/chroots --remove-old unstable
```

**Troubleshooting**:

```console
$ podman run -it --entrypoint=/bin/bash --rm -p 8090:8090 -v $( pwd ):/mnt ghcr.io/jelmer/janitor/site:latest
$ podman run \
  --tty \
  --interactive \
  --entrypoint=/bin/bash \
  --rm \
  --publish 8090:8090 \
  --volume $( pwd ):/janitor \
  --workdir /janitor \
  ghcr.io/jelmer/janitor/site:latest
```
