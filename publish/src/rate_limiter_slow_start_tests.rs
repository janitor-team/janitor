use super::*;

const BUCKET: &str = "lintian-fixes";

fn limiter(max: Option<usize>, open: usize, merged: usize, applied: usize) -> SlowStartRateLimiter {
    let mut limiter = SlowStartRateLimiter::new(max);
    let mut counts = maplit::hashmap! {
        MergeProposalStatus::Open => maplit::hashmap! {},
        MergeProposalStatus::Merged => maplit::hashmap! {},
        MergeProposalStatus::Applied => maplit::hashmap! {},
    };
    for (status, count) in [
        (MergeProposalStatus::Open, open),
        (MergeProposalStatus::Merged, merged),
        (MergeProposalStatus::Applied, applied),
    ] {
        if count > 0 {
            counts
                .get_mut(&status)
                .unwrap()
                .insert(BUCKET.to_string(), count);
        }
    }
    limiter.set_mps_per_bucket(&counts);
    limiter
}

/// The limit a bucket was refused at, or `None` when it was allowed.
fn refused_at(limiter: &SlowStartRateLimiter) -> Option<usize> {
    match limiter.check_allowed(BUCKET) {
        RateLimitStatus::Allowed => None,
        RateLimitStatus::BucketRateLimited {
            bucket,
            open_mps: _,
            max_open_mps,
        } => {
            assert_eq!(bucket, BUCKET);
            Some(max_open_mps)
        }
        RateLimitStatus::RateLimited => panic!("counts are set, expected a bucket decision"),
    }
}

#[test]
fn refuses_everything_before_the_first_refresh() {
    for max in [None, Some(5)] {
        let status = SlowStartRateLimiter::new(max).check_allowed(BUCKET);
        assert!(matches!(status, RateLimitStatus::RateLimited), "{status}");
    }
}

#[test]
fn bucket_with_nothing_absorbed_gets_one_open_proposal() {
    assert_eq!(refused_at(&limiter(Some(10), 0, 0, 0)), None);
    assert_eq!(refused_at(&limiter(Some(10), 1, 0, 0)), Some(1));
    assert_eq!(refused_at(&limiter(Some(10), 2, 0, 0)), Some(1));
}

#[test]
fn limit_is_merged_plus_applied_plus_one() {
    assert_eq!(refused_at(&limiter(Some(10), 3, 2, 1)), None);
    assert_eq!(refused_at(&limiter(Some(10), 4, 2, 1)), Some(4));
    assert_eq!(refused_at(&limiter(Some(10), 5, 2, 1)), Some(4));
}

#[test]
fn overall_maximum_refuses_at_the_boundary() {
    assert_eq!(refused_at(&limiter(Some(3), 2, 10, 0)), None);
    assert_eq!(refused_at(&limiter(Some(3), 3, 10, 0)), Some(3));
    assert_eq!(refused_at(&limiter(Some(3), 4, 10, 0)), Some(3));
}

#[test]
fn slow_start_works_without_an_overall_maximum() {
    assert_eq!(refused_at(&limiter(None, 0, 0, 0)), None);
    assert_eq!(refused_at(&limiter(None, 2, 2, 0)), None);
    assert_eq!(refused_at(&limiter(None, 3, 2, 0)), Some(3));
}

#[test]
fn overall_maximum_of_zero_means_no_maximum() {
    assert_eq!(refused_at(&limiter(Some(0), 1, 2, 0)), None);
    assert_eq!(refused_at(&limiter(Some(0), 3, 2, 0)), Some(3));
}

#[test]
fn inc_counts_towards_the_limit() {
    let mut limiter = limiter(Some(10), 1, 1, 0);
    assert_eq!(refused_at(&limiter), None);
    limiter.inc(BUCKET);
    assert_eq!(refused_at(&limiter), Some(2));
}

#[test]
fn counts_without_an_open_entry_mean_nothing_open() {
    let mut limiter = SlowStartRateLimiter::new(Some(10));
    limiter.set_mps_per_bucket(&maplit::hashmap! {
        MergeProposalStatus::Merged => maplit::hashmap! { BUCKET.to_string() => 2 },
    });
    assert_eq!(refused_at(&limiter), None);
}

#[test]
fn get_stats_is_none_before_a_refresh() {
    assert!(SlowStartRateLimiter::new(Some(5)).get_stats().is_none());
}

#[test]
fn get_stats_reports_open_counts_with_nothing_absorbed() {
    let stats = limiter(Some(5), 3, 0, 0).get_stats().unwrap();
    assert_eq!(stats.per_bucket.get(BUCKET), Some(&3));
}

#[test]
fn get_stats_reports_open_counts_without_an_overall_maximum() {
    let stats = limiter(None, 2, 4, 0).get_stats().unwrap();
    assert_eq!(stats.per_bucket.get(BUCKET), Some(&2));
}

#[test]
fn get_stats_reports_the_same_open_counts_as_the_fixed_limiter() {
    let counts = maplit::hashmap! {
        MergeProposalStatus::Open => maplit::hashmap! { BUCKET.to_string() => 7 },
        MergeProposalStatus::Merged => maplit::hashmap! { BUCKET.to_string() => 1 },
    };
    let mut slow = SlowStartRateLimiter::new(Some(10));
    slow.set_mps_per_bucket(&counts);
    let mut fixed = FixedRateLimiter::new(10);
    fixed.set_mps_per_bucket(&counts);
    assert_eq!(
        slow.get_stats().unwrap().per_bucket,
        fixed.get_stats().unwrap().per_bucket
    );
    assert_eq!(slow.get_stats().unwrap().per_bucket.get(BUCKET), Some(&7));
}
