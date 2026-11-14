# Review runs

The publisher does not publish a successful run automatically until its
publish status is `approved`. A new run's publish status is `unknown`, so
the run waits for approval, whatever the candidate's publish policy says.
But an open merge proposal is updated with a newer successful run,
whatever its publish status.
The run's page shows the status in its "Publish status" line.

## Who can approve

On the site, only QA reviewers can approve a run. A QA reviewer is a
member of the group that `qa_reviewer_group` names in the `oauth2_provider`
block of `janitor.conf` (see [Web UI login](production.md#web-ui-login)).

## Review a run

The `/cupboard/review` page shows the runs that need a review one at a
time, with their changes and a link to the run's page. QA reviewers who
have logged in choose one of these verdicts:

- *Approve*: the publish status becomes `approved`. The publisher then
  handles the run as the candidate's publish policy says.
- *Reject*: the site asks for a comment, and the publish status becomes
  `rejected`. The publisher does not publish the run automatically, but
  updates an open merge proposal with it.
- *Reschedule*: the run is rejected and a new run is scheduled.
- *Abstain*: the verdict is recorded, and the publish status stays as it
  was.
