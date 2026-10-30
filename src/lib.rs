pub mod analyze_log;
pub mod api;
pub mod artifacts;
pub mod auth;
pub mod config;
pub mod database;
pub mod debdiff;
pub mod error;
pub mod identifiers;
pub mod logging;
pub mod logs;
#[cfg(feature = "otlp")]
pub mod otlp;
pub mod pagination;
pub mod prometheus;
pub mod publish;
pub mod queue;
pub mod redis;
pub mod reprocess_logs;
pub mod review;
pub mod schedule;
pub mod schema;
pub mod security;
pub mod shared_config;
pub mod state;
#[cfg(feature = "testing")]
pub mod test_utils;
pub mod tracing;
pub mod utils;
pub mod vcs;
pub mod worker_auth;

pub use identifiers::{Campaign, ChangeSetId, CodebaseName, InvalidIdentifier, PublishId, RunId};

/// Open the object store at `url`, rooted at `prefix`, with `scheme` and
/// `prefix` as returned by `ObjectStoreScheme::parse`.
///
/// S3 takes its endpoint, region and credentials from the `AWS_*`
/// environment variables. Other schemes go to `object_store::parse_url`.
pub(crate) fn open_object_store(
    url: &url::Url,
    scheme: object_store::ObjectStoreScheme,
    prefix: object_store::path::Path,
) -> object_store::Result<std::sync::Arc<dyn object_store::ObjectStore>> {
    let store: Box<dyn object_store::ObjectStore> = match scheme {
        object_store::ObjectStoreScheme::AmazonS3 => Box::new(
            object_store::aws::AmazonS3Builder::from_env()
                .with_url(url.as_str())
                .with_allow_http(true)
                .build()?,
        ),
        _ => object_store::parse_url(url)?.0,
    };
    Ok(std::sync::Arc::new(object_store::prefix::PrefixStore::new(
        store, prefix,
    )))
}
