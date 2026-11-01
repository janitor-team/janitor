# Service Map

How the Janitor services talk to each other, to storage and to the outside
world. See also [how work flows](flow.md), the [glossary](glossary.md) and
the [production setup](production.md).

```mermaid
%%{init: {"flowchart": {"nodeSpacing": 20, "rankSpacing": 40}}}%%
flowchart TB
    users(["browsers, API clients"])
    forges(["forges: GitHub, GitLab,<br>Gitea, Launchpad, Salsa"])
    udd(["UDD or apt Sources files"])
    mta(["mail server"])
    mailfilter["mail filter"]
    workers["workers"]
    importers(["importer scripts"])
    proxy(["reverse proxy"])
    dep(["ognibuild dep server"])

    subgraph svc["Janitor services"]
        site["site"]
        runner["runner"]
        publish["publish"]
        vcs["VCS stores:<br>git-store, bzr-store"]
        archive["archive"]
        differ["differ"]
        autoupload["auto-upload"]
    end

    subgraph storage["Storage"]
        pg[("PostgreSQL")]
        redis[("Redis")]
        store[("logs and artifacts")]
    end

    debian(["Debian archive"])
    target(["upload target"])
    prom(["Prometheus"])
    grafana(["Grafana"])
    traces(["trace collector,<br>e.g. Jaeger"])
    oidc(["OAuth2 / OIDC provider"])

    users -->|HTTPS| proxy
    forges -->|"HTTPS webhooks"| proxy
    workers -->|"HTTP(S): runner API,<br>git/bzr, apt, deps"| proxy
    udd -->|"package data"| importers
    importers -->|"HTTP private"| runner
    forges -->|mail| mta
    mta -->|"mail on stdin"| mailfilter
    mailfilter -->|"HTTP: --refresh-url"| proxy

    proxy -->|"HTTP public: /"| site
    proxy -->|"HTTP public: /runner/"| runner
    proxy -->|"HTTP public: /git/, /bzr/"| vcs
    proxy -->|"HTTP: /dists/"| archive
    proxy -->|HTTP| dep

    site -->|"HTTP private"| runner & publish & differ & archive & vcs
    oidc <-->|"OAuth2 login"| site
    publish -->|"HTTP private"| differ
    publish -->|"git/bzr pull"| vcs
    runner -->|"git/bzr pull"| vcs
    runner -->|"HTTP poll"| workers

    publish -->|"git/bzr push,<br>forge API"| forges
    runner -->|"git/bzr pull,<br>forge API"| forges
    workers -->|"git/bzr pull"| forges
    workers -->|"apt fetch"| debian
    autoupload -->|dput| target

    svc -->|SQL| pg
    runner -->|"PUBLISH result,<br>queue, publish-status"| redis
    publish -->|"PUBLISH publish,<br>merge-proposal;<br>SUBSCRIBE<br>publish-status"| redis
    site -->|"SUBSCRIBE publish,<br>merge-proposal,<br>queue, result"| redis
    archive & differ & autoupload -->|"SUBSCRIBE result"| redis
    runner -->|write| store
    site & archive & differ & autoupload -->|read| store

    prom -->|"scrape /metrics"| svc
    grafana -->|PromQL| prom
    runner & archive -->|OTLP| traces
    vcs -->|"OTLP (git-store)"| traces
    site -->|Zipkin| traces
```

Square boxes are Janitor programs, rounded boxes are other software,
cylinders are storage. Public ports sit behind the reverse proxy; private
ports serve the other services and Prometheus. Workers log in to the runner
and the VCS stores with HTTP Basic Auth, checked against the `worker` table.

| Service or store | Ports (public / private) | Talks to | Protocol | Config key or flag |
|---|---|---|---|---|
| site | 8090 / 8080 | runner, publish, differ, archive, VCS stores, OAuth2 / OIDC provider | HTTP, OAuth2 | `oauth2_provider`, `--runner-url`, `--publisher-url`, `--differ-url`, `--archiver-url` |
| runner | 9919 / 9911 | VCS stores, workers, forges | HTTP, git/bzr, forge API | `--public-vcs-location`, `--public-apt-archive-location`, `--public-dep-server-url` |
| publish | - / 9912 | differ, VCS stores, forges | HTTP, git/bzr, forge API | `--differ-url`, `--external-url` |
| archive | 9914 (one port) | - | HTTP | `--dists-directory` |
| differ | - / 9920 | - | HTTP | `--cache-path` |
| git-store, bzr-store | git 9924 / 9923, bzr 9930 / 9929 | - | git or bzr over HTTP | `git_location`, `bzr_location`, `--vcs-path` |
| auto-upload | - / 9933 | upload target | dput | `--dput-host` |
| ognibuild dep server | 9934 (one port) | - | HTTP | the runner's `--public-dep-server-url` |
| mail filter | - | site (`/api/refresh-proposal-status`), which asks publish | HTTP | `--refresh-url` |
| workers | - / 9821 | runner, VCS stores, apt repository (archive, site), dep server, forges, Debian archive | HTTP(S), git/bzr, apt | `--base-url`, `--credentials` |
| PostgreSQL | - | used by every service in "Janitor services"; auto-upload only with `--backfill` | SQL | `database_location` |
| Redis | - | used by site, runner, publish, archive, differ, auto-upload | pub/sub; runner and publish also keep keys | `redis_location` |
| logs and artifacts | - | runner writes; site reads; archive, differ and auto-upload read artifacts | files or `gs://`; S3 for logs | `logs_location`, `artifact_location` |
| Prometheus, trace collector | - | scrapes `/metrics`; gets spans from runner, archive, git-store (OTLP) and site (Zipkin) | HTTP | `zipkin_address` (traces) |
