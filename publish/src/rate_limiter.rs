//! Rate limiter module for the publish crate.
//!
//! This module provides rate limiting functionality for merge proposals.

use janitor::publish::MergeProposalStatus;
use std::collections::HashMap;

/// Status of a rate limit check.
#[derive(Debug)]
pub enum RateLimitStatus {
    /// The operation is allowed.
    Allowed,
    /// The operation is rate limited.
    RateLimited,
    /// The operation is rate limited due to bucket limits.
    BucketRateLimited {
        /// The bucket that is rate limited.
        bucket: String,
        /// The current number of open merge proposals.
        open_mps: usize,
        /// The maximum number of open merge proposals allowed.
        max_open_mps: usize,
    },
}

impl RateLimitStatus {
    /// Check if the operation is allowed.
    ///
    /// # Returns
    /// `true` if the operation is allowed, `false` otherwise
    pub fn is_allowed(&self) -> bool {
        match self {
            RateLimitStatus::Allowed => true,
            _ => false,
        }
    }
}

impl std::fmt::Display for RateLimitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            RateLimitStatus::Allowed => write!(f, "Allowed"),
            RateLimitStatus::RateLimited => write!(f, "RateLimited"),
            RateLimitStatus::BucketRateLimited {
                bucket,
                open_mps,
                max_open_mps,
            } => write!(
                f,
                "BucketRateLimited: bucket={}, open_mps={}, max_open_mps={}",
                bucket, open_mps, max_open_mps
            ),
        }
    }
}

/// Statistics about rate limiting.
pub struct RateLimitStats {
    /// Number of merge proposals per bucket.
    pub per_bucket: HashMap<String, usize>,
}

/// Trait for rate limiters.
pub trait RateLimiter: Send + Sync {
    /// Set the number of merge proposals per bucket.
    ///
    /// # Arguments
    /// * `mps_per_bucket` - Map of merge proposal status to map of bucket to count
    fn set_mps_per_bucket(
        &mut self,
        mps_per_bucket: &HashMap<MergeProposalStatus, HashMap<String, usize>>,
    );

    /// Check if an operation is allowed for a bucket.
    ///
    /// # Arguments
    /// * `bucket` - The bucket to check
    ///
    /// # Returns
    /// The rate limit status
    fn check_allowed(&self, bucket: &str) -> RateLimitStatus;

    /// Increment the count for a bucket.
    ///
    /// # Arguments
    /// * `bucket` - The bucket to increment
    fn inc(&mut self, bucket: &str);

    /// Get rate limit statistics.
    ///
    /// # Returns
    /// Rate limit statistics, if available
    fn get_stats(&self) -> Option<RateLimitStats>;

    /// Get the maximum number of open merge proposals for a bucket.
    ///
    /// # Arguments
    /// * `bucket` - The bucket to check
    ///
    /// # Returns
    /// The maximum number of open merge proposals, if available
    fn get_max_open(&self, bucket: &str) -> Option<usize> {
        None
    }
}

/// Rate limiter that always allows operations.
pub struct NonRateLimiter;

impl Default for NonRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl NonRateLimiter {
    /// Create a new NonRateLimiter.
    ///
    /// # Returns
    /// A new NonRateLimiter instance
    pub fn new() -> Self {
        NonRateLimiter
    }
}

impl RateLimiter for NonRateLimiter {
    fn set_mps_per_bucket(
        &mut self,
        _mps_per_bucket: &HashMap<MergeProposalStatus, HashMap<String, usize>>,
    ) {
    }

    fn check_allowed(&self, _bucket: &str) -> RateLimitStatus {
        RateLimitStatus::Allowed
    }

    fn inc(&mut self, _bucket: &str) {}

    fn get_stats(&self) -> Option<RateLimitStats> {
        None
    }
}

/// Rate limiter with a fixed maximum number of merge proposals per bucket.
pub struct FixedRateLimiter {
    /// Maximum number of merge proposals per bucket.
    max_mps_per_bucket: usize,
    /// Current number of open merge proposals per bucket.
    open_mps_per_bucket: Option<HashMap<String, usize>>,
}

impl FixedRateLimiter {
    /// Create a new FixedRateLimiter.
    ///
    /// # Arguments
    /// * `max_mps_per_bucket` - Maximum number of merge proposals per bucket
    ///
    /// # Returns
    /// A new FixedRateLimiter instance
    pub fn new(max_mps_per_bucket: usize) -> Self {
        FixedRateLimiter {
            max_mps_per_bucket,
            open_mps_per_bucket: None,
        }
    }
}

impl RateLimiter for FixedRateLimiter {
    fn set_mps_per_bucket(
        &mut self,
        mps_per_bucket: &HashMap<MergeProposalStatus, HashMap<String, usize>>,
    ) {
        self.open_mps_per_bucket = mps_per_bucket.get(&MergeProposalStatus::Open).cloned();
    }

    fn check_allowed(&self, bucket: &str) -> RateLimitStatus {
        if let Some(open_mps_per_bucket) = &self.open_mps_per_bucket {
            if let Some(current) = open_mps_per_bucket.get(bucket) {
                if *current > self.max_mps_per_bucket {
                    return RateLimitStatus::BucketRateLimited {
                        bucket: bucket.to_string(),
                        open_mps: *current,
                        max_open_mps: self.max_mps_per_bucket,
                    };
                }
            }
        } else {
            // Be conservative
            return RateLimitStatus::RateLimited;
        }
        RateLimitStatus::Allowed
    }

    fn inc(&mut self, bucket: &str) {
        if let Some(open_mps_per_bucket) = self.open_mps_per_bucket.as_mut() {
            open_mps_per_bucket
                .entry(bucket.to_string())
                .and_modify(|e| *e += 1)
                .or_insert(1);
        }
    }

    fn get_stats(&self) -> Option<RateLimitStats> {
        self.open_mps_per_bucket
            .as_ref()
            .map(|open_mps_per_bucket| RateLimitStats {
                per_bucket: open_mps_per_bucket.clone(),
            })
    }
}

/// Rate limiter that gradually increases the limit based on absorbed merge proposals.
pub struct SlowStartRateLimiter {
    /// Optional maximum number of merge proposals per bucket.
    max_mps_per_bucket: Option<usize>,
    /// Current number of open merge proposals per bucket.
    open_mps_per_bucket: Option<HashMap<String, usize>>,
    /// Number of absorbed (merged or applied) merge proposals per bucket.
    absorbed_mps_per_bucket: Option<HashMap<String, usize>>,
}

impl SlowStartRateLimiter {
    /// Create a new SlowStartRateLimiter.
    ///
    /// # Arguments
    /// * `max_mps_per_bucket` - Optional maximum number of merge proposals per bucket
    ///
    /// # Returns
    /// A new SlowStartRateLimiter instance
    pub fn new(max_mps_per_bucket: Option<usize>) -> Self {
        SlowStartRateLimiter {
            max_mps_per_bucket,
            open_mps_per_bucket: None,
            absorbed_mps_per_bucket: None,
        }
    }

    /// Get the limit for a bucket based on the number of absorbed merge proposals.
    ///
    /// # Arguments
    /// * `bucket` - The bucket to get the limit for
    ///
    /// # Returns
    /// The limit for the bucket, if available
    fn get_limit(&self, bucket: &str) -> Option<usize> {
        if let Some(absorbed_mps_per_bucket) = &self.absorbed_mps_per_bucket {
            absorbed_mps_per_bucket.get(bucket).map(|c| c + 1)
        } else {
            None
        }
    }
}

impl RateLimiter for SlowStartRateLimiter {
    fn check_allowed(&self, bucket: &str) -> RateLimitStatus {
        if let Some(max_mps_per_bucket) = self.max_mps_per_bucket {
            if let Some(open_mps_per_bucket) = &self.open_mps_per_bucket {
                if let Some(current) = open_mps_per_bucket.get(bucket) {
                    if *current > max_mps_per_bucket {
                        return RateLimitStatus::BucketRateLimited {
                            bucket: bucket.to_string(),
                            open_mps: *current,
                            max_open_mps: max_mps_per_bucket,
                        };
                    }
                }
            } else {
                // Be conservative
                return RateLimitStatus::RateLimited;
            }
        } else {
            // Be conservative
            return RateLimitStatus::RateLimited;
        }
        RateLimitStatus::Allowed
    }

    fn inc(&mut self, bucket: &str) {
        if let Some(open_mps_per_bucket) = self.open_mps_per_bucket.as_mut() {
            open_mps_per_bucket
                .entry(bucket.to_string())
                .and_modify(|e| *e += 1)
                .or_insert(1);
        }
    }

    fn set_mps_per_bucket(
        &mut self,
        mps_per_bucket: &HashMap<MergeProposalStatus, HashMap<String, usize>>,
    ) {
        self.open_mps_per_bucket = mps_per_bucket.get(&MergeProposalStatus::Open).cloned();
        let mut absorbed_mps_per_bucket = HashMap::new();
        for status in [MergeProposalStatus::Merged, MergeProposalStatus::Applied] {
            for (bucket, count) in mps_per_bucket.get(&status).unwrap_or(&HashMap::new()) {
                absorbed_mps_per_bucket
                    .entry(bucket.to_string())
                    .and_modify(|e| *e += count)
                    .or_insert(*count);
            }
        }
        self.absorbed_mps_per_bucket = Some(absorbed_mps_per_bucket);
    }

    fn get_stats(&self) -> Option<RateLimitStats> {
        self.open_mps_per_bucket
            .as_ref()
            .map(|open_mps_per_bucket| RateLimitStats {
                per_bucket: open_mps_per_bucket
                    .iter()
                    .map(|(k, _v)| {
                        (
                            k.clone(),
                            std::cmp::min(
                                self.max_mps_per_bucket.unwrap(),
                                self.get_limit(k).unwrap(),
                            ),
                        )
                    })
                    .collect(),
            })
    }
}
