# Suggested changes on merge proposals

For https://github.com/janitor-team/janitor/issues/91.

## The problem

Campaigns fix problems once they are on a codebase's branch. If someone
else's merge proposal adds a problem that a campaign fixes, it is merged
with the problem, and then the janitor opens its own merge proposal to
fix it. The maintainer reviews the same lines twice, and the problem
sits on the branch in between.

The janitor could instead suggest the fix on the first proposal, where
it can be applied with one click before the merge. Only problems the
proposal adds get suggestions. Problems already on the target branch
are left to the normal runs.

For example, a merge request on salsa adds a binary package with no
`${misc:Depends}`. Today lintian-fixes adds it after the merge; with
this, the fix is suggested on the merge request.

## What the forges offer

A suggestion is a review comment on lines of the diff that holds the new
text. Applying it makes a commit on the proposal's branch. It can only
replace lines shown in the diff. When a later push changes those lines,
the forge marks the comment outdated and it can no longer be applied.

- GitHub: `suggestion` blocks in review comments. One review holds many
  of them and sends one notification.
- GitLab: `suggestion:-N+M` blocks in diff threads. Draft notes are
  published together.
- Forgejo: suggestions come in 18.0:
  https://codeberg.org/forgejo/forgejo/pulls/13023.
- Launchpad has no suggestions and is left out.

## How someone asks

Someone with write access to the target repository requests the
janitor's review on the proposal. Mentions are not used, since people
mention the janitor's account for other reasons too. GitHub only lets
collaborators be requested, so there the account has to be added to
the repository first.

The janitor lists the open proposals that request its review: a
`review-requested:` search on GitHub, merge requests by
`reviewer_username` on GitLab, and the issue search with
`review_requested` on Forgejo. For a new request it looks up who made
it in the proposal's history and asks the forge for that person's
permission on the target repository. If they cannot push there, the
request is ignored.

The target branch's URL gives the codebase, as it does for push
webhooks today. The janitor runs that codebase's campaigns that opt in.

## When it runs

Once asked, the janitor follows the proposal until it is merged or
closed, and runs each new head revision. A loop checks the review
requests and the heads of followed proposals every five minutes.

The `/webhook` endpoint takes push events today. It would also take
merge request events, so a project that sends them gets the check as
soon as someone pushes or requests a review.

## How the runs are compared

The head run starts from the proposal's head. The base run starts from
the merge base of the proposal and the target branch, which is what the
forge's diff is against. The base run is reused until the merge base
changes, for example after a rebase.

Each run diffs its result against the revision it started from and
splits that into hunks without context lines. Each hunk takes the
message of the codemod commit that last changed its lines. A hunk is
known by its campaign, its file and the lines it removes and adds. Line
numbers are not part of it, so a hunk still matches after lines move.

For each hunk the base run also has, one matching hunk of the head run
is dropped, preferring one outside the lines the proposal added. The
hunks left are the suggestions. One that only touches lines the
proposal added goes inline. So does one that only adds lines, if the
line above them is one the proposal added; it sits on that line. Any
other is listed in the review's text with its message and diff, since a
suggestion can only replace lines in the diff.

Take a campaign that strips trailing whitespace from the files the
project's `.editorconfig` asks for. The target branch has a line with
trailing whitespace in `README.md`. A proposal adds another such line to
`README.md`, and adds `docs/*.md` to `.editorconfig`. An old line in
`docs/intro.md` has trailing whitespace. Both runs fix the old
`README.md` line, so that hunk is dropped. The new `README.md` line is
only fixed in the head run and the proposal added it, so it gets an
inline suggestion. The `docs/intro.md` fix is new too, but its line is
not in the proposal's diff, so it goes in the review's text.

## The runs

The runs use the campaign's codemod without a build. They add no
changelog entry, as they fix the proposal before it lands. Nothing is
scheduled or published from them, and their branches are not kept.

## Posting

The janitor posts one review per head revision, and none when nothing
is new. The review's text names each campaign and links its run. Each
inline comment starts with the hunk's message, then the suggestion.

The review is posted on the head revision the run started from, so its
line numbers are that revision's. If the proposal has a newer head by
then, nothing is posted and the newer head is run instead.

## When the proposal changes

The janitor keeps the hunk of each thread it opened. A thread that
someone else resolves while the next run still wants its hunk turns that
hunk down, and it is not suggested on that proposal again, even after
it moves. On GitLab, applying a suggestion also resolves its thread, but
the hunk is then no longer wanted, so it is not turned down. A resolved
thread that is outdated does not turn its hunk down either, as GitLab
can resolve outdated threads by itself on a push.

A turned-down hunk only counts on that proposal. The janitor's own
merge proposal after the merge still has the fix, since whoever turned
it down may not be the maintainer.

After each run, for each thread the janitor opened:

- If the hunk is no longer wanted (applied or fixed another way), the
  janitor replies that it is no longer needed, unless the thread is
  resolved.
- If it is still wanted and the thread is outdated, it is suggested
  again in the next review, unless it was turned down.

## Implementation

breezy 3.4, in its GitHub, GitLab and Gitea forges. Today it can only
post a plain comment on a proposal (`MergeProposal.post_comment`). New
API:

- `Forge.iter_review_requests()`: open proposals that request the
  current user's review, each with who requested it.
- `MergeProposal.user_can_push(user)`: whether a user can push to the
  target repository.
- `MergeProposal.post_review(revision, body, suggestions)`: one review
  on a given head revision, with inline suggestions (file, first and
  last line, new lines, message). breezy writes each forge's suggestion
  syntax.
- `MergeProposal.iter_review_threads()`: threads with their id, file,
  first comment, author, and resolved and outdated state.
- `MergeProposal.reply_to_thread(thread_id, body)`.

breezyshim: Rust wrappers for these.

janitor:

- config: a `Campaign` option to opt in.
- schema: a table of followed proposals (URL, codebase, head and merge
  base revisions, last runs), and one of the threads the janitor opened
  (proposal, thread id, campaign, hunk, turned down).
- runner: queue items for a proposal's head or merge base, outside the
  normal scheduling, never published, and with
  `DEB_UPDATE_CHANGELOG=leave` for Debian codebases.
- worker: start from a given revision of any branch URL (an assignment
  only has a branch URL today), skip the build, and return the hunks
  with their commit messages in the result.
- publish: the five-minute loop, the comparison, posting reviews and
  checking threads.
- site: merge request events on `/webhook`, passed on to publish.

## Concerns

- Cost: a run on each new head, and one on each new merge base.
- The janitor keeps state about proposals it did not open.
- On GitHub its account has to be a collaborator of each repository
  that uses this.
- The five-minute loop makes forge API calls for each followed
  proposal.

## Alternatives considered

- The current flow: the janitor's own merge proposal after the merge.
  Nothing to build, but the same lines are reviewed twice.
- A CI job that runs the codemod and posts its diff. A second place to
  keep in step with the campaign, and CI for a proposal from a fork
  usually cannot post to it.
- One plain comment with the whole diff. Works everywhere, but the fix
  is applied by hand.
- Pushing a fix commit to the contributor's branch. Needs write access
  and changes their branch without asking.
