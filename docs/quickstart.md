# Quickstart

This page shows how to define a campaign, add a codebase, a publish
policy and a candidate,
follow the run, and find its logs and diff. It assumes the services and
at least one worker are running (see [production.md](production.md)).
[glossary.md](glossary.md) explains the terms it uses.

## Define a campaign

A campaign is one kind of change that the janitor tries to make across
many codebases: `lintian-fixes`, for example, fixes the problems that
lintian finds in Debian packages. Each campaign has a `campaign` block in
`janitor.conf`, the file the services' `--config` names:

```
campaign {
  name: "lintian-fixes"
  command: "lintian-brush"
  branch_name: "lintian-fixes"
  debian_build {
    base_distribution: "unstable"
    build_distribution: "lintian-fixes"
    build_suffix: "jan+lint"
  }
}
```

`janitor.conf.example` has this campaign and the `unstable`
`distribution` block it uses, so copy both from there. Copy its `control`
campaign too: after each successful run, the runner queues a `control` run
that builds the codebase without the change, so the two builds can be
compared.

`name` is what candidates refer to, and `branch_name` is the name of the
branch that merge proposals come from.

`command` is the codemod script, which makes the change. Workers look it
up on their `PATH` and run it in a checkout of the codebase, so it has to
be installed in the worker image. The published worker image has no
codemods, so build your own, as [Worker images](production.md#worker-images)
shows. The script has to follow silver-platter's
[codemod protocol](https://github.com/jelmer/silver-platter/blob/master/codemod-protocol.md):
change the files in the checkout and exit with status 0. It can also
describe the change in the JSON file that `$SVP_RESULT` names.

With `debian_build`, the worker builds the changed code as a Debian
package. `base_distribution` names a `distribution` block in
`janitor.conf`. That block's `build_command` runs the build, unless
`debian_build` sets its own. The package is built for
`build_distribution`, with `build_suffix` added to its version.
`build_distribution` is only a name for this campaign's builds and needs
no `distribution` block of its own. It does need an sbuild chroot, so
after adding the campaign, run `create-sbuild-chroot-unshare.py` again
as [sbuild chroots](production.md#sbuild-chroots) shows.

For code that is not a Debian package, use `generic_build {}` in place of
`debian_build`. The worker then finds the code's build system, such as a
`Makefile`, builds the code and runs its tests. A generic build needs no
`distribution` block or sbuild chroot.

Merge proposals take their description from a template named after the
campaign, such as `lintian-fixes.md`. Without one, the description is
`Changes for lintian-fixes (<run id>)`.

The services read `janitor.conf` when they start, so restart them after
adding a campaign.
[Adding a new campaign](production.md#adding-a-new-campaign) shows how to
write the codemod script and the templates, and how to find candidates.

## Add a codebase, a publish policy and a candidate

A codebase is code to work on, usually a branch in version control; a
candidate pairs a codebase with a campaign to run on it. Both are uploaded
to the runner on its private port (its `--port`, 9911 by default), which is
meant for use inside the deployment only.

First the codebase:

```console
$ curl -X POST http://localhost:9911/codebases \
    -H 'Content-Type: application/json' \
    -d '[{"name": "my-project", "branch_url": "https://salsa.debian.org/example/my-project.git", "vcs_type": "git"}]'
{}
```

The upload can also set:

- `subpath`: the directory, relative to the root of the branch, that holds
  the code, when it is not the whole branch.
- `web_url`: a page where people can browse the code.
- `value`: how much work on this codebase is worth compared with the
  others. A higher value moves its runs up the queue.

An upload replaces the stored codebase of the same `name` and clears any
field that is left out, so send every field you want to keep.

Then a publish policy. Its `mode` says whether the publisher pushes a
successful run's change (`push`), opens a merge proposal for it
(`propose`) or leaves it unpublished (`build-only`). A candidate names a
policy; without one, its runs are not published. The publisher keeps the
policies. On its port (its `--port`, 9912 by default), the first command
adds or replaces the policy named `propose` and the second lists all
policies:

```console
$ curl -X PUT http://localhost:9912/policy/propose \
    -H 'Content-Type: application/json' \
    -d '{"per_branch": {"main": {"mode": "propose"}}}'
{}
$ curl http://localhost:9912/policy
```

`per_branch` is keyed by branch role: `main` is the run's main result
branch, not the name of a branch in the codebase.

Finally the candidate, for the `lintian-fixes` campaign:

```console
$ curl -X POST http://localhost:9911/candidates \
    -H 'Content-Type: application/json' \
    -d '[{"codebase": "my-project", "campaign": "lintian-fixes", "publish-policy": "propose"}]'
```

A candidate can also set its own `command`; without one, the campaign's
command is used. As with codebases, an upload replaces the stored candidate
for the same `codebase` and `campaign` and clears any field that is left
out. The response lists what happened to each entry; `queue-id` is the id
of the queue entry the upload added:

```json
{
  "invalid_command": [],
  "invalid_value": [],
  "success": [
    {
      "bucket": "default",
      "campaign": "lintian-fixes",
      "change_set": null,
      "codebase": "my-project",
      "estimated_duration": 9,
      "offset": 285637.44327272725,
      "queue-id": 535,
      "refresh": false
    }
  ],
  "unknown_campaigns": [],
  "unknown_codebases": [],
  "unknown_publish_policies": []
}
```

A codebase, campaign or publish policy the runner does not know comes back
in `unknown_codebases`, `unknown_campaigns` or `unknown_publish_policies`,
and nothing is stored for that entry.

## Follow the queue

The site's public port (its `--public-port`, 8090 by default) lists the
queue:

```console
$ curl http://localhost:8090/api/queue
[{"campaign": "lintian-fixes", "codebase": "my-project", "command": "lintian-brush", "context": null, "queue_id": 535}]
```

An entry stays in the queue while a worker runs it, and leaves it once the
run has finished. The `/cupboard/queue` page shows the same queue and the
runs in progress.

## Follow a run

`/api/active-runs` lists the runs in progress:

```console
$ curl http://localhost:8090/api/active-runs
```

Each entry has the run's `id`, its `queue_id` (the `queue-id` from the
candidate upload), `codebase`, `campaign`, `command`, `worker`,
`start_time`, `estimated_duration` and `current_duration`. Copy the run's
`id` from that list into `RUN_ID`; the commands below use it. While the
run is in progress, the second command shows just that run:

```console
$ RUN_ID=571371fa-3b7b-45e3-84d2-8201b3bfe8ff
$ curl http://localhost:8090/api/active-runs/$RUN_ID
```

Once the run has finished, it leaves this list. The
`/cupboard/history` page then lists it among the latest finished runs, each
with a link to its page; take the `id` from that link if you missed it.

## Find the logs and diff

The run's page is the codebase name and the run's `id` under
`/cupboard/c/`, for example
`/cupboard/c/my-project/571371fa-3b7b-45e3-84d2-8201b3bfe8ff/`. Open it in
a browser to see the run's [result](runs.md#how-a-run-ends), its logs and, if there is one, its
diff. Each log file is also served on its own, for example `worker.log`:

```console
$ curl http://localhost:8090/cupboard/c/my-project/$RUN_ID/worker.log
```

The diff of a successful run is served too, as a plain diff with this
`Accept` header and as HTML without it:

```console
$ curl -H 'Accept: text/x-diff' http://localhost:8090/api/run/$RUN_ID/diff
```

## What happens next

The publisher handles each successful run as the candidate's publish policy
says. [scheduling.md](scheduling.md) describes how the janitor schedules
more runs by itself.
