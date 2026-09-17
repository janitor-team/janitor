//! Janitor archive service: APT repository generation and HTTP serving.

#![deny(missing_docs)]

pub use tracing;

/// Prefix for temporary directories created during scan/publish.
pub const TMP_PREFIX: &str = "janitor-apt";
/// Default GCS artifact-retrieval timeout in seconds.
pub const DEFAULT_GCS_TIMEOUT: usize = 60 * 30;

/// Archive service configuration.
pub mod config;
/// `Contents-<arch>` file formatter (dak-format file->package index).
pub mod contents;
/// Postgres queries against `debian_build` for repo generation.
pub mod database;
/// `.deb` file listing via in-process `ar` + `tar` parsing.
pub mod deb;
/// Typed errors returned by every fallible operation in the crate.
pub mod error;
/// Background scheduler that turns runner pub/sub events into republishes.
pub mod manager;
/// On-demand `/dists/{kind}/{id}/...` generation.
pub mod on_demand;
/// Periodic republish + housekeeping loops.
pub mod periodic;
/// Runner pub/sub listener.
pub mod redis;
/// APT repository builder: writes Release, Packages, Sources, Contents.
pub mod repository;
/// Downloads build artifacts and runs `dpkg-scanpackages`/`dpkg-scansources`.
pub mod scanner;
/// GPG signing for `Release.gpg` and `InRelease`.
pub mod sign;
/// HTTP handlers and axum wiring.
pub mod web;

pub use error::{ArchiveError, ArchiveResult};
pub use manager::{
    GeneratorManager, GeneratorManagerConfig, JobInfo, JobStatus, ManagerStatistics,
};
pub use periodic::{HealthCheck, HealthStatus, PeriodicConfig, PeriodicServices, ServiceMetrics};
pub use redis::{ArchiveEvent, RedisPublisher, RedisSubscriber};
pub use repository::{RepositoryGenerationConfig, RepositoryGenerator};
pub use scanner::{BuildInfo, PackageScanner};
pub use web::{AppState, ArchiveWebService, PublishRequest, PublishResponse};
