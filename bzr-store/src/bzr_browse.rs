//! Loggerhead-backed browse handlers for `/bzr/{codebase}/...`.
//!
//! Per-codebase `loggerhead` routers are built on demand and cached in
//! `BrowseRouters` (insert-once map). Each request strips the
//! `/bzr/{codebase}` prefix from the URI, dispatches via a `tower`
//! oneshot into the cached `Router`, and forwards loggerhead's
//! response back.
//!
//! This replaces the Python loggerhead WSGI bridge that the Python
//! `bzr_store` service depended on externally for HTML browsing. The
//! bzr smart protocol POSTs (`/bzr/<codebase>/.bzr/smart`) are routed
//! by `smart_protocol_dispatch` and don't come through this module.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

use crate::error::{BzrError, Result};
use crate::web::AppState;
use janitor_types::CodebaseName;

/// Insert-once cache of `(codebase) -> loggerhead Router`. Routers are
/// cheap to build but they pay a Python-GIL hop to open the branch on
/// first use, so we hold them across requests.
#[derive(Clone, Default)]
pub struct BrowseRouters {
    inner: Arc<RwLock<std::collections::HashMap<CodebaseName, Arc<axum::Router>>>>,
}

impl BrowseRouters {
    /// Create an empty cache. Routers are inserted lazily on first
    /// browse request per codebase.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fast path: return the cached router for `codebase` without
    /// building. `None` means no router has been built yet.
    pub fn get(&self, codebase: &CodebaseName) -> Option<Arc<axum::Router>> {
        self.inner.read().unwrap().get(codebase).cloned()
    }

    fn get_or_build(
        &self,
        codebase: &CodebaseName,
        repo_path: &std::path::Path,
        static_dir: &std::path::Path,
    ) -> Arc<axum::Router> {
        if let Some(r) = self.inner.read().unwrap().get(codebase).cloned() {
            return r;
        }
        let lh_state = Arc::new(loggerhead::app::AppState::new(
            repo_path.to_string_lossy().into_owned(),
            None,
            false,
            static_dir.to_path_buf(),
            false,
            None,
            format!("/bzr/{}", codebase),
        ));
        let router = Arc::new(loggerhead::app::build_router(lh_state));
        self.inner
            .write()
            .unwrap()
            .entry(codebase.clone())
            .or_insert_with(|| router.clone())
            .clone()
    }
}

/// The codebase of a `/{codebase}/{rest}` path, or `None` for the bare `/` path.
fn split_codebase(path: &str) -> Result<Option<CodebaseName>> {
    let Some(path) = path.strip_prefix('/').filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let name = path.split_once('/').map_or(path, |(name, _)| name);
    let codebase = CodebaseName::new(name).map_err(|e| BzrError::invalid_request(e.to_string()))?;
    Ok(Some(codebase))
}

/// `GET /bzr/{codebase}/...`, the fallback of the router nested at `/bzr`.
pub async fn bzr_browse_handler(State(state): State<AppState>, req: Request) -> Result<Response> {
    let Some(codebase) = split_codebase(req.uri().path())? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    dispatch(&state, &codebase, req).await
}

async fn dispatch(state: &AppState, codebase: &CodebaseName, mut req: Request) -> Result<Response> {
    let repo_path = state.config.repository_path.join(codebase);
    if !repo_path.exists() {
        return Ok((
            StatusCode::NOT_FOUND,
            format!("Unknown codebase: {codebase}"),
        )
            .into_response());
    }

    let static_dir: PathBuf = match state.loggerhead_static_dir.clone() {
        Some(d) => d,
        None => {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                "loggerhead static assets not configured",
            )
                .into_response());
        }
    };

    let router = state
        .browse_routers
        .get_or_build(codebase, &repo_path, &static_dir);

    // Strip `/<codebase>` from the request URI so loggerhead's
    // routes (mounted at `/`, `/changes`, `/atom`, etc) match.
    let prefix = format!("/{}", codebase);
    let original = req.uri();
    let rest = original.path().strip_prefix(&prefix).unwrap_or("");
    let rest = if rest.is_empty() { "/" } else { rest };
    let new_path_and_query = match original.query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest.to_string(),
    };
    let mut parts = original.clone().into_parts();
    parts.path_and_query = Some(new_path_and_query.parse().unwrap());
    *req.uri_mut() = Uri::from_parts(parts).unwrap();

    let response = match (*router).clone().oneshot(req).await {
        Ok(r) => r,
        Err(e) => match e {},
    };

    // Convert loggerhead's response body type into axum::body::Body so
    // it slots back into the bzr-store response stream.
    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(parts, Body::new(body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_none_for_unknown_codebase() {
        let cache = BrowseRouters::new();
        assert!(cache.get(&CodebaseName::new("nothing").unwrap()).is_none());
    }

    #[test]
    fn split_codebase_valid() {
        let foo = Some(CodebaseName::new("foo").unwrap());
        for (path, expected) in [
            ("", None),
            ("/", None),
            ("/foo", foo.clone()),
            ("/foo/", foo.clone()),
            ("/foo/changes/1", foo.clone()),
            ("/foo/view/1/README", foo.clone()),
            ("/foo/main/.bzr/smart", foo.clone()),
            ("/g++/files", Some(CodebaseName::new("g++").unwrap())),
        ] {
            assert_eq!(split_codebase(path).unwrap(), expected, "{path}");
        }
    }

    #[test]
    fn split_codebase_invalid() {
        for (path, name) in [
            ("//changes", ""),
            ("/Foo", "Foo"),
            ("/../changes", ".."),
            ("/foo_bar/changes/1", "foo_bar"),
            ("/%66oo/changes", "%66oo"),
        ] {
            assert_eq!(
                split_codebase(path).unwrap_err().to_string(),
                format!("Invalid request: invalid codebase name: {name:?}"),
                "{path}"
            );
        }
    }
}
