# Multi-host deployment

Running the worker on one or more separate hosts from everything else,
using the [containers](production.md) directly.

One control host runs everything except the worker - `runner`, `site`,
`postgres`, `redis`, `differ`, `git_store`, `publish`, `archive`,
`auto_upload`, `bzr_store`, `caddy` - the normal single-host set, minus
`worker`. Any number of separate worker hosts (one, two, ten - however many
you need) each run just the `worker` container, talking back to the control
host over the network instead of `localhost`.

## Control host

`runner` and `git_store` both default to binding `localhost`, since a
co-located worker reaches them there directly. A remote worker needs both
bound to an address it can actually reach instead:

```console
$ podman run -d --name janitor-runner --network host \
    -v /path/to/janitor:/mnt/janitor:rw \
    ghcr.io/jelmer/janitor/runner:latest \
    --listen-address 0.0.0.0 --port=9911 --public-port=9919 \
    --config /mnt/janitor/janitor.conf \
    --public-vcs-location=http://<control-host>:9924/

$ podman run -d --name janitor-git-store --network host \
    -v /path/to/janitor:/mnt/janitor:rw \
    ghcr.io/jelmer/janitor/git_store:latest \
    --listen-address 0.0.0.0 --port=9923 --public-port=9924 \
    --config /mnt/janitor/janitor.conf --vcs-path /mnt/janitor/data/git
```

`--public-vcs-location` is what every worker fetches VCS content from
during a build (`git_store`'s own public port) - it needs to be the control
host's real, externally-reachable address, not loopback. Everything else
(`site`, `publish`, `differ`, `archive`, `postgres`, `redis`, `caddy`,
`auto_upload`, `bzr_store`) runs exactly as it would for a single host.

Each worker also needs its own set of credentials - see [registering
workers](production.md#registering-workers) for both halves of that (the
`worker` table row the runner checks, and the `WORKER_NAME`/
`WORKER_PASSWORD` the worker container itself needs, used below). Nothing
about either half changes because the worker is remote; it's the same
mechanism a single host uses.

Those same credentials also authenticate the push side of
`--public-vcs-location`: `git_store`'s public port accepts anonymous
`git-upload-pack` (fetches) but requires HTTP Basic-auth on
`git-receive-pack`, checked against the `worker` table row above. The
worker embeds `WORKER_NAME`/`WORKER_PASSWORD` into the push URL it hands
to breezy automatically, so there's no separate git credential to set up
for a remote worker to push its result branch back.

## Each worker host

```console
$ podman run -d --name janitor-worker --network host \
    --cap-add SYS_ADMIN --cap-add SYS_CHROOT --cap-add SETUID --cap-add SETGID \
    -e WORKER_NAME=<this-worker-name> -e WORKER_PASSWORD=<this-worker-password> \
    -v ~/.config/breezy:/root/.config/breezy:ro \
    ghcr.io/jelmer/janitor/worker:latest \
    --port=9821 --listen-address 0.0.0.0 \
    --external-address <this-worker-host> \
    --base-url http://<control-host>:9919/runner/ --loop
```

- `--base-url` points at the control host's runner public port - this is
  how the worker finds work to do.
- `--external-address` is what the worker tells the runner to use for its
  keepalive backchannel pings - it needs to resolve, from the control
  host, back to this worker. Without it, the runner's keepalive watchdog
  can only fall back to a 24h staleness check instead of the configured
  run timeout, so a worker that silently drops a job blocks that queue
  slot for up to a day before being reaped and requeued.
- `--listen-address` is what the worker's own web server binds to, for
  those backchannel pings to actually land - `0.0.0.0` for a remote
  worker, same reasoning as the control host's `runner`/`git_store` above.
- The four `--cap-add` flags are what `sbuild`'s unshare-chroot mode needs
  to work unprivileged inside a rootless container - without them it fails
  at the "create chroot session" stage.

Repeat this for as many worker hosts as needed - each one just needs its
own credentials and its own `--external-address`, all pointing at the same
control host.
