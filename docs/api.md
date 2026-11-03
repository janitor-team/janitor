# Site API

The site serves an HTTP API under `/api` on its public port
(`--public-port`, 8090 by default). The [quickstart](quickstart.md) shows
its queue, active runs and diff endpoints.

## Run logs

`<run-id>` (`$RUN_ID` below) is a run's `id`.
[Follow a run](quickstart.md#follow-a-run) shows how to find it.

`GET /api/run/<run-id>/logs` lists the names of a finished run's log files
as JSON:

```console
$ curl http://localhost:8090/api/run/$RUN_ID/logs
["build.log", "worker.log", "codemod.log"]
```

`GET /api/run/<run-id>/logs/<name>` returns one of them as plain text:

```console
$ curl http://localhost:8090/api/run/$RUN_ID/logs/build.log
Building using working tree
Building package in normal mode
...
```

Neither needs a login. Both return 404 for an unknown run or a run that is
still in progress, and the second also for a log name that is not in the
list.
