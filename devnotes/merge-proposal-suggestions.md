# Suggested changes on merge proposals

For https://github.com/janitor-team/janitor/issues/91.

## The problem

When someone opens a merge proposal that adds a problem a campaign
fixes, for example code that triggers a lintian tag, the janitor only
finds it after the merge. It then opens a merge proposal of its own.

This costs more than it needs to:

- The maintainer reviews two proposals for one change, and the second
  one only cleans up after the first.
- The fix lands later than it could have.
- The contributor is the best person to fix it, and their proposal is
  still open. A suggestion they can apply with one click is cheaper for
  everyone than a follow-up proposal.

Only changes the campaign would make on the proposal, and would not
make on the revision it is based on, should be suggested. Problems that
are already on the base are not the contributor's, and the janitor's
normal runs deal with them.

## Background: suggestions on each forge

On every forge that has them, a suggestion is just an inline comment
with a special block. No forge removes one on a push. GitHub, Gitea and
Forgejo can dismiss a whole review, but no forge can dismiss one
suggestion. GitLab's docs say to reject one by resolving its thread. On
the other forges, a thread resolved without the change is the nearest
thing to a rejection. On GitHub and GitLab, only the proposal's author
or people with write access can resolve threads, so the janitor's
account usually cannot resolve its own. It can still edit its own
comments.

**GitHub.** A reviewer leaves a review comment on some lines of the
diff, with a `suggestion` code block that holds the new text. Anyone
with write access to the branch, usually the author, applies it with
one click, or adds several to a batch for one commit. Over the API it
is an ordinary review comment, with no separate suggestion API.
Comments can only go on lines in the pull request diff. When a later
commit changes the lines, the thread is marked outdated but stays.
https://docs.github.com/en/pull-requests/how-tos/review-pull-requests/incorporating-feedback-in-your-pull-request

**GitLab.** The same idea, with a `suggestion:-0+0` block in a diff
thread. Suggestions are posted through the Discussions API, and have
their own API to apply one or a batch. Applying adds a commit and
resolves the thread. Once a push changes the lines, the suggestion can
no longer be applied. A project can also choose to resolve outdated
threads on every push.
https://docs.gitlab.com/user/project/merge_requests/reviews/suggestions/

**Forgejo.** Forgejo 18, not released yet, adds suggestions with a
`suggestion` block: https://codeberg.org/forgejo/forgejo/pulls/13023.
They are applied from the web UI. Forgejo works out where the commented
lines are at the new head, and the suggestion can be applied as long as
they still match. Threads can be resolved, and Forgejo 18 also adds
that to the API.

**Gitea.** Review comments on diff lines, marked outdated when the
lines change, and threads that can be resolved in the web UI and the
API. Gitea has no suggestions.

**Launchpad.** Inline comments on the preview diff, with no
suggestions and no resolved state. Inline comments belong to one
preview diff, so after a push old comments stay on the old diff.
https://launchpad.net/+apidoc/devel.html

## What the janitor would do

1. When a merge proposal on a known codebase is opened or updated, and
   the campaign has opted in, run the campaign on the proposal's head
   and on the revision it is based on. Proposals the janitor opened
   itself are skipped.
2. Compare the two. Changes that only show up on the head are the ones
   the proposal brought in.
3. Post each of those as a suggestion on the lines it changes.
4. Remember each suggestion: the proposal, the comment, and the change
   itself (which fix, the file, the old text and the new text), not
   only its line.

These runs are kept apart from the codebase's normal results.

## When the proposal is updated

Every push starts again at step 1 with the new head. Then, for each
suggestion already posted:

- The new run no longer wants the change, because the contributor
  applied it, fixed it by hand, or changed the code another way. The
  janitor withdraws its suggestion: it resolves the thread where it is
  allowed to, and otherwise edits its comment.
- The change is still wanted and the posted suggestion still applies,
  even if its lines moved. The suggestion stays.
- The change is still wanted, but the posted suggestion no longer
  applies because its lines changed. The janitor withdraws it and,
  unless it was turned down, posts the suggestion again where it now
  belongs.

## When a suggestion is turned down

If a person resolves one of the janitor's threads without applying it,
and the next run still wants the change, the janitor counts the
suggestion as turned down. A thread resolved by the janitor itself, or
by the forge after a push changed its lines (GitLab can do this), does
not count. The janitor records this against the change, not the line, so
it does not suggest the same change on that proposal again, even after
the code moves.

## Open questions

1. Every proposal for opted-in campaigns, or only when someone asks,
   for example with a label?
2. Is the base revision the merge base, or the tip of the target
   branch?
3. When the janitor cannot resolve its own thread, should it edit the
   comment to say it is no longer needed, or delete it?
4. Should a turned down suggestion also stop the janitor's own
   proposal for that change after the merge?
5. Gitea, Launchpad and Forgejo before 18 have no suggestions. Post a
   plain inline comment with the diff there, or skip them?
6. A change can touch lines outside the proposal's diff, which GitHub
   cannot comment on. Drop it, or list it in one plain comment?
