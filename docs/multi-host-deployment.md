# Multi-host deployment

Running the worker on one or more separate hosts from everything else,
using the [containers](Dockerfiles_.md) directly.

One control host runs every service except `worker`: `runner`, `site`,
`differ`, `git_store`, `publish`, `archive`, `auto_upload` and `bzr_store`,
plus PostgreSQL and Redis. Each worker host runs only the `worker`
container and talks to the control host over the network instead of
`localhost`.

## Control host

Start the services as for a single host, see [containers](Dockerfiles_.md)
and [production](production.md).
The `runner` and `git_store` images already listen on `0.0.0.0`, so they
need no extra listen flag. Two ports must be reachable from the worker
hosts:

- `9919`, the `runner` public port. Workers get work from it and send
  results to it.
- `9924`, the `git_store` public port. It serves the repositories under
  `/git/`.

`janitor-runner` and `janitor-git-store` both read `janitor.conf`
(`--config`). Keep `git_location` there at the `git_store` private port,
`http://localhost:9923`. The services on the control host read it and call
paths (`/<codebase>/diff`, `/<codebase>/revision-info`) that the private
port serves.

The workers get their address from the runner's `--public-vcs-location`
flag, which is required. Set it to `http://<control-host>:9924/`, not
`localhost`. The runner then hands each worker
`http://<control-host>:9924/git/<codebase>` as the URL to push its result
to.

Each worker needs a login. [Registering
workers](production.md#registering-workers) adds the `worker` table row
that the runner checks, and [Database](production.md#database) lists the
ways to give the same name and password to the worker; the command below
uses `WORKER_NAME`/`WORKER_PASSWORD`. None of this changes because the
worker is remote.

The same login is used for pushes. The `git_store` public port lets anyone
fetch (`git-upload-pack`) but asks for HTTP Basic auth on
`git-receive-pack`, checked against the `worker` table. The worker puts
its login into the push URL itself, so there is no separate git credential
to set up.

## Each worker host

```console
$ podman run -d --name janitor-worker --network host \
    --cap-add SYS_ADMIN --cap-add SYS_CHROOT --cap-add SETUID --cap-add SETGID \
    -e WORKER_NAME=<this-worker-name> -e WORKER_PASSWORD=<this-worker-password> \
    ghcr.io/jelmer/janitor/worker:latest \
    --external-address <this-worker-host> \
    --base-url http://<control-host>:9919/ --loop
```

- `--base-url` is the control host's `runner` public port. The worker asks
  it for work. There is no `/runner/` path: the port serves
  `/active-runs` at the top level.
- `--external-address` is the address the runner uses to poll this worker
  for its status, as `http://<address>:9821`. It must resolve, from the
  control host, to this worker. Without an address the runner cannot
  check the worker and ends the run as `worker-disappeared` after three
  failed checks, 30 seconds apart.
- The image already passes `--port=9821` and `--listen-address=0.0.0.0`,
  so they are not given again here.
- The image sets `sbuild` to `unshare` mode. The `--cap-add` flags are
  there for that mode in a rootless container.

Repeat this on each worker host, each with its own `--external-address`,
all pointing at the same control host.
