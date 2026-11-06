# Service map

How the Janitor services talk to each other, to storage and to the outside
world. See also [how work is scheduled](scheduling.md), the
[glossary](glossary.md) and the [production setup](production.md).

```mermaid
flowchart TB
    users(["browsers, git, apt and API clients"])
    workers["workers"]

    subgraph svc["Janitor services"]
        site["site"]
        runner["runner"]
        publish["publish"]
        differ["differ"]
        subgraph vcs["VCS stores"]
            gitstore["git-store"]
            bzrstore["bzr-store"]
        end
        archive["archive (optional)"]
        autoupload["auto-upload (optional)"]
    end

    storage[("PostgreSQL, Redis,<br>logs and artifacts")]

    forges(["forges"])
    target(["upload target"])

    users --> site & vcs
    users -.-> archive
    workers <--> runner
    workers --> vcs & forges
    workers -.-> archive
    site --> runner & publish & differ & vcs
    site -.-> archive
    runner --> vcs & forges
    publish --> differ & vcs & forges
    autoupload -.-> target
    svc --> storage
    forges -->|webhooks| site
```

An arrow points from the program that opens the connection to the one that
answers. Square boxes are Janitor programs, rounded boxes are other software,
the cylinder is storage; dashed lines lead to or from optional services. The
storage arrow is a summary: the table below says which service uses which
store, and also lists the parts the diagram leaves out. Public ports face
users, workers and forges; private ports serve the other services and
Prometheus. Workers log in to the runner and the VCS stores with HTTP Basic
Auth, checked against the `worker` table.

| Service or store | Ports (public / private) | Talks to | Protocol | Config key or flag |
|---|---|---|---|---|
| site | 8090 / 8080 | runner, publish, differ, archive, VCS stores, OAuth2 / OIDC provider | HTTP, OAuth2 | `oauth2_provider`, `--runner-url`, `--publisher-url`, `--differ-url`, `--archiver-url` |
| runner | 9919 / 9911 | VCS stores, workers, forges | HTTP, git/bzr, forge API | `--public-vcs-location`, `--public-apt-archive-location`, `--public-dep-server-url` |
| publish | - / 9912 | differ, VCS stores, forges | HTTP, git/bzr, forge API | `--differ-url`, `--external-url` |
| archive (optional) | 9914 (one port) | - | HTTP | `--dists-directory` |
| differ | - / 9920 | - | HTTP | `--cache-path` |
| git-store | 9924 / 9923 | - | git over HTTP | `git_location`, `--vcs-path` |
| bzr-store | 9930 / 9929 | - | bzr over HTTP | `bzr_location`, `--vcs-path` |
| auto-upload (optional) | - / 9933 | upload target | dput | `--dput-host` |
| ognibuild dep server | 9934 (one port) | - | HTTP | the runner's `--public-dep-server-url` |
| mail filter | - | site (`/api/refresh-proposal-status`), which asks publish | HTTP | `--refresh-url` |
| workers | - / 9821 | runner, VCS stores, apt repository (archive), dep server, forges, Debian archive | HTTP(S), git/bzr, apt | `--base-url`, `--credentials` |
| PostgreSQL | - | used by every service in "Janitor services"; auto-upload only with `--backfill` | SQL | `database_location` |
| Redis | - | used by site, runner, publish, archive, differ, auto-upload | pub/sub; runner and publish also keep keys | `redis_location` |
| logs and artifacts | - | runner writes; site reads; archive, differ and auto-upload read artifacts | files, `gs://` or `s3://` | `logs_location`, `artifact_location` |
| Prometheus, trace collector | - | scrapes `/metrics`; gets spans from runner, archive, git-store (OTLP) and site (Zipkin) | HTTP | `zipkin_address` (traces) |
