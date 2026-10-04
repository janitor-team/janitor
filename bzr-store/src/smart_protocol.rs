//! Bazaar smart protocol handler.
//!
//! Port of `py/janitor/bzr_store.py::bzr_backend`. Each request:
//!
//! 1. Checks the codebase exists in the `codebase` table (404 otherwise).
//! 2. Opens the shared bzr repository at `<vcs_path>/<codebase>`,
//!    creating one via `ControlDir.create(...).create_repository(shared=True)`
//!    on `NotBranchError`.
//! 3. Starts from `repo.user_transport` and, when a campaign (and
//!    role) is given, clones the transport into the campaign/role
//!    sub-paths. When `allow_writes`, the sub-path directories are
//!    created with `transport.ensure_base()`.
//! 4. When `allow_writes` is false, wraps the backing transport in
//!    `readonly+<url>` so writes are rejected at the transport layer.
//! 5. Feeds the HTTP request body through
//!    [`breezyshim::bazaar::smart::detect_protocol_factory`],
//!    builds the protocol with the backing + jail transports, and
//!    runs `accept_bytes(unused)`.
//! 6. If `next_read_size() != 0` after `accept_bytes`, returns the
//!    canonical incomplete-request body
//!    `b"error\x01incomplete request\n"`; otherwise returns the
//!    contents of the `BytesIO` buffer.
//!
//! The whole handler goes through typed `breezyshim` wrappers now —
//! no raw PyO3 remains outside the `BytesIO` wiring (which exists
//! only because the protocol writes to a Python-callable sink).

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use breezyshim::bazaar::smart;
use breezyshim::repository::Repository;
use breezyshim::transport::Transport;
use tracing::{debug, info, warn};

use crate::error::{BzrError, Result};
use crate::web::AppState;

/// Canonical incomplete-request error body emitted by Python.
const INCOMPLETE_REQUEST_ERROR: &[u8] = b"error\x01incomplete request\n";

/// Open the shared bzr repo at `repo_path`, creating one on
/// `NotBranchError`. Matches Python's `_bzr_open_repo` logic.
#[allow(clippy::result_large_err)] // breezyshim::error::Error is intentionally large; not our choice.
fn open_or_create_shared_repo(
    repo_path: &std::path::Path,
) -> std::result::Result<breezyshim::repository::GenericRepository, breezyshim::error::Error> {
    match breezyshim::repository::open(repo_path) {
        Ok(repo) => Ok(repo),
        Err(breezyshim::error::Error::NotBranchError(_, _)) => {
            let controldir = breezyshim::controldir::create(repo_path, "2a", None)?;
            controldir.create_repository(Some(true))
        }
        Err(other) => Err(other),
    }
}

/// Run one smart protocol exchange for the given on-disk repo path
/// and optional campaign/role transport sub-paths. Returns the raw
/// response bytes to send back with
/// `content-type: application/octet-stream`.
fn handle_exchange(
    repo_path: &std::path::Path,
    campaign: Option<&str>,
    role: Option<&str>,
    allow_writes: bool,
    body: &[u8],
) -> Result<Vec<u8>> {
    // Open the shared repo through breezyshim; this handles
    // NotBranchError -> auto-create inline.
    let repo = open_or_create_shared_repo(repo_path)
        .map_err(|e| BzrError::Python(format!("open/create shared repo: {}", e)))?;

    // Walk from the repo's user_transport into the campaign (and
    // optionally role) sub-paths. Each `clone` returns a new typed
    // Transport; `ensure_base` creates the directory on write-enabled
    // requests, matching the Python branch.
    let user_transport: Transport = repo.user_transport();
    let mut transport: Transport = user_transport
        .clone(".")
        .map_err(|e| BzrError::Python(format!("initial transport.clone(.): {}", e)))?;
    if let Some(c) = campaign {
        transport = transport
            .clone(c)
            .map_err(|e| BzrError::Python(format!("transport.clone({}): {}", c, e)))?;
        if allow_writes {
            // ensure_base can fail if the parent is missing; log and
            // continue, matching Python's `transport.ensure_base()`
            // which also swallows a FileExists race.
            if let Err(e) = transport.ensure_base() {
                debug!("ensure_base({}): {}", c, e);
            }
        }
    }
    if let Some(r) = role {
        transport = transport
            .clone(r)
            .map_err(|e| BzrError::Python(format!("transport.clone({}): {}", r, e)))?;
        if allow_writes {
            if let Err(e) = transport.ensure_base() {
                debug!("ensure_base({}): {}", r, e);
            }
        }
    }

    // Wrap in `readonly+` when writes aren't allowed. url::Url parses
    // `readonly+file://...` fine (scheme becomes `readonly+file`), and
    // breezyshim's `get_transport` passes it through to Python as a
    // string so the compound scheme is preserved.
    let backing_transport = if allow_writes {
        transport
    } else {
        let base = transport.base();
        let readonly_url = url::Url::parse(&format!("readonly+{}", base))
            .map_err(|e| BzrError::Python(format!("readonly url parse: {}", e)))?;
        breezyshim::transport::get_transport(&readonly_url, None)
            .map_err(|e| BzrError::Python(format!("get_transport(readonly+...): {}", e)))?
    };

    let buffer = smart::ResponseBuffer::new()
        .map_err(|e| BzrError::Python(format!("ResponseBuffer::new: {}", e)))?;
    let write_func = buffer
        .write_func()
        .map_err(|e| BzrError::Python(format!("ResponseBuffer::write_func: {}", e)))?;

    let (factory, unused) = smart::detect_protocol_factory(body)
        .map_err(|e| BzrError::Python(format!("detect_protocol_factory: {}", e)))?;

    let proto = factory
        .build(&backing_transport, write_func, ".", &user_transport)
        .map_err(|e| BzrError::Python(format!("build smart protocol: {}", e)))?;

    proto
        .accept_bytes(&unused)
        .map_err(|e| BzrError::Python(format!("accept_bytes: {}", e)))?;

    let next_size = proto
        .next_read_size()
        .map_err(|e| BzrError::Python(format!("next_read_size: {}", e)))?;
    if next_size != 0 {
        debug!(
            "incomplete smart protocol request (next_read_size={})",
            next_size
        );
        return Ok(INCOMPLETE_REQUEST_ERROR.to_vec());
    }

    buffer
        .into_bytes()
        .map_err(|e| BzrError::Python(format!("ResponseBuffer::into_bytes: {}", e)))
}

/// Internal dispatch shared by the three route shapes. Checks the
/// codebase in the DB, validates the campaign against `janitor.conf`
/// campaigns, and runs the smart protocol exchange with per-app
/// `allow_writes` semantics.
pub async fn smart_protocol_dispatch(
    state: AppState,
    codebase: String,
    campaign: Option<String>,
    role: Option<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    info!(
        "smart protocol request codebase={} campaign={:?} role={:?}",
        codebase, campaign, role
    );

    if !state.database.validate_codebase(&codebase).await? {
        warn!("unknown codebase: {}", codebase);
        return Err(BzrError::PathNotFound {
            path: codebase.clone(),
        });
    }

    if let Some(c) = campaign.as_deref() {
        if state.janitor_config.get_campaign(c).is_none() {
            warn!("unknown campaign: {}", c);
            return Err(BzrError::PathNotFound {
                path: format!("campaign/{}", c),
            });
        }
    }

    let allow_writes = state.resolve_allow_writes(&headers).await?;
    let repo_path = state.config.repository_path.join(&codebase);

    // Python's flow creates the shared repo on demand (NotBranchError
    // -> ControlDir.create -> create_repository(shared=True)). Don't
    // 404 on missing fs dir; Breezy will make it.
    let body_vec = body.to_vec();
    let response_bytes = tokio::task::spawn_blocking(move || {
        handle_exchange(
            &repo_path,
            campaign.as_deref(),
            role.as_deref(),
            allow_writes,
            &body_vec,
        )
    })
    .await
    .map_err(|e| BzrError::internal(format!("smart protocol task join: {}", e)))??;

    debug!("smart protocol response: {} bytes", response_bytes.len());

    Ok((
        StatusCode::OK,
        [("content-type", "application/octet-stream")],
        response_bytes,
    )
        .into_response())
}

/// `POST /:codebase/.bzr/smart`
pub async fn smart_protocol_codebase_handler(
    State(state): State<AppState>,
    Path(codebase): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    smart_protocol_dispatch(state, codebase, None, None, headers, body).await
}

/// `POST /:codebase/:campaign/.bzr/smart`
pub async fn smart_protocol_campaign_handler(
    State(state): State<AppState>,
    Path((codebase, campaign)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    smart_protocol_dispatch(state, codebase, Some(campaign), None, headers, body).await
}

/// `POST /:codebase/:campaign/:role/.bzr/smart`
pub async fn smart_protocol_role_handler(
    State(state): State<AppState>,
    Path((codebase, campaign, role)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    smart_protocol_dispatch(state, codebase, Some(campaign), Some(role), headers, body).await
}
