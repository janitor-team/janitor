# How the janitor works

This page follows one Debian package through the janitor, from the queue
to a merge proposal or an upload, and names the service that does each
step. The example is the `lintian-fixes` campaign, which runs
`lintian-brush` on a package to fix its lintian warnings.

1. You add the package to the runner as a codebase, with a candidate that
   pairs it with the campaign. The runner puts the candidate in its queue.
   [scheduling.md](scheduling.md) says how to add them and how the queue is
   ordered.

2. A worker asks the runner for work and gets the next item in the queue:
   where the package's branch is, the command to run and how to build the
   package. The worker clones the branch from its forge and runs
   `lintian-brush`. It pushes the changed branch to the git-store (the
   bzr-store for Bazaar), builds the package with sbuild and sends the logs
   and built packages back to the runner. [runs.md](runs.md) lists the
   stages of a run and the ways it can end.

3. The runner saves the run in PostgreSQL, keeps the logs and built
   packages, then announces the result over Redis. After a successful run,
   it also queues a control run, which builds the same package without the
   change.

   The archiver and auto-upload are optional. When you run them, the
   archiver adds the new packages to the campaign's
   [apt repository](debian.md#the-apt-archive) and auto-upload signs and
   uploads them with dput, both straight away, without waiting for a
   review. Once the control run is done, the differ compares the two builds
   with debdiff and diffoscope.

4. A QA reviewer approves or rejects the run on the site, which shows its
   diff and logs (see [Review and publishing](runs.md#review-and-publishing)).
   The site sends the verdict to the runner, and the runner passes it on
   to the publisher.

5. The publisher acts on the approval at once. It fetches the changed
   branch from the git-store and, as the candidate's publish policy says,
   pushes it to the forge or opens a merge proposal there. It gets the
   debdiff from the differ too, for the merge proposal text.

6. Every two hours by default, the publisher goes through approved runs
   that are not published yet and checks the merge proposals it opened,
   recording when one is merged or closed. If a proposal has a merge
   conflict with its target branch, the publisher queues a new run of the
   candidate.
