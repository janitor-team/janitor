# Screenshot tour

This is a showcase of Janitor, seeded with various Debian packages.

- [Campaign pages](#campaign-pages)
  - [Home](#home)
  - [lintian-fixes campaign](#lintian-fixes-campaign)
- [Cupboard](#cupboard)
  - [Cupboard index](#cupboard-index)
  - [Queue](#queue)
  - [Workers](#workers)
  - [History](#history)
  - [Result codes](#result-codes)
  - [Failure stages](#failure-stages)
  - [Ready to publish](#ready-to-publish)
  - [Review](#review)
  - [Rejected runs during review](#rejected-runs-during-review)
  - [Done](#done)
  - [Publish history](#publish-history)
  - [Merge proposals](#merge-proposals)
  - [Broken merge proposals](#broken-merge-proposals)
  - [Changesets](#changesets)
  - [Never processed](#never-processed)
  - [Reprocess logs](#reprocess-logs)
  - [Run detail](#run-detail)
  - [A codebase's most recent run](#a-codebases-most-recent-run)
  - [Evaluate](#evaluate)
- [Setting up an instance](#setting-up-an-instance)

## Terms used on this page

See [glossary.md](glossary.md) for definitions of codebase, campaign,
candidate, run, cupboard, runner, worker, and merge proposal.

## Campaign pages

### Home
`/`

The root page lists the campaigns this instance runs. `lintian-fixes` is
just this instance's default example campaign, not the only kind janitor
supports; see [`janitor.conf`](../janitor.conf.example) in the
repository root for the `campaign { ... }` block shape used to define
additional ones.

The page also links to `/cupboard/` for the internal status pages covered
below.

![Home page listing the lintian-fixes campaign and a link to the cupboard](images/tour/home.png)

---

### lintian-fixes campaign
`/lintian-fixes/`

Each campaign gets its own top-level page grouping the same kind of
status views cupboard exposes, scoped to that one campaign. This
instance runs a single campaign, `lintian-fixes`, backed by the
`lintian-brush` command.

![lintian-fixes campaign page linking to its merge proposals, ready changes, done changes, and candidates lists](images/tour/campaign-lintian-fixes.png)

`/lintian-fixes/merge-proposals` is the per-campaign equivalent of
`/cupboard/merge-proposals` below, scoped to just this campaign's merge
proposals.

![lintian-fixes merge proposal status page](images/tour/campaign-merge-proposals.png)

## Cupboard

### Cupboard index
`/cupboard/`

`/cupboard/` is the entry point for the internal status and
administrative pages covered in the rest of this tour: queue, history,
workers, result codes, failure stages, ready, review, publish history,
merge proposals, broken merge proposals, and changesets.

![Cupboard index page listing links to every internal status and administrative page](images/tour/cupboard.png)

---

### Queue
`/cupboard/queue`

The queue lists work waiting to be picked up by a worker, ordered by
priority, plus what is currently being processed. `bsdgames` and `lolcat`
sit at the top here. The page also has a "Restrictions" section listing
any host currently subject to rate limiting. [scheduling](flow.md#scheduling)
covers the factors behind that ordering.

![Queue page listing codebases waiting to be processed, headed by bsdgames and lolcat](images/tour/queue.png)

---

### Workers
`/cupboard/workers`

The workers page lists every worker currently registered with the
runner, with a pie chart and table of total runs per worker. worker-1
and worker-2 both appear here, actively picking up assignments from the
queue. See [design](../README.md#design) for what each of the
runner, worker, and other permanently running components does.

![Workers page showing worker-1 and worker-2 registered, each with tens of thousands of runs](images/tour/workers.png)

---

### History
`/cupboard/history`

The history page lists finished runs, most recent first, with each run's
codebase, suite, worker, duration, and result.

![History page listing the most recent runs, including bsdgames and lolcat](images/tour/history.png)

---

### Result codes
`/cupboard/result-codes/`

Runs grouped by their result code, with a chart plus a table, filterable
by campaign and by whether to include transient, historical, and
never-processed entries.

![Result Codes page with a pie chart and table breaking down runs by result code](images/tour/result-codes.png)

Clicking a code in the table opens a filtered list of the codebases
recorded with that code; `/cupboard/result-codes/branch-missing` here
lists `gcolor2` and `witty`.

![Result code drill-down for branch-missing listing gcolor2 and witty](images/tour/result-code-branch-missing.png)

---

### Failure stages
`/cupboard/failure-stages/`

Runs grouped by the pipeline stage at which they failed, useful for
seeing where processing tends to break down across a campaign's history.

![Failure Stages page with a pie chart and table breaking down runs by pipeline stage](images/tour/failure-stages.png)

---

### Ready to publish
`/cupboard/ready`

Lists runs across all campaigns with changes ready to publish, the queue
a maintainer works from before pushing a change or opening a merge
proposal.

![Ready to publish page](images/tour/ready.png)

---

### Review
`/cupboard/review`

Lists runs awaiting a manual publish decision, used when a campaign's
publish mode calls for human review before a change goes out.

![Review page](images/tour/review.png)

---

### Rejected runs during review
`/cupboard/rejected`

Lists runs whose changes were rejected during manual review, kept
separate from the main review queue as a record of past decisions.

![Rejected runs during review page](images/tour/rejected.png)

---

### Done
`/cupboard/done`

The done page lists changes already merged or pushed to their target
branch, the record of a campaign's completed output.

![Done page listing changes merged or pushed to their target branch](images/tour/done.png)

---

### Publish history
`/cupboard/publish/`

Tracks the last 100 push or merge-proposal actions the instance has
taken.

![Publish History page](images/tour/publish.png)

---

### Merge proposals
`/cupboard/merge-proposals`

Status of merge proposals opened across all campaigns, tracking each one
from creation through review to merge or close.

![Merge Proposal Status page](images/tour/merge-proposals.png)

---

### Broken merge proposals
`/cupboard/broken-merge-proposals`

Lists open merge proposals whose most recent run failed, surfacing
proposals that may need another look before they can merge.

![Merge Proposals With Broken Runs page](images/tour/broken-merge-proposals.png)

---

### Changesets
`/cupboard/cs/`

Changesets group related runs across multiple codebases for a
coordinated migration that spans more than one repository.

![Changesets page](images/tour/changesets.png)

---

### Never processed
`/cupboard/never-processed`

Lists codebases queued as candidates for a campaign but never actually
run, useful for spotting candidates stuck behind higher-priority work.
See [candidates](flow.md#candidates) for how those candidates get
created.

![Never Processed page](images/tour/never-processed.png)

---

### Reprocess logs
`/cupboard/reprocess-logs`

An administrative form for re-running log processing across past runs,
with dry-run and reschedule options.

![Reprocess Logs admin page with Dry Run and Reschedule checkboxes](images/tour/reprocess-logs.png)

---

### Run detail
`/cupboard/c/bsdgames/{run_id}/`

A single run's page: which codebase and change set it ran, when it
started, its duration, and its result, along with a predicted success
probability drawn from the campaign's run history. Once a run produces
output, this page also links to its log and diff.

![Run detail page for a single bsdgames run, showing worker, status, and a 0.0 success probability based on over 16000 prior runs](images/tour/run-detail.png)

---

### A codebase's most recent run
`/cupboard/c/lolcat/`

Visiting `/cupboard/c/{codebase}/` without a run id, for example
`/cupboard/c/lolcat/`, redirects to this same kind of page for that
codebase's most recent run.

![Run page for a codebase's most recent run, reached without specifying a run id](images/tour/codebase-lolcat.png)

---

### Evaluate
`/cupboard/evaluate/{run_id}`

`/cupboard/evaluate/{run_id}` renders as a minimal, unstyled page, with
none of the site's usual navigation or layout: just the run's score,
command, and finish time as plain text.

![Evaluate page for a bsdgames run showing the run's score, command, and finish time](images/tour/evaluate.png)

## Setting up an instance

To run an instance like this one, see [deployment](../deploy/README.md)
for the Ansible-based single-host setup and the local Vagrant VM for
trying it out first, and [production](production.md) for building and
running the service containers more generally.
