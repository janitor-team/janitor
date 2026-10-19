//! # APT Repository Library
//!
//! A Rust library for creating and parsing APT repositories. This library provides
//! functionality to generate repository metadata files (Release, Packages, Sources)
//! with proper cryptographic hashing and compression support.
//!
//! ## Features
//!
//! - Generate APT repository metadata files
//! - Support for multiple compression formats (gzip, bzip2, uncompressed)
//! - Cryptographic hashing (MD5, SHA1, SHA256, SHA512)
//! - By-hash repository structure support
//! - Release file generation with GPG signing preparation
//! - Async support for file operations
//!
//! ## Example
//!
//! ```rust
//! use apt_repository::RepositoryBuilder;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let repo = RepositoryBuilder::new()
//!     .origin("Example Origin")
//!     .label("Example Repository")
//!     .suite("stable")
//!     .codename("stable")
//!     .architectures(vec!["amd64".to_string(), "i386".to_string()])
//!     .components(vec!["main".to_string()])
//!     .build()?;
//!
//! // Generate repository files with
//! // AsyncRepository::new(repo).generate_repository(path, &packages, &sources)
//! # Ok(())
//! # }
//! ```

pub mod compression;
pub mod error;
pub mod hash;
pub mod packages;
pub mod release;
pub mod repository;
pub mod sources;

#[cfg(feature = "async")]
pub mod async_repository;

pub use compression::Compression;
pub use error::{AptRepositoryError, Result};
pub use hash::{HashAlgorithm, HashSet, HashedFile};
pub use packages::{new_package, Package, PackageFile};
pub use release::{Release, ReleaseBuilder};
pub use repository::{Repository, RepositoryBuilder};
pub use sources::{new_source, Source, SourceFile};

#[cfg(feature = "async")]
pub use async_repository::{AsyncPackageProvider, AsyncRepository, AsyncSourceProvider};

/// Default compression formats used for repository files
pub const DEFAULT_COMPRESSIONS: &[Compression] =
    &[Compression::None, Compression::Gzip, Compression::Bzip2];

/// Default hash algorithms used for by-hash repositories
pub const DEFAULT_HASH_ALGORITHMS: &[HashAlgorithm] = &[
    HashAlgorithm::Md5,
    HashAlgorithm::Sha1,
    HashAlgorithm::Sha256,
    HashAlgorithm::Sha512,
];
