use super::*;
use std::collections::HashMap;

/// A limiter with a maximum of `max` that has seen `open` open proposals in `bucket`.
fn scanned(max: usize, open: usize) -> FixedRateLimiter {
    let mut limiter = FixedRateLimiter::new(max);
    let mut mps = HashMap::new();
    mps.insert(
        MergeProposalStatus::Open,
        HashMap::from([("bucket".to_string(), open)]),
    );
    limiter.set_mps_per_bucket(&mps);
    limiter
}

#[test]
fn allowed_below_the_maximum() {
    assert!(scanned(3, 2).check_allowed("bucket").is_allowed());
    assert!(scanned(3, 2).check_allowed("other").is_allowed());
}

#[test]
fn limited_at_the_maximum() {
    match scanned(3, 3).check_allowed("bucket") {
        RateLimitStatus::BucketRateLimited {
            bucket,
            open_mps,
            max_open_mps,
        } => {
            assert_eq!(bucket, "bucket");
            assert_eq!(open_mps, 3);
            assert_eq!(max_open_mps, 3);
        }
        other => panic!("expected the bucket to be limited, got {other:?}"),
    }
}

#[test]
fn limited_after_reaching_the_maximum() {
    let mut limiter = scanned(3, 2);
    assert!(limiter.check_allowed("bucket").is_allowed());
    limiter.inc("bucket");
    assert!(!limiter.check_allowed("bucket").is_allowed());
}

#[test]
fn allowed_after_a_scan_with_no_open_proposals() {
    let mut limiter = FixedRateLimiter::new(3);
    limiter.set_mps_per_bucket(&HashMap::new());
    assert!(limiter.check_allowed("bucket").is_allowed());
}

#[test]
fn limited_before_the_first_scan() {
    let limiter = FixedRateLimiter::new(3);
    assert!(!limiter.check_allowed("bucket").is_allowed());
}
