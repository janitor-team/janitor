# Containers

Each service has a `Dockerfile_<service>` at the top of the repository, and a
matching image, `ghcr.io/jelmer/janitor/<service>`.

Before you start, set up PostgreSQL and Redis, see
[Running Janitor in production](production.md).

## Pull (pre-built)

```console
$ podman pull ghcr.io/jelmer/janitor/site:latest
```

Repeat this for each image you need. In production, pin a commit id tag
instead of `latest`, see [Upgrading](production.md#upgrading).

## Build

```console
$ podman build -t ghcr.io/jelmer/janitor/site:latest \
    -f Dockerfile_site .
$ buildah build -t ghcr.io/jelmer/janitor/site:latest \
    -f Dockerfile_site .
```

`make build-all` builds all the images.

## Run

```console
$ cp janitor.conf.example janitor.conf
$ $EDITOR janitor.conf  # set it up for your instance
```

In `janitor.conf`, set `artifact_location` and `logs_location` to directories
under `/mnt/janitor` (the current directory, mounted in each container), or to
GCS or S3, so that all services see the same files.

The commands below use host networking for a quick local setup, and talk to
each service on its own port. Every image listens on all interfaces, and the
private ports have no authentication: firewall them, see
[Services and ports](production.md#services-and-ports). Named volumes keep the
repositories and the archive when a container is removed.

```console
$ podman run -d --rm --network=host --name janitor-git-store \
    --volume $( pwd ):/mnt/janitor --volume janitor-git:/srv/git \
    ghcr.io/jelmer/janitor/git_store:latest \
    --config /mnt/janitor/janitor.conf --vcs-path /srv/git
$ podman run -d --rm --network=host --name janitor-bzr-store \
    --volume $( pwd ):/mnt/janitor --volume janitor-bzr:/srv/bzr \
    ghcr.io/jelmer/janitor/bzr_store:latest \
    --config /mnt/janitor/janitor.conf --vcs-path /srv/bzr
$ podman run -d --rm --network=host --name janitor-differ \
    --volume $( pwd ):/mnt/janitor \
    ghcr.io/jelmer/janitor/differ:latest \
    --config /mnt/janitor/janitor.conf --cache-path /srv/cache
$ podman run -d --rm --network=host --name janitor-runner \
    --volume $( pwd ):/mnt/janitor \
    --volume ~/.config/breezy:/root/.config/breezy:ro \
    ghcr.io/jelmer/janitor/runner:latest \
    --config /mnt/janitor/janitor.conf \
    --public-vcs-location http://localhost:9924/
$ podman run -d --rm --network=host --name janitor-publish \
    --volume $( pwd ):/mnt/janitor \
    --volume ~/.config/breezy:/root/.config/breezy:ro \
    ghcr.io/jelmer/janitor/publish:latest \
    --config /mnt/janitor/janitor.conf \
    --differ-url http://localhost:9920/ \
    --external-url http://localhost:8090/
$ podman run -d --rm --network=host --name janitor-site \
    --volume $( pwd ):/mnt/janitor \
    ghcr.io/jelmer/janitor/site:latest \
    --config /mnt/janitor/janitor.conf \
    --external-url http://localhost:8090/ \
    --archiver-url http://localhost:9914/ \
    --differ-url http://localhost:9920/ \
    --publisher-url http://localhost:9912/ \
    --runner-url http://localhost:9911/
```

Now register the worker, see
[Registering workers](production.md#registering-workers), then start it:

```console
$ podman run -d --rm --network=host --name janitor-worker \
    --env WORKER_NAME=myworker --env WORKER_PASSWORD=mypassword \
    ghcr.io/jelmer/janitor/worker:latest \
    --base-url http://localhost:9919/ \
    --loop
```

Then open `http://localhost:8090/`. Without `--loop`, the worker would process
one item (or wait 5 seconds on an empty queue) and exit.

Optional services (the archiver and the auto-uploader are Debian-specific, and
the dependency server helps workers find packages for missing build
dependencies in generic builds):

```console
$ podman run -d --rm --network=host --name janitor-archive \
    --volume $( pwd ):/mnt/janitor --volume janitor-dists:/srv/dists \
    ghcr.io/jelmer/janitor/archive:latest \
    --config /mnt/janitor/janitor.conf \
    --cache-directory /srv/cache --dists-directory /srv/dists
$ podman run -d --rm --network=host --name janitor-auto-upload \
    --volume $( pwd ):/mnt/janitor \
    ghcr.io/jelmer/janitor/auto_upload:latest \
    --config /mnt/janitor/janitor.conf
$ podman run -d --rm --network=host --name janitor-ognibuild-dep \
    ghcr.io/jelmer/janitor/ognibuild_dep:latest
```

To use the dependency server, add
`--public-dep-server-url http://localhost:9934/` to the runner command. The
runner hands that URL to the workers.

`janitor-mail-filter` is not a service. It reads one e-mail on standard
input, and asks the site to refresh the merge proposal the mail is about. It
is meant to run from your mail setup for each incoming mail:

```console
$ podman run -i --rm --network=host \
    ghcr.io/jelmer/janitor/mail_filter:latest \
    --refresh-url http://localhost:8090/api/refresh-proposal-status \
    < message.eml
```

Today that request fails: the filter sends JSON without a login, and the site
wants a logged-in user and a form body.

(custom-worker-tooling)=

## Custom worker tooling

The worker image bundles Debian's build tooling: `sbuild` (set to unshare
mode, which needs user namespaces in the container runtime), `schroot`,
`mmdebstrap`, `lintian`, `dpkg-dev` and `brz-debian`. To add anything else your
campaigns need, derive your own image from it. For example, to add
`lintian-brush` for the `lintian-fixes` campaign in `janitor.conf.example`:

```dockerfile
FROM ghcr.io/jelmer/janitor/worker:latest
RUN apt-get update \
 && apt-get install -y lintian-brush \
 && apt-get clean
```

Save it as `Dockerfile` in an empty directory, then build it and run it in
place of the published image:

```console
$ podman build -t my-janitor-worker .
$ podman run --rm --network=host --name janitor-worker my-janitor-worker \
    --base-url http://localhost/
```

## Troubleshooting

To get a shell in an image:

```console
$ podman run \
    --tty \
    --interactive \
    --entrypoint=/bin/bash \
    --rm \
    --volume $( pwd ):/janitor \
    --workdir /janitor \
    ghcr.io/jelmer/janitor/site:latest
```
