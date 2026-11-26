//! API types for the auto-upload service.

use crate::RunId;
use serde::{Deserialize, Serialize};

/// Build result message from the runner service.
///
/// Matches the shape of `ActiveRun.result.json()` on the runner side. Only the
/// fields consulted by auto-upload are modelled; the rest of the payload is
/// ignored. `target` is `null` when the run had no builder result.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BuildResultMessage {
    /// Unique identifier for the build run.
    pub log_id: RunId,
    /// Runner result code (e.g. `"success"`, `"nothing-to-do"`,
    /// `"codemod-error"`). Non-success runs publish `target: null`, so
    /// we filter them out before dispatching on `target`.
    #[serde(default)]
    pub code: Option<String>,
    /// `None` when the run had no builder result.
    #[serde(default)]
    pub target: Option<BuildTarget>,
}

/// Target field of a build result. Known targets are the only accepted
/// shapes; an unknown or malformed `target` fails deserialisation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "name", content = "details", rename_all = "lowercase")]
pub enum BuildTarget {
    /// Debian build target.
    Debian(DebianTargetDetails),
    /// Generic build target.
    Generic(GenericTargetDetails),
}

/// Debian target details; auto-upload only reads `build_distribution`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DebianTargetDetails {
    /// Debian suite/distribution the build targets.
    pub build_distribution: String,
    /// Everything else the runner sends; kept for round-trip fidelity.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Generic target details. The runner emits `{}` here; auto-upload has
/// nothing to do with generic builds beyond rejecting them.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GenericTargetDetails {}

impl super::PubSubMessage for BuildResultMessage {
    fn channel() -> &'static str {
        "result"
    }
}
