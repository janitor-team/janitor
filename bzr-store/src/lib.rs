//! BZR Store service for the Janitor project.
//!
//! Serves an admin and a public HTTP interface to a directory of
//! per-codebase shared bzr repositories. Admin is unauthenticated;
//! public gates smart-protocol writes on HTTP Basic worker
//! credentials and offers a per-branch loggerhead HTML browser.
//!
//! Bazaar access is delegated to the Python `breezy` library via the
//! `breezyshim` crate, matching how the original
//! `py/janitor/bzr_store.py` deployed.

#![deny(missing_docs)]

pub mod api_types;
pub mod bzr_browse;
pub mod config;
pub mod database;
pub mod error;
pub mod pyo3_bridge;
pub mod repository;
pub mod smart_protocol;
pub mod web;
pub mod web_utils;

pub use config::Config;
pub use error::{BzrError, Result};
pub use pyo3_bridge::BreezyOperations;

/// Version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
