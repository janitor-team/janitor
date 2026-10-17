//! End-to-end tests for `publish_one` against real git repositories.

mod e2e_harness;

use e2e_harness::{run_publish_one, GitDaemon, GitFixture};
use janitor::publish::Mode;

#[test]
fn publish_one_opens_the_source_branch_by_its_unescaped_name() {
    let fx = GitFixture::new();
    let source = fx.init_repo("cb");
    let tip = fx.add_branch(&source, "lintian-fixes/main");
    let target = fx.bare_clone(&source, "target");
    fx.git(&target, &["update-ref", "refs/heads/main", &tip]);
    // With no default branch and no main, only the named branch can be opened.
    fx.git(&source, &["symbolic-ref", "HEAD", "refs/heads/unborn"]);
    fx.git(&source, &["branch", "-D", "main"]);
    let daemon = GitDaemon::start(&fx);
    let request = fx.request(
        daemon.source_branch_url("cb"),
        fx.url_of(&target),
        &tip,
        Mode::Push,
    );

    run_publish_one(&request).unwrap();
}
