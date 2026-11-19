# Known bugs

Captured from a live-cluster log scan on 2026-04-24 (minikube-local,
`runner`/`publish`/`site`/`git-store` pods). Priority is "how noisy
/ damaging" not "how hard to fix". File issues as they get fixed or
reproduce elsewhere.

## Open

1. **`publish` — stale `merge_proposal` rows with no matching `run`.**
   1303 such rows in a 6h scan. `check_existing_mp` fails them into
   `CheckMpError::NoRunForMergeProposal`, which the loop now rolls
   up into a single summary warn per scan (`publish/src/lib.rs`).
   The underlying drift is real — either runs are being deleted while
   MPs stay open, or old MPs were imported before the matching runs
   existed. Needs either:

   a. A periodic reaper along the lines of
      ```sql
      DELETE FROM merge_proposal mp
       WHERE NOT EXISTS (SELECT 1 FROM run r WHERE r.revision = mp.revision)
         AND mp.status = 'open'
         AND mp.last_scanned < NOW() - INTERVAL '30 days';
      ```
      (add `merge_proposal.last_scanned timestamptz` first; set it in
      `check_existing_mp`'s success path).

   b. Or a foreign-key cascade so MPs follow when runs vanish.
      Requires deciding whether an MP should survive deletion of its
      originating run at all — probably not for the "open" state.

   Design call before any `DELETE` lands in code.

2. **`publish` — `breezyshim::forge.rs:557` unwrap on
   `GitHubLoginRequired`.** Fixed in the sibling breezyshim checkout
   (`~/src/breezyshim`) by replacing the `.try_iter().unwrap()` +
   `.map(|p| p.unwrap())` chain with a for-loop that `?`-propagates
   both iterator-creation and per-item errors. `janitor/Cargo.toml`
   has a `[patch.crates-io]` entry pointing at that checkout. Drop
   the patch once the fix ships in a crates.io release and
   `[workspace.dependencies] breezyshim` is bumped past 0.7.15.

## Resolved in this repo

Removed from the open list; summaries kept in git log:

- runner `UnsupportedProtocol` / `open_resume_branch` scheme guard
  (`35cb8560`).
- publish `proposal_info.rs` — `merge_proposal_status` ENUM cast
  (`437cd480`).
- publish `state.rs` — `guess_codebase_from_branch_url` scheme
  variants for `https ↔ ssh ↔ git+ssh` (`437cd480`).
- git-store `git_http.rs` — strip `,branch=<role>` / `%2Cbranch%3D`
  before codebase lookup (`35cb8560`).
- runner `watchdog.rs` — "different-run" log now carries the worker
  identity + active-runs dedup check (`437cd480`).
- runner `campaign_config` / distribution DB queries replaced with
  in-memory textproto reads (`ac652fd1`).
- publish "Unable to find metadata" rollup (demoted to debug +
  summary warn at scan end) (`5f2f2ee0`).
- differ / followup / ognibuild-dep were "quiet — check traffic" —
  all three respond 200 on `/health`, genuinely idle, not a bug.
- publish 35 s `COMMIT` was a one-off; no repro in subsequent scans.
  Reopen if it recurs.
