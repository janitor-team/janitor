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
$ podman run --rm --network=host --name janitor-publish       --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/publish:latest       --config /mnt/janitor/janitor.conf --differ-url http://localhost:9920/ --external-url http://localhost/
$ podman run --rm --network=host --name janitor-runner        --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/runner:latest        --config /mnt/janitor/janitor.conf --public-vcs-location git=http://localhost:9924/git/,bzr=http://localhost:9930/bzr/
$ podman run --rm --network=host --name janitor-site          --volume $( pwd ):/mnt/janitor ghcr.io/jelmer/janitor/site:latest          --config /mnt/janitor/janitor.conf --archiver-url http://localhost:9914/ --differ-url http://localhost:9920/ --external-url http://localhost/ --publisher-url http://localhost:9912/ --runner-url http://localhost:9911/
```

Register the worker first; see [Registering workers](production.md#registering-workers). Set `WORKER_PASSWORD` to its password in your shell; `--env WORKER_PASSWORD` passes it on. The worker talks to the runner's public port, 9919. Without `--loop` it exits after one item, so pass `--loop` to keep it running:

```console
$ podman run --rm --network=host --name janitor-worker \
    --env WORKER_NAME=myworker --env WORKER_PASSWORD \
    ghcr.io/jelmer/janitor/worker:latest \
    --base-url http://localhost:9919/ --loop
```

`janitor-mail-filter` is not a service. It reads one email from standard input
and posts the URL of the merge proposal in it to `--refresh-url`.

**Custom worker tooling** - `janitor-worker` only bundles Debian's `sbuild`/`schroot`/`mmdebstrap`; add anything else your campaigns need by deriving your own image the same way. For example, to add `lintian-brush` for the `lintian-fixes` campaign in `janitor.conf.example`:

```dockerfile
FROM ghcr.io/jelmer/janitor/worker:latest
RUN apt-get update && apt-get install -y lintian-brush && apt-get clean
```

Save it as `Dockerfile` in an empty directory, then build it and run it in place of the published image:

```console
$ podman build -t my-janitor-worker .
$ podman run --rm --network=host --name janitor-worker \
    --env WORKER_NAME=myworker --env WORKER_PASSWORD \
    my-janitor-worker --base-url http://localhost:9919/ --loop
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
