# Multi-host deployment

How to run the worker on one or more hosts separate from the other
services, using the [containers](containers.md) directly.

One control host runs every service except `worker`, plus PostgreSQL and
Redis. Each worker host runs only the `worker` container and talks to the
control host over the network instead of `localhost`.

## Control host

Start the services as for a single host, see [containers](containers.md)
and [production](production.md).

Set the runner's `--public-vcs-location` as in
[containers](containers.md#run), with the control host's name in place of
`localhost`:
`git=http://<control-host>:9924/git/,bzr=http://<control-host>:9930/bzr/`.
The runner then hands each worker
`http://<control-host>:9924/git/<codebase>` as the URL to push a Git result
to. Keep `git_location` in `janitor.conf` at the `git_store` private port,
`http://localhost:9923`, for the services on the control host.

If the runner gets `--public-apt-archive-location` or
`--public-dep-server-url`, it hands those URLs to the workers too, so they
also need a host name the worker hosts can resolve and reach.

Register each worker as in [Registering workers](production.md#registering-workers).
The worker pushes its results with the same login, and only sends it to the
host in its `--base-url`, so use the same `<control-host>` name in
`--public-vcs-location`.

## Network

| From | To | Port | For |
| --- | --- | --- | --- |
| worker hosts | control host | `9919` | `runner` public port |
| worker hosts | control host | `9924` | `git_store` public port |
| worker hosts | control host | `9930` | `bzr_store` public port |
| control host | each worker host | `9821` | `worker` port, for `runner` status checks |

Workers do not use PostgreSQL or Redis. Anyone who can reach `9924` or
`9930` can fetch the repositories.

Over plain `http://` the worker login is sent in clear text. Keep the
traffic on a private network or VPN, or put TLS in front of `9919`,
`9924` and `9930` and use `https://` in `--base-url` and
`--public-vcs-location`. With TLS in front of the worker's `9821` too, pass
its `https://` URL as `--my-url`.

## Each worker host

Run the worker as in [containers](containers.md#run), with
`WORKER_PASSWORD` set to this worker's password, the control host in
`--base-url` and the name the control host reaches this host by in
`--external-address`:

```console
$ podman run --rm --network=host --name janitor-worker \
    --env WORKER_NAME=<this-worker-name> --env WORKER_PASSWORD \
    ghcr.io/jelmer/janitor/worker:latest \
    --external-address <this-worker-host> \
    --base-url http://<control-host>:9919/ --loop
```

Repeat this on each worker host, each with its own `--external-address`,
all pointing at the same control host.
