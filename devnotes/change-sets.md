# Change sets for candidates

A design for the last open item of
https://github.com/janitor-team/janitor/issues/61: "add changesets for
every candidate and make that field mandatory".

## Problem

A change set groups the runs that make up one change. Its state says how
far the change has got. The publisher only publishes a set that is
`ready` or `publishing`, and `/dists/cs/<id>` serves the set's builds in
one place.

That only works for candidates uploaded with a change set. A candidate
uploaded without one has `candidate.change_set` NULL, and when one of its
runs finishes, the runner stores the run in a new set named after the
run. On a test instance nearly every set held one campaign run, plus a
control run when that run succeeded. For these candidates:

- There is no one URL for a change. Each run from the regular scheduler
  gets a new set, so each `/dists/cs/<id>` holds the builds of one run,
  and `/dists/<campaign>/<codebase>` serves only the set of the latest
  successful run. A run that the publisher reschedules joins the earlier
  run's set, but the runs for one merge proposal can still end up spread
  over several sets.
- The set state follows one run, not the change. `change_set_todo` skips
  candidates without a set, so such a set is `ready` as soon as its run
  succeeds, and a later run for the same change starts a new set from
  `created`.
- The follow-up schedule after a run looks the candidate up by the run's
  set. Reading the code, for a candidate without a set it finds none, so
  no follow-up is queued.

Giving every candidate a set makes the set stand for one change of one
candidate: all of its runs, one `/dists/cs/<id>` for the whole change,
and a state that follows the change from its first run until it has
landed.

## Goals

- Every candidate uploaded without a change set gets a set of its own,
  and `candidate.change_set` is NOT NULL.
- All runs for one change of such a candidate share its own set, from the
  first run until the change has landed.
- Once the change has landed, that set stays `done` and the candidate's
  next runs go in a new own set.
- Change sets given in an upload work as they do now.

## Non-goals

- New change set states, or moving a set backwards. `done` stays final.
- Changes to how a set given in an upload becomes `done`.
- Moving old runs into new sets. Old runs keep the set they have.
- Changes to the publish policy or to the review flow.

## Data model

`change_set` gets one new nullable column, and at most one open own set
per codebase and campaign:

```sql
ALTER TABLE change_set
  ADD COLUMN codebase text REFERENCES codebase(name) ON DELETE CASCADE;
CREATE UNIQUE INDEX ON change_set (codebase, campaign)
  WHERE codebase IS NOT NULL AND state != 'done';
```

A set with `codebase` set is a candidate's own set. A set with `codebase`
NULL is a set given in an upload, or one of the old one-run sets, and is
left alone.

New set ids are random, like run ids. They are not derived from the
codebase and campaign, because the next set for the same candidate needs
a different id.

`candidate.change_set` becomes NOT NULL. A codebase can still have several
candidates for one campaign, one per set, as today. The index above keeps
it to one open own set, which is the one used when no set is given.

## Lifecycle

**Made.** A candidate gets its own set when it is first uploaded without
one. The upload in `POST /candidates` cannot rely on its current
`ON CONFLICT (codebase, suite, coalesce(change_set, ''))` for this,
because a new random id would never conflict and each upload would add
another candidate row. So when no set is given, the upload updates the
candidate's row with an open own set, and only inserts a row with a new
own set when there is none.

**Used.** Anything that schedules a run without a set uses the
candidate's own set: the regular scheduler, `/schedule`, the follow-up
after a run, and the publisher's reschedules. A run that is given a set
keeps it. The runner's fallback of a set named after the run then only
applies to runs that have no candidate.

**Done.** An own set is `done` once the last successful run of the set's
campaign (`run.suite = change_set.campaign`) has no unabsorbed result
branch, so once that change was merged or pushed. A later failed or
nothing-to-do run in the set does not count, and neither does a control
run, which shares the set but has no result branches. Today the
`publishing` to `done` step reads `change_set_unpublished`, which looks at
`last_unabsorbed_runs`. A failed run after an open merge proposal becomes
that last run, and the set moves to `done`. For own sets the check looks
at the last successful run of the set's campaign instead.

Today `store_publish` in the publisher also sets `done` itself, when no
run in the set has an unabsorbed branch. That update goes through
`refresh_change_set_state` instead, so there is one place that marks a
set `done`.

**Replaced.** When `refresh_change_set_state` moves an own set to `done`,
it also makes a new own set for that candidate, points the candidate at
it, and moves the candidate's queue items to it. As that is the only
place that marks a set `done`, a candidate never points at a `done` own
set, and the code that reads `candidate.change_set` needs no extra check.

A `done` set is never opened again. A run that started before the change
landed keeps the old set id, see the open questions.

## Publishing

`publish_ready` joins on the run's set, and the publisher only takes rows
whose set is `ready` or `publishing`. That stays. With own sets, all runs
for one merge proposal are in one set, so its state follows the proposal:
`publishing` while it is open, `done` once it is merged. The publisher's
one change is the `store_publish` update above.

`/dists/cs/<id>` then serves the runs of one change while it is open, and
the next change gets a new id once the old one has landed.

## Site

The cupboard queries that test `candidate.change_set IS NULL` look for an
own set instead (`change_set.codebase IS NOT NULL`). The change set list
in the cupboard only lists sets that have candidates. With own sets every
candidate would show up there, so it lists sets with `codebase` NULL.

## Migration

For an existing database:

1. Add `change_set.codebase` and its index.
2. For each candidate with `change_set` NULL, make a new own set for its
   codebase and campaign in state `created`, and point the candidate at
   it.
3. Move each queue item with `change_set` NULL to the own set of its
   candidate.
4. Make `candidate.change_set` NOT NULL.

Old runs keep their one-run sets, so a change that is being published
during the migration finishes in its old set. The next run of that
candidate goes in the new own set.

## Alternatives considered

**One set per candidate, for good.** This was
https://github.com/janitor-team/janitor/pull/2031, with ids from
`md5(codebase/suite)`. A set that is `done` then cannot take the next
change. Either it is opened again when a new run adds an unpublished
branch, which mixes landed and new runs in one set and `cs/<id>`, or the
new runs are never published. From the comment that closed it: "we'll
need a new changeset once the old change has landed".

**Keep one set per run.** No migration, but the set state and `cs/<id>`
keep describing single runs, `change_set_todo` never applies to these
candidates, and the follow-up after a run keeps finding no candidate.

**Make the next set later.** Instead of at once, the next own set could be
made when the candidate is next uploaded or scheduled. That spreads the
swap over every place that schedules a run, and a candidate would point
at a `done` set in between.

**A trigger on `candidate` for the first set.** That PR used one. With
random ids it does not fit the upload's `ON CONFLICT`, as described under
Lifecycle, so the first set is made in the upload code.

## Open questions

1. A run that started before its set became `done` finishes in that set.
   It is not published, because `done` is final, and its follow-up
   schedule finds no candidate. Should the runner store such a run in the
   candidate's current set instead, or is it fine to leave it, since the
   change it was made for has landed?
2. When a merge proposal is closed without being merged, its branches are
   never absorbed, so the set stays `publishing`. Should a rejected change
   also get a new set, and if so, what marks it?
3. Should sets given in an upload also ignore a later failed run when
   they move to `done`?
4. Should the migration be idempotent statements in `schema/state.sql`,
   as https://github.com/janitor-team/janitor/pull/2031 did, or a
   separate script for existing instances?
5. https://github.com/janitor-team/janitor/pull/1609 refreshes `last_run`
   first, which the set state relies on. Should this wait for it?
