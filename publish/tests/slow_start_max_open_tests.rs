use std::collections::HashMap;

use janitor::publish::MergeProposalStatus;
use janitor_publish::rate_limiter::{RateLimitStatus, RateLimiter, SlowStartRateLimiter};

/// A limiter that has seen one open and `merged` merged proposals in `bucket`.
fn scanned(max: Option<usize>, merged: usize) -> SlowStartRateLimiter {
    let mut limiter = SlowStartRateLimiter::new(max);
    let mut mps = HashMap::new();
    mps.insert(
        MergeProposalStatus::Open,
        HashMap::from([("bucket".to_string(), 1)]),
    );
    mps.insert(
        MergeProposalStatus::Merged,
        HashMap::from([("bucket".to_string(), merged)]),
    );
    limiter.set_mps_per_bucket(&mps);
    limiter
}

#[test]
fn max_open_is_unknown_before_the_first_scan() {
    let limiter = SlowStartRateLimiter::new(Some(10));
    assert_eq!(limiter.get_max_open("bucket"), None);
}

#[test]
fn max_open_grows_with_merged_proposals() {
    assert_eq!(scanned(None, 0).get_max_open("bucket"), Some(1));
    assert_eq!(scanned(None, 4).get_max_open("bucket"), Some(5));
    assert_eq!(scanned(None, 4).get_max_open("other"), Some(1));
}

#[test]
fn max_open_is_capped_by_the_maximum() {
    assert_eq!(scanned(Some(3), 4).get_max_open("bucket"), Some(3));
    assert_eq!(scanned(Some(10), 4).get_max_open("bucket"), Some(5));
}

#[test]
fn a_maximum_of_zero_is_no_maximum() {
    assert_eq!(scanned(Some(0), 4).get_max_open("bucket"), Some(5));
}

#[test]
fn max_open_matches_check_allowed_when_the_maximum_binds() {
    let mut limiter = scanned(Some(3), 4);
    limiter.inc("bucket");
    limiter.inc("bucket");
    match limiter.check_allowed("bucket") {
        RateLimitStatus::BucketRateLimited { max_open_mps, .. } => {
            assert_eq!(Some(max_open_mps), limiter.get_max_open("bucket"));
        }
        other => panic!("expected the bucket to be limited, got {other:?}"),
    }
}
