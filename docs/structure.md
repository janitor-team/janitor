## Structure 

- `./reschedule.py` - a tool for users of the janitor and can be run by anybody locally
- `./helpers/*` - all need to run inside of a janitor deployment (and talk to the database, etc) by an admin.
- `src/bin/janitor-admin.rs` (`janitor-admin`) - CLI over the site's HTTP API for admin-only actions (workers, mass-reschedule, log reprocessing, publisher passes). See [`janitor-package.md`](janitor-package.md) for its non-admin sibling.
- `src/bin/janitor-package.rs` (`janitor-package`) - CLI over the site's HTTP API for the actions a logged-in, non-admin user (a package maintainer or QA reviewer) can take. See [`janitor-package.md`](janitor-package.md).
