//! Klaus (Flask) HTML repo browser, invoked as a WSGI app via PyO3.
//!
//! Smart-protocol requests are recognised by
//! [`is_git_client_request`] and dispatched to
//! [`crate::git_http::git_backend`] instead.

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use std::path::Path;
use tracing::{debug, warn};

use crate::error::{GitStoreError, Result};
use crate::web::AppState;

/// Cap on request body we buffer for the WSGI call.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// True if this request should go to `git http-backend` instead of
/// klaus (dispatched by the smart-protocol URL suffix, service query
/// param, or a git/breezy `User-Agent`).
pub fn is_git_client_request(req: &Request) -> bool {
    let path = req.uri().path();
    if path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
    {
        return true;
    }
    if let Some(q) = req.uri().query() {
        if q.contains("service=git-upload-pack") || q.contains("service=git-receive-pack") {
            return true;
        }
    }
    if let Some(ua) = req.headers().get(axum::http::header::USER_AGENT) {
        if let Ok(s) = ua.to_str() {
            if s.starts_with("git/") || s.starts_with("Breezy/") {
                return true;
            }
        }
    }
    false
}

/// Render `/git/<codebase>/<sub_path>` via klaus.
pub async fn handle_klaus(
    State(state): State<AppState>,
    axum::extract::Path((codebase, sub_path)): axum::extract::Path<(String, String)>,
    req: Request,
) -> Result<Response> {
    debug!("klaus request: {} {}", codebase, sub_path);

    let repo_path = state.repo_manager.repo_path(&codebase);

    // Mirror py::_git_open_repo: if the local clone is missing, auto-
    // create it when the codebase is registered, else 404.
    if !repo_path.exists() {
        if !state.db_manager.codebase_exists(&codebase).await? {
            return Err(GitStoreError::RepositoryNotFound(codebase.clone()));
        }
        state.repo_manager.open_or_create(&codebase)?;
    }

    let (parts, body) = req.into_parts();
    let body_bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|e| GitStoreError::HttpError(format!("read request body: {}", e)))?;

    let method = parts.method.as_str().to_string();
    let path_info = format!("/{}", sub_path);
    let query_string = parts.uri.query().unwrap_or("").to_string();
    let headers = parts.headers;

    let (status, response_headers, body_out) = tokio::task::spawn_blocking(move || {
        invoke_klaus(
            &codebase,
            &repo_path,
            &method,
            &path_info,
            &query_string,
            &headers,
            &body_bytes,
        )
    })
    .await
    .map_err(|e| GitStoreError::HttpError(format!("klaus task join error: {}", e)))??;

    let mut builder = Response::builder().status(status);
    for (k, v) in &response_headers {
        builder = builder.header(k, v);
    }
    builder
        .body(Body::from(body_out))
        .map_err(|e| GitStoreError::HttpError(format!("Failed to build response: {}", e)))
}

/// (status, response headers, response body) returned from a klaus
/// WSGI invocation.
type KlausResponse = (StatusCode, Vec<(String, String)>, Vec<u8>);

/// Build a WSGI environ dict for `repo_path`, instantiate the klaus
/// Flask app via Python, call it, and return (status, headers, body).
fn invoke_klaus(
    codebase: &str,
    repo_path: &Path,
    method: &str,
    path_info: &str,
    query_string: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<KlausResponse> {
    Python::attach(|py| -> Result<_> {
        // Build the Flask/Klaus app for this repository. We recreate it
        // per request - klaus keeps no meaningful state across requests.
        let klaus_app = build_klaus_app(py, codebase, repo_path)?;

        // Build a WSGI environ dict.
        let io = py
            .import("io")
            .map_err(|e| GitStoreError::HttpError(format!("import io: {}", e)))?;
        let environ = PyDict::new(py);
        environ.set_item("REQUEST_METHOD", method).map_err(py_err)?;
        // SCRIPT_NAME anchors klaus's `url_for(...)` under the
        // per-codebase mount so links resolve back through our
        // dispatch instead of hitting the document root.
        let script_name = format!("/git/{}", codebase);
        environ
            .set_item("SCRIPT_NAME", script_name)
            .map_err(py_err)?;
        environ.set_item("PATH_INFO", path_info).map_err(py_err)?;
        environ
            .set_item("QUERY_STRING", query_string)
            .map_err(py_err)?;
        environ
            .set_item("SERVER_NAME", "localhost")
            .map_err(py_err)?;
        environ.set_item("SERVER_PORT", "80").map_err(py_err)?;
        environ
            .set_item("SERVER_PROTOCOL", "HTTP/1.1")
            .map_err(py_err)?;
        environ
            .set_item("wsgi.url_scheme", "http")
            .map_err(py_err)?;
        environ.set_item("wsgi.multithread", true).map_err(py_err)?;
        environ
            .set_item("wsgi.multiprocess", false)
            .map_err(py_err)?;
        environ.set_item("wsgi.run_once", false).map_err(py_err)?;
        environ
            .set_item("wsgi.version", (1i32, 0i32))
            .map_err(py_err)?;

        let errors_stream = io
            .getattr("StringIO")
            .and_then(|c| c.call0())
            .map_err(py_err)?;
        environ
            .set_item("wsgi.errors", errors_stream)
            .map_err(py_err)?;

        let body_py = PyBytes::new(py, body);
        let input_stream = io
            .getattr("BytesIO")
            .and_then(|c| c.call1((body_py,)))
            .map_err(py_err)?;
        environ
            .set_item("wsgi.input", input_stream)
            .map_err(py_err)?;

        // PEP 3333 wants CONTENT_LENGTH / CONTENT_TYPE as their own
        // keys, not just HTTP_*.
        if let Some(cl) = headers.get(axum::http::header::CONTENT_LENGTH) {
            if let Ok(s) = cl.to_str() {
                environ.set_item("CONTENT_LENGTH", s).map_err(py_err)?;
            }
        } else {
            environ
                .set_item("CONTENT_LENGTH", body.len().to_string())
                .map_err(py_err)?;
        }
        if let Some(ct) = headers.get(axum::http::header::CONTENT_TYPE) {
            if let Ok(s) = ct.to_str() {
                environ.set_item("CONTENT_TYPE", s).map_err(py_err)?;
            }
        }

        for (name, value) in headers.iter() {
            let name_str = name.as_str();
            if name_str.eq_ignore_ascii_case("content-length")
                || name_str.eq_ignore_ascii_case("content-type")
            {
                continue;
            }
            let Ok(value_str) = value.to_str() else {
                continue;
            };
            let wsgi_name = format!("HTTP_{}", name_str.to_ascii_uppercase().replace('-', "_"));
            environ.set_item(wsgi_name, value_str).map_err(py_err)?;
        }

        // Owned Py<PyDict> so the start_response closure is Send-safe.
        let status_holder: Py<PyDict> = PyDict::new(py).unbind();
        status_holder
            .bind(py)
            .set_item("status", py.None())
            .map_err(py_err)?;
        status_holder
            .bind(py)
            .set_item("headers", py.None())
            .map_err(py_err)?;

        let sr_holder = status_holder.clone_ref(py);
        let start_response = pyo3::types::PyCFunction::new_closure(
            py,
            None,
            None,
            move |args: &Bound<'_, pyo3::types::PyTuple>,
                  _kwargs: Option<&Bound<'_, PyDict>>|
                  -> PyResult<Py<PyAny>> {
                let py = args.py();
                let status: String = args.get_item(0)?.extract()?;
                let headers: Py<PyAny> = args.get_item(1)?.unbind();
                let bound = sr_holder.bind(py);
                bound.set_item("status", status)?;
                bound.set_item("headers", headers)?;
                Ok(py.None())
            },
        )
        .map_err(py_err)?;

        let iterable = klaus_app.call1((environ, start_response)).map_err(py_err)?;

        let mut body_out: Vec<u8> = Vec::new();
        for chunk in iterable.try_iter().map_err(py_err)? {
            let chunk = chunk.map_err(py_err)?;
            let bytes: &[u8] = chunk
                .cast::<PyBytes>()
                .map_err(|_| GitStoreError::HttpError("klaus yielded non-bytes chunk".to_string()))?
                .as_bytes();
            body_out.extend_from_slice(bytes);
        }

        let status_holder_bound = status_holder.bind(py);
        let status_item = status_holder_bound
            .get_item("status")
            .map_err(py_err)?
            .ok_or_else(|| {
                GitStoreError::HttpError("klaus did not call start_response".to_string())
            })?;
        let status_str: String = status_item.extract().map_err(|_| {
            GitStoreError::HttpError("klaus returned non-string status".to_string())
        })?;
        let code_str = status_str.split_whitespace().next().unwrap_or("500");
        let status_code: u16 = code_str.parse().unwrap_or(500);
        let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        let mut response_headers: Vec<(String, String)> = Vec::new();
        if let Some(headers_py) = status_holder_bound.get_item("headers").map_err(py_err)? {
            if !headers_py.is_none() {
                for item in headers_py.try_iter().map_err(py_err)? {
                    let item: Bound<'_, PyAny> = item.map_err(py_err)?;
                    let name_item = item.get_item(0).map_err(py_err)?;
                    let value_item = item.get_item(1).map_err(py_err)?;
                    let name: String = name_item.extract().map_err(py_err)?;
                    let value: String = value_item.extract().map_err(py_err)?;
                    if HeaderName::try_from(&name).is_err()
                        || HeaderValue::try_from(&value).is_err()
                    {
                        continue;
                    }
                    response_headers.push((name, value));
                }
            }
        }

        Ok((status, response_headers, body_out))
    })
}

/// Locate klaus's bundled `static/` directory so axum can serve it
/// directly, sharing a single cache key across all codebases.
pub fn klaus_static_dir() -> Result<std::path::PathBuf> {
    Python::attach(|py| {
        let klaus = py
            .import("klaus")
            .map_err(|e| GitStoreError::HttpError(format!("import klaus: {}", e)))?;
        let file: String = klaus
            .getattr("__file__")
            .and_then(|f| f.extract())
            .map_err(|e| GitStoreError::HttpError(format!("klaus.__file__: {}", e)))?;
        let dir = std::path::PathBuf::from(file)
            .parent()
            .ok_or_else(|| GitStoreError::HttpError("klaus.__file__ has no parent".to_string()))?
            .join("static");
        Ok(dir)
    })
}

/// Build a klaus Flask app scoped to one repository.
fn build_klaus_app<'py>(
    py: Python<'py>,
    codebase: &str,
    repo_path: &Path,
) -> Result<Bound<'py, PyAny>> {
    let builtins = py
        .import("builtins")
        .map_err(|e| GitStoreError::HttpError(format!("import builtins: {}", e)))?;

    let src = r#"
def build_klaus_app(codebase, repo_path):
    from flask import Flask
    from klaus import utils, views
    from klaus.repo import FancyRepo

    class Klaus(Flask):
        def __init__(self, codebase, repo_path):
            super().__init__("klaus")
            self.codebase = codebase
            self.valid_repos = {codebase: FancyRepo(repo_path, namespace=None)}

        def should_use_ctags(self, git_repo, git_commit):
            return False

        def url_for(self, endpoint, **values):
            # Rewrite the static endpoint to a shared /git/_static/
            # path so all codebases hit the same cache key.
            if endpoint == "static":
                filename = values.get("filename", "")
                return f"/git/_static/{filename}"
            return super().url_for(endpoint, **values)

        def create_jinja_environment(self):
            env = super().create_jinja_environment()
            for func in [
                "force_unicode",
                "timesince",
                "shorten_sha1",
                "shorten_message",
                "extract_author_name",
                "formattimestamp",
            ]:
                env.filters[func] = getattr(utils, func)
            env.globals["KLAUS_VERSION"] = getattr(__import__("klaus"), "KLAUS_VERSION", "")
            env.globals["USE_SMARTHTTP"] = False
            env.globals["SITE_NAME"] = "Codebase list"
            return env

    app = Klaus(codebase, repo_path)

    for endpoint, rule in [
        ("blob", "/blob/"),
        ("blob", "/blob/<rev>/<path:path>"),
        ("blame", "/blame/"),
        ("blame", "/blame/<rev>/<path:path>"),
        ("raw", "/raw/<path:path>/"),
        ("raw", "/raw/<rev>/<path:path>"),
        ("submodule", "/submodule/<rev>/"),
        ("submodule", "/submodule/<rev>/<path:path>"),
        ("commit", "/commit/<path:rev>/"),
        ("patch", "/commit/<path:rev>.diff"),
        ("patch", "/commit/<path:rev>.patch"),
        ("index", "/"),
        ("index", "/<path:rev>"),
        ("history", "/tree/<rev>/"),
        ("history", "/tree/<rev>/<path:path>"),
        ("download", "/tarball/<path:rev>/"),
        ("repo_list", "/.."),
    ]:
        app.add_url_rule(
            rule, view_func=getattr(views, endpoint), defaults={"repo": codebase}
        )

    return app
"#;
    let globals = PyDict::new(py);
    globals.set_item("__builtins__", builtins).map_err(py_err)?;
    py.run(
        std::ffi::CString::new(src)
            .map_err(|e| GitStoreError::HttpError(e.to_string()))?
            .as_c_str(),
        Some(&globals),
        None,
    )
    .map_err(py_err)?;

    let builder = globals
        .get_item("build_klaus_app")
        .map_err(py_err)?
        .ok_or_else(|| GitStoreError::HttpError("build_klaus_app not defined".to_string()))?;

    let repo_path_str = repo_path.to_string_lossy().into_owned();
    builder.call1((codebase, repo_path_str)).map_err(py_err)
}

fn py_err(e: PyErr) -> GitStoreError {
    warn!("klaus python error: {}", e);
    GitStoreError::HttpError(format!("klaus error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn build_req(uri: &str, user_agent: Option<&str>) -> Request {
        let mut builder = Request::builder().method(Method::GET).uri(uri);
        if let Some(ua) = user_agent {
            builder = builder.header("user-agent", ua);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn test_is_git_client_info_refs() {
        let req = build_req("/git/example/info/refs?service=git-upload-pack", None);
        assert!(is_git_client_request(&req));
    }

    #[test]
    fn test_is_git_client_upload_pack_url() {
        let req = build_req("/git/example/git-upload-pack", None);
        assert!(is_git_client_request(&req));
    }

    #[test]
    fn test_is_git_client_receive_pack_url() {
        let req = build_req("/git/example/git-receive-pack", None);
        assert!(is_git_client_request(&req));
    }

    #[test]
    fn test_is_git_client_service_query() {
        let req = build_req("/git/example/?service=git-upload-pack", None);
        assert!(is_git_client_request(&req));
    }

    #[test]
    fn test_is_git_client_user_agent() {
        let req = build_req("/git/example/tree/main/", Some("git/2.45.2"));
        assert!(is_git_client_request(&req));
    }

    #[test]
    fn test_is_not_git_client_browser() {
        let req = build_req(
            "/git/example/tree/main/",
            Some("Mozilla/5.0 (compatible; Firefox)"),
        );
        assert!(!is_git_client_request(&req));
    }

    #[test]
    fn test_is_not_git_client_no_ua() {
        let req = build_req("/git/example/tree/main/", None);
        assert!(!is_git_client_request(&req));
    }
}
