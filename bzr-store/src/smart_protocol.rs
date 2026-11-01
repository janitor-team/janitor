//! Bazaar smart protocol handler.
//!
//! Port of `py/janitor/bzr_store.py::bzr_backend`. Each request:
//!
//! 1. Checks the codebase exists in the `codebase` table (404 otherwise).
//! 2. When the request may write, opens the shared bzr repository at
//!    `<vcs_path>/<codebase>`, creating one via
//!    `ControlDir.create(...).create_repository(shared=True)` on
//!    `NotBranchError`. A read-only request never creates anything.
//! 3. Serves every request from a `breezyshim` `ChrootServer` rooted
//!    at the codebase directory, the same jail `brz serve` uses. The
//!    chroot clamps `..` (including percent-encoded forms) to that
//!    directory.
//!    When a campaign (and role) is given, the backing transport is
//!    cloned into the campaign/role sub-paths. The directories are
//!    created with `ensure_base()` only on a write-enabled request.
//! 4. When writes are not allowed, wraps the backing and jail
//!    transports in `readonly+<url>` so writes are rejected at the
//!    transport layer.
//! 5. Feeds the HTTP request body through
//!    `breezy.bzr.smart.medium._get_protocol_factory_for_bytes`,
//!    builds the protocol with `root_client_path=None` (the chroot,
//!    not path-prefix translation, is the boundary) and the chroot
//!    jail, then runs `accept_bytes(unused)`.
//! 6. If `next_read_size() != 0` after `accept_bytes`, returns the
//!    canonical incomplete-request body
//!    `b"error\x01incomplete request\n"`; otherwise returns the
//!    contents of the `BytesIO` buffer.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use breezyshim::bazaar::smart::ResponseBuffer;
use breezyshim::transport::{chroot::ChrootServer, get_transport};
use janitor::{Campaign, CodebaseName};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
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
///
/// The exchange is jailed to `repo_path` with a Breezy `ChrootServer`.
fn handle_exchange(
    repo_path: &std::path::Path,
    campaign: Option<&str>,
    role: Option<&str>,
    allow_writes: bool,
    body: &[u8],
) -> Result<Vec<u8>> {
    // Only a write-enabled request may create the shared repo.
    if allow_writes {
        open_or_create_shared_repo(repo_path)
            .map_err(|e| BzrError::Python(format!("open/create shared repo: {}", e)))?;
    }

    // A relative --vcs-path must still give a file URL.
    let abs_path = std::path::absolute(repo_path)?;
    let root_url = url::Url::from_file_path(&abs_path).map_err(|_| {
        BzrError::Python(format!(
            "repository path is not absolute: {}",
            abs_path.display()
        ))
    })?;

    let root = get_transport(&root_url, None)
        .map_err(|e| BzrError::Python(format!("get_transport({}): {}", root_url, e)))?;
    let server = ChrootServer::new(&root)
        .map_err(|e| BzrError::Python(format!("ChrootServer::new: {}", e)))?;
    server
        .start_server()
        .map_err(|e| BzrError::Python(format!("ChrootServer::start_server: {}", e)))?;

    let result = run_chrooted_exchange(&server, campaign, role, allow_writes, body)
        .map_err(|e| BzrError::Python(format!("smart protocol exchange: {}", e)));

    if let Err(e) = server.stop_server() {
        warn!("ChrootServer::stop_server({}): {}", root_url, e);
    }
    result
}

/// Run the exchange in the chroot of `server`, with `root_client_path` set to `None`.
#[allow(clippy::result_large_err)] // breezyshim::error::Error is intentionally large; not our choice.
fn run_chrooted_exchange(
    server: &ChrootServer,
    campaign: Option<&str>,
    role: Option<&str>,
    allow_writes: bool,
    body: &[u8],
) -> std::result::Result<Vec<u8>, breezyshim::error::Error> {
    let chroot_url = server.get_url()?;

    let mut backing = get_transport(&chroot_url, None)?;
    if let Some(c) = campaign {
        backing = backing.clone(c)?;
        if allow_writes {
            // ensure_base can fail if the parent is missing; log and
            // continue, matching Python's `transport.ensure_base()`
            // which also swallows a FileExists race.
            if let Err(e) = backing.ensure_base() {
                debug!("ensure_base({}): {}", c, e);
            }
        }
    }
    if let Some(r) = role {
        backing = backing.clone(r)?;
        if allow_writes {
            if let Err(e) = backing.ensure_base() {
                debug!("ensure_base({}): {}", r, e);
            }
        }
    }

    let mut jail = get_transport(&chroot_url, None)?;
    if !allow_writes {
        let readonly = |url: &url::Url| url::Url::parse(&format!("readonly+{}", url));
        backing = get_transport(&readonly(&backing.base())?, None)?;
        jail = get_transport(&readonly(&chroot_url)?, None)?;
    }

    let buffer = ResponseBuffer::new()?;
    let write_func = buffer.write_func()?;

    let next_size = Python::attach(|py| -> PyResult<usize> {
        let pair = py
            .import("breezy.bzr.smart.medium")?
            .getattr("_get_protocol_factory_for_bytes")?
            .call1((PyBytes::new(py, body),))?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("root_client_path", py.None())?;
        kwargs.set_item("jail_root", jail)?;
        let proto = pair
            .get_item(0)?
            .call((backing, write_func), Some(&kwargs))?;
        proto.call_method1("accept_bytes", (pair.get_item(1)?,))?;
        proto.call_method0("next_read_size")?.extract()
    })?;
    if next_size != 0 {
        debug!(
            "incomplete smart protocol request (next_read_size={})",
            next_size
        );
        return Ok(INCOMPLETE_REQUEST_ERROR.to_vec());
    }
    buffer.into_bytes()
}

/// Internal dispatch shared by the three route shapes. Checks the
/// codebase in the DB, validates the campaign against `janitor.conf`
/// campaigns, and runs the smart protocol exchange with per-app
/// `allow_writes` semantics.
pub async fn smart_protocol_dispatch(
    state: AppState,
    codebase: CodebaseName,
    campaign: Option<Campaign>,
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
            path: codebase.into_string(),
        });
    }

    if let Some(c) = campaign.as_ref() {
        if state.janitor_config.get_campaign(c).is_none() {
            warn!("unknown campaign: {}", c);
            return Err(BzrError::PathNotFound {
                path: format!("campaign/{}", c),
            });
        }
    }

    let allow_writes = state.resolve_allow_writes(&headers).await?;
    let repo_path = state.config.repository_path.join(&codebase);

    // Only a write-enabled request creates the shared repo on demand.
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
    Path(codebase): Path<CodebaseName>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    smart_protocol_dispatch(state, codebase, None, None, headers, body).await
}

/// `POST /:codebase/:campaign/.bzr/smart`
pub async fn smart_protocol_campaign_handler(
    State(state): State<AppState>,
    Path((codebase, campaign)): Path<(CodebaseName, Campaign)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    smart_protocol_dispatch(state, codebase, Some(campaign), None, headers, body).await
}

/// `POST /:codebase/:campaign/:role/.bzr/smart`
pub async fn smart_protocol_role_handler(
    State(state): State<AppState>,
    Path((codebase, campaign, role)): Path<(CodebaseName, Campaign, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    smart_protocol_dispatch(state, codebase, Some(campaign), Some(role), headers, body).await
}
