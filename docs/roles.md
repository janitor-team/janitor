# Access control on the Janitor web UI

The cupboard recognizes four levels of access: anonymous visitors,
logged-in users, the QA reviewer group, and the admin group. Reviewer
and admin membership come from the OAuth2 identity provider configured
in the `oauth2_provider` block in `janitor.conf` (see
`janitor.conf.example`), specifically its `qa_reviewer_group` and
`admin_group` fields. Those fields name a group in the identity
provider's own group list, for example a GitHub org team or a GitLab
group - the site keeps no user database of its own, so granting or
revoking reviewer or admin access means adding or removing someone from
that group on the identity provider itself, not anything done inside
janitor.

## How access is determined

Access level follows from whether you are logged in and, if so, which
groups the identity provider currently reports for your account,
checked again on every request. An anonymous visitor has no session at
all; logging in through the OAuth2 provider establishes one, and QA
reviewer and admin status are then a matter of group membership as the
identity provider reports it, not something tracked or remembered
independently.

Leaving `admin_group` or `qa_reviewer_group` unset in `janitor.conf`
grants that role to every logged-in user - the group check only
restricts access once a group is actually configured; there is no
"nobody qualifies" default.

## Anonymous / guest

An anonymous visitor can browse the campaign home and per-campaign
pages, candidate listings, per-codebase and per-run pages, and the
whole cupboard section, including the review queue itself - none of
it requires a session.

An anonymous visitor can trigger a publish attempt for a codebase
using the "Publish now" button - it has no login or admin check
either.

## Logged in, no group membership

Logging in through the OAuth2 flow does not by itself unlock any
additional pages. The one behavioral change is on the review form shown
on a run page: anonymous visitors see the same form, but clicking a
verdict button while logged out gets a 401 and redirects to `/login`,
while a logged-in user's click succeeds and the verdict is recorded -
accepted from any logged-in user, regardless of group membership.
Whether it also takes effect - forwarded to the runner as a publish
status update - depends on whether you belong to the QA reviewer group,
so a plain logged-in user's "approve" is recorded but does not by
itself publish anything.

## QA reviewer group (`qa_reviewer_group`)

Reviewer membership changes what a submitted verdict does, not who is
allowed to submit one. A verdict other than "abstain" from a reviewer is
forwarded to the runner as a publish status update; a non-reviewer's
verdict of any kind is stored as a comment and never changes whether the
run publishes.

Reviewer group membership also affects one piece of UI: on a successful
run, the Reviews section is shown to a reviewer even before any review
exists, so they see a place to leave one, while other visitors only see
it once a review has been submitted.

## Admin group (`admin_group`)

Admin is the only role enforced server-side beyond being logged in.
Every one of the following requires admin group membership and returns
401 to anyone else:

* `POST /api/merge-proposal` - change a merge proposal's status
  (closed, abandoned, applied, rejected).
* `POST /api/active-runs/{run_id}/kill` - kill a running worker job.
* `POST /cupboard/api/mass-reschedule` - bulk-reschedule runs matching
  a result code and description regex.
* `POST /cupboard/api/run/{run_id}/reprocess-logs` and
  `POST /cupboard/api/reprocess-logs` - reprocess stored build logs for
  one run or in bulk.
* `POST /cupboard/api/publish/autopublish` and
  `POST /cupboard/api/publish/scan` - trigger an autopublish pass or a
  merge-proposal status rescan.
* `GET /cupboard/api/workers`, `POST /cupboard/api/workers`, and
  `DELETE /cupboard/api/workers/{name}` - list registered workers,
  register a new one with a generated password, and remove one.

Templates hide the corresponding controls from non-admins: the Kill
button and Admin column in the queue, the mass-reschedule form on the
result-code and never-processed pages, the Refresh Merge Proposal
Status and Automatically Publish buttons on the publish history page,
the Reprocess Logs sidebar link, and the closed/abandoned/applied/
rejected status dropdown on a merge proposal page.

