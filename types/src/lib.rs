//! Core types shared between the janitor crates.

pub mod identifiers;

pub use identifiers::{
    Campaign, ChangeSetId, CodebaseName, InvalidIdentifier, LogFilename, PublishId, Role, RunId,
};

#[cfg(feature = "breezyshim")]
pub use breezyshim::RevisionId;
