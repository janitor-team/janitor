//! A resume branch that is not used must show up in the log and in the metrics.

use janitor_runner::metrics::{MetricsCollector, RESUME_BRANCHES_SKIPPED_TOTAL};
use janitor_runner::resume::{note_resume_branch_skipped, SKIP_REQUIRES_AUTHENTICATION};
use std::sync::Mutex;

const BRANCH_URL: &str = "git+ssh://git:hunter2@github.com/janitor-bot/foo,branch=lintian-fixes";
const QUERY_URL: &str =
    "git+ssh://git@github.com/janitor-bot/foo,branch=lintian-fixes?token=secret#frag";
const SHOWN_URL: &str = "git+ssh://github.com/janitor-bot/foo,branch=lintian-fixes";

/// Logger that keeps every record so the test can read them back.
struct Capture(Mutex<Vec<(log::Level, String)>>);

impl log::Log for Capture {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.0
            .lock()
            .unwrap()
            .push((record.level(), record.args().to_string()));
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));

/// Skipping a resume branch logs one warning without credentials and counts it.
#[test]
fn skipped_resume_branch_is_warned_about_and_counted() {
    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(log::LevelFilter::Trace);

    assert_eq!(SKIP_REQUIRES_AUTHENTICATION, "requires-authentication");
    let counter = RESUME_BRANCHES_SKIPPED_TOTAL.with_label_values(&[SKIP_REQUIRES_AUTHENTICATION]);
    let before = counter.get();

    let url = url::Url::parse(BRANCH_URL).unwrap();
    note_resume_branch_skipped(
        "foo",
        "lintian-fixes",
        "run-1",
        &url,
        SKIP_REQUIRES_AUTHENTICATION,
    );

    assert_eq!(counter.get(), before + 1);

    let warnings: Vec<String> = CAPTURE
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, _)| *level == log::Level::Warn)
        .map(|(_, message)| message.clone())
        .collect();
    let expected = format!(
        "Not resuming foo/lintian-fixes from run run-1: skipping resume branch {} ({})",
        SHOWN_URL, SKIP_REQUIRES_AUTHENTICATION
    );
    assert_eq!(warnings, vec![expected]);

    // The counter is in what /metrics serves.
    let exported = MetricsCollector::collect_metrics().unwrap();
    let wanted = format!(
        "janitor_runner_resume_branches_skipped_total{{reason=\"{}\"}} {}",
        SKIP_REQUIRES_AUTHENTICATION,
        before + 1
    );
    assert!(exported.lines().any(|l| l == wanted), "{}", exported);

    println!("WARN {}", warnings[0]);
    println!("{}", wanted);

    // A query or fragment on the URL is not logged either.
    let url = url::Url::parse(QUERY_URL).unwrap();
    note_resume_branch_skipped(
        "foo",
        "lintian-fixes",
        "run-1",
        &url,
        SKIP_REQUIRES_AUTHENTICATION,
    );
    let records = CAPTURE.0.lock().unwrap();
    let (level, message) = records.last().unwrap();
    assert_eq!(*level, log::Level::Warn);
    assert!(!message.contains("token=secret"), "{}", message);
    assert!(!message.contains("frag"), "{}", message);
    assert!(message.contains(SHOWN_URL), "{}", message);
    assert_eq!(counter.get(), before + 2);
}
