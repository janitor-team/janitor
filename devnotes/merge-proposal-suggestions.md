# Suggested changes on merge proposals

For https://github.com/janitor-team/janitor/issues/91.

## The problem

Campaigns fix problems once they are on a codebase's branch. If someone
else's merge proposal adds a problem that a campaign fixes, it is merged
with the problem, and then the Janitor opens its own merge proposal to
fix it. The maintainer reviews the same lines twice, and the problem
sits on the branch in between.

The Janitor could instead suggest the fix on the first proposal, where
it can be applied with one click before the merge. Only problems the
proposal adds get suggestions. Problems already on the target branch
are left to the normal runs.

For example, a merge request on salsa adds a binary package with no
`${misc:Depends}`. Today lintian-fixes adds it after the merge. With
this, the fix is suggested on the merge request.

## What the forges offer

A suggestion is a review comment on lines of the diff that holds the new
text. Applying it makes a commit on the proposal's branch. It can only
replace lines shown in the diff. When a later push changes those lines,
the forge marks the comment outdated.

- GitHub: `suggestion` blocks in review comments. One review holds many
  of them and sends one notification.
- GitLab: `suggestion:-N+M` blocks in diff threads. Draft notes are
  published together.
- Forgejo: suggestions come in 18.0:
  https://codeberg.org/forgejo/forgejo/pulls/13023. On earlier
  versions, every hunk goes in the review's text, as hunks outside the
  diff do.
- Launchpad has no suggestions and is left out.

## How someone asks

Someone with write access to the target repository requests the
Janitor's review on the proposal. Mentions are not used, since people
mention the Janitor's account for other reasons too. On GitHub, only
people and teams that have been given access to the repository can be
requested, so the account has to be added there first. Posting a review
on a public repository needs no access.

For a new request the Janitor needs to know who made it. GitHub's and
GitLab's lists of requested reviewers do not say. On GitHub, the
`review_requested` event in the proposal's timeline names who asked.
GitLab's documentation only names who asked in the webhook's `user`
field, so there the requester is taken from the payload. On Forgejo,
whether the proposal's timeline names who asked has not been checked
yet. The Janitor then asks the forge for that person's permission on the
target repository. If they cannot push there, the request is ignored. On
GitHub, organization members with triage access can also request a
review. Their requests are ignored on purpose, so that only people who
can push to the target repository start these runs.

## How the Janitor hears about it

The Janitor does not poll the forges. It learns about review requests
and new pushes from webhooks, the same way it learns about pushes to
target branches today: the project points a webhook at `/webhook` and
also sends pull request or merge request events. A project that sends
none gets no suggestions.

The `/webhook` endpoint does not authenticate deliveries today. For a
push event the harm is small, since it only records the pushed revision
and reschedules runs. A merge request event is only taken as a hint: it
gives the proposal's URL, and the Janitor then reads the proposal and
its review requests from the forge API with its own credentials and acts
on what the forge says, not on what the payload says, apart from who
made the request on GitLab (see "How someone asks"). A forged delivery
costs at most a look at a real proposal, except on GitLab, where it
could name the wrong person as the requester. It cannot start a second
run for a head that was already run. Where a project has set a webhook
secret, `/webhook` can also check the forge's signature or token.

The proposal's URL ties the event to a proposal the Janitor already
follows. For a proposal it does not follow yet, the target branch's URL
gives the codebase, as it does for push events today. The Janitor runs
that codebase's campaigns that opt in.

## When it runs

Once asked, the Janitor follows the proposal until it is merged or
closed, and runs the tip of the proposal's branch each time a webhook
says it changed.

There is at most one run per proposal, campaign and head revision. A
webhook that arrives for a head that is already queued or run is
dropped, so a redelivered webhook or two events for the same push start
one run per campaign. While runs are going, newer pushes only update the
head to run next. The Janitor gives up on a proposal after 5 heads in an
hour that it could not post a review for, and waits for the next review
request.

## How the runs are compared

Both runs are of a branch tip. The head run starts from the tip of the
proposal's branch. The base run is the campaign's last run on the tip
of the target branch, which the Janitor usually has already, so most
proposals need only the head run. If there is none for the target's
current tip, a base run is queued for it the same way as a head run.
No run starts from an older revision. A problem that the target branch
fixed after the proposal branched off, for example by merging one of the
Janitor's own merge proposals, then looks new on the proposal. A rebase
of the proposal makes it go away.

Each run diffs its result against the revision it started from and
splits that into hunks without context lines. Each hunk takes the
message of the codemod commit that last changed its lines. A hunk is
known by its campaign, its file and the lines it removes and adds. Line
numbers are not part of it, so a hunk still matches after lines move.

For each hunk the base run also has, one matching hunk of the head run
outside the lines the proposal added is dropped. If every matching head
hunk is inside the lines the proposal added, the base run's hunk counts
as already covered and none is dropped. So if a proposal adds three
identical lines with trailing whitespace, all three get a suggestion.

The hunks left are the suggestions. One that only touches lines the
proposal added goes inline. So does one that only adds lines, if the
line above them is one the proposal added. It then sits on that line.
Any other is listed in the review's text with its message and diff,
since a suggestion can only replace lines in the diff.

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

The head runs, and any base run queued for this, use the campaign's
codemod without a build. They add no changelog entry, as they fix the
proposal before it lands. Nothing is scheduled or published from them,
and their branches are not kept.

## Posting

The Janitor posts one review per head revision, and none when nothing
is new. The review's text names each campaign and links its run. Each
inline comment starts with the hunk's message, then the suggestion.

The review is posted on the head revision the run started from, so its
line numbers are that revision's. If the proposal has a newer head by
then, nothing is posted and the newer head is run instead. The cap in
"When it runs" bounds how often that can happen. The runs have no
build, so they are short.

## When the proposal changes

The Janitor keeps the hunk of each thread it opened. Someone may resolve
such a thread while the next run still wants its hunk. That turns the
hunk down, and it is not suggested on that proposal again, even after it
moves. This holds for anyone other than the Janitor, the proposal's
author as well as a maintainer. On GitLab, applying a suggestion also
resolves its thread, but the hunk is then no longer wanted, so it is not
turned down. A resolved thread that is outdated does not turn its hunk
down either, as GitLab can resolve outdated threads by itself on a push.

A turned-down hunk only counts on that proposal. The Janitor's own
merge proposal after the merge still has the fix, since whoever turned
it down may not be the maintainer.

After each run, for each thread the Janitor opened:

- If the hunk is no longer wanted (applied or fixed another way), the
  Janitor replies that it is no longer needed, unless the thread is
  resolved. Someone already closed a resolved thread, and a reply would
  only send them a notification about a fix they no longer need to look
  at.
- If it is still wanted and the thread is outdated, it is suggested
  again in the next review, unless it was turned down.

## Implementation

breezy 3.4, in its GitHub, GitLab and Gitea forges. Forgejo has no forge
class of its own: breezy's Gitea forge talks to it, as Forgejo has a
Gitea 1.22 compatible API. That forge tells Forgejo from Gitea by the
version the server reports, and writes suggestions only where the server
is known to have them. Everywhere else, as on Forgejo before 18.0, every
hunk goes in the review's text. Today breezy can only post a plain
comment on a proposal (`MergeProposal.post_comment`). New API:

- `MergeProposal.iter_review_requests()`: the proposal's open review
  requests, each with who made it where the forge API gives that (see
  "How someone asks").
- `MergeProposal.user_can_push(user)`: whether a user can push to the
  target repository. A user is given as a username on GitHub and
  Forgejo and as a numeric id on GitLab, which is what their permission
  APIs take.
- `MergeProposal.post_review(revision, body, suggestions)`: one review
  on a given head revision, with inline suggestions (file, first and
  last line, new lines, message). breezy writes each forge's suggestion
  syntax, and puts them in the review's text where the server has no
  suggestions.
- `MergeProposal.iter_review_threads()`: threads with their id, file,
  first comment, author, and resolved and outdated state. On GitHub that
  state is only in the GraphQL API. GitLab and Forgejo have no outdated
  field, so there breezy works it out by comparing the comment's
  position and commit with the current diff.
- `MergeProposal.reply_to_thread(thread_id, body)`. Each of these forges
  can reply in a thread: a reply to the review comment on GitHub, a note
  in the discussion on GitLab, and a reply to the review comment on
  Gitea and on Forgejo from 18.0. Where the server cannot, as on Forgejo
  before 18.0, breezy posts a plain comment that quotes the thread
  instead.

breezyshim: Rust wrappers for these.

janitor:

- config: a `Campaign` option to opt in.
- schema: a table of followed proposals (URL, codebase, head revision
  to run next, last run), and one of the threads the Janitor opened
  (proposal, thread id, campaign, hunk, turned down). The hunk is stored
  as a SHA-256 of its campaign, file, removed and added lines, which is
  all that is needed to recognise it again, so a row stays small
  whatever the hunk's size.
- runner: queue items for the tip of a proposal's branch, or of the
  target branch for a missing base run, outside the normal scheduling,
  never published, and with `DEB_UPDATE_CHANGELOG=leave` for Debian
  codebases. One item per proposal, campaign and head revision, or per
  campaign and target tip for a base run.
- worker: start from the tip of any branch URL as it does today, skip
  the build, and return the hunks with their commit messages in the
  result.
- publish: the comparison, posting reviews and checking threads, after
  each run.
- site: merge request events on `/webhook`, passed on to publish.

## Concerns

- Cost: a run on each new head of a followed proposal, and a base run
  when the target branch's tip has none.
- The Janitor keeps state about proposals it did not open.
- On GitHub its account has to be added to each repository that uses
  this, so that it can be requested as a reviewer. In an organization,
  read or triage access is enough. A repository owned by a personal
  account only offers collaborators write access.
- Only projects that send merge request webhooks get suggestions.

## Alternatives considered

- The current flow: the Janitor's own merge proposal after the merge.
  Nothing to build, but the same lines are reviewed twice.
- A CI job that runs the codemod and posts its diff. A second place to
  keep in step with the campaign, and CI for a proposal from a fork
  usually cannot post to it.
- One plain comment with the whole diff. Works everywhere, but the fix
  is applied by hand.
- Pushing a fix commit to the contributor's branch. Needs write access
  and changes their branch without asking.
