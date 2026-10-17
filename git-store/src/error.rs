//! Error type and axum response mapping.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitStoreError {
    #[error("Git operation failed: {0}")]
    GitError(#[from] git2::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),

    #[error("HTTP error: {0}")]
    HttpError(String),

    /// Malformed request from the client (missing query params, bad
    /// form body, etc). Maps to 400.
    #[error("{0}")]
    BadRequest(String),

    /// The codebase isn't registered. Response body matches Python's
    /// `_git_open_repo` exactly: `no such codebase: {name}`.
    #[error("no such codebase: {0}")]
    RepositoryNotFound(String),

    /// The URL subpath isn't one of the git-http verbs we accept
    /// (dumb-HTTP metadata, info/refs, git-upload-pack,
    /// git-receive-pack). Rust-only guard; Python doesn't register
    /// unmatched paths at all so there's no body to mirror.
    #[error("no such git path: /{0}")]
    GitPathNotFound(String),

    /// Repo exists but the requested revision object is missing.
    /// Distinct from [`Self::RepositoryNotFound`] so callers can
    /// render "diff unavailable" instead of a service outage.
    #[error("Revision not found in {codebase}: {revision}")]
    RevisionNotFound { codebase: String, revision: String },

    /// Codebase is known but the on-disk clone is missing.
    #[error("Local VCS repository for {0} temporarily inaccessible")]
    LocalRepositoryUnavailable(String),

    #[error("Invalid SHA: {0}")]
    InvalidSha(String),

    #[error("Operation timed out")]
    Timeout,

    /// Diff subprocess exceeded the configured timeout. Distinct
    /// message so callers see the same text Python emits.
    #[error("diff generation timed out")]
    DiffTimeout,

    /// The `git diff` subprocess exited non-zero; stderr is included
    /// so the failure message matches Python's exact string.
    #[error("git diff failed: {0}")]
    GitDiffFailed(String),

    /// Set when the read of the request body stream fails.
    #[error("request body did not arrive in full")]
    IncompleteRequestBody,

    #[error("Authentication failed")]
    AuthenticationFailed,

    #[error("Permission denied")]
    PermissionDenied,

    #[error("Invalid configuration: {0}")]
    ConfigError(String),

    #[error("Template error: {0}")]
    TemplateError(#[from] tera::Error),

    #[error("HTTP error: {0}")]
    HttpLibError(#[from] http::Error),

    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, GitStoreError>;

/// Header carrying the machine-readable error class. Callers branch
/// on this instead of parsing the response body.
pub const ERROR_CLASS_HEADER: &str = "X-Janitor-Error";

fn error_class(err: &GitStoreError) -> &'static str {
    match err {
        GitStoreError::GitError(_) => "git-error",
        GitStoreError::IoError(_) => "io-error",
        GitStoreError::DatabaseError(_) => "database-error",
        GitStoreError::HttpError(_) => "http-error",
        GitStoreError::BadRequest(_) => "bad-request",
        GitStoreError::RepositoryNotFound(_) => "repository-not-found",
        GitStoreError::GitPathNotFound(_) => "git-path-not-found",
        GitStoreError::RevisionNotFound { .. } => "revision-not-found",
        GitStoreError::LocalRepositoryUnavailable(_) => "local-repository-unavailable",
        GitStoreError::InvalidSha(_) => "invalid-sha",
        GitStoreError::Timeout => "timeout",
        GitStoreError::DiffTimeout => "diff-timeout",
        GitStoreError::GitDiffFailed(_) => "git-diff-failed",
        GitStoreError::IncompleteRequestBody => "incomplete-request-body",
        GitStoreError::AuthenticationFailed => "authentication-failed",
        GitStoreError::PermissionDenied => "permission-denied",
        GitStoreError::ConfigError(_) => "config-error",
        GitStoreError::TemplateError(_) => "template-error",
        GitStoreError::HttpLibError(_) => "http-lib-error",
        GitStoreError::Other(_) => "other",
    }
}

impl axum::response::IntoResponse for GitStoreError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        use axum::response::Response;

        let (status, message) = match &self {
            GitStoreError::GitError(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            GitStoreError::IoError(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            GitStoreError::DatabaseError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            ),
            GitStoreError::HttpError(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
            GitStoreError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            GitStoreError::RepositoryNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            GitStoreError::GitPathNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            GitStoreError::RevisionNotFound { .. } => (StatusCode::NOT_FOUND, self.to_string()),
            // Matches Python `git_diff_request` / `git_revision_info_request`:
            // raises HTTPServiceUnavailable when the on-disk clone is
            // missing so callers with retry-on-503 keep polling.
            GitStoreError::LocalRepositoryUnavailable(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
            }
            GitStoreError::InvalidSha(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            GitStoreError::Timeout => (StatusCode::REQUEST_TIMEOUT, self.to_string()),
            GitStoreError::DiffTimeout => (StatusCode::REQUEST_TIMEOUT, self.to_string()),
            GitStoreError::GitDiffFailed(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
            GitStoreError::IncompleteRequestBody => (StatusCode::BAD_REQUEST, self.to_string()),
            GitStoreError::AuthenticationFailed => (StatusCode::UNAUTHORIZED, self.to_string()),
            GitStoreError::PermissionDenied => (StatusCode::FORBIDDEN, self.to_string()),
            GitStoreError::ConfigError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Configuration error".to_string(),
            ),
            GitStoreError::TemplateError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Template error".to_string(),
            ),
            GitStoreError::HttpLibError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "HTTP library error".to_string(),
            ),
            GitStoreError::Other(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            ),
        };

        let class = error_class(&self);
        let mut builder = Response::builder()
            .status(status)
            .header(ERROR_CLASS_HEADER, class);
        // RFC 7235: a 401 must include WWW-Authenticate or clients
        // treat it as terminal and won't retry with URL userinfo.
        if status == StatusCode::UNAUTHORIZED {
            builder = builder.header("WWW-Authenticate", "Basic realm=\"Janitor\"");
        }
        builder.body(message.into()).unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body("Failed to construct error response".into())
                .expect("fallback response cannot fail")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[test]
    fn revision_not_found_is_404_with_label() {
        let resp = GitStoreError::RevisionNotFound {
            codebase: "foo".to_string(),
            revision: "abc".to_string(),
        }
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers().get(ERROR_CLASS_HEADER).unwrap(),
            "revision-not-found"
        );
    }

    #[test]
    fn local_repository_unavailable_is_503_with_distinct_label() {
        let resp = GitStoreError::LocalRepositoryUnavailable("foo".to_string()).into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers().get(ERROR_CLASS_HEADER).unwrap(),
            "local-repository-unavailable"
        );
    }

    #[test]
    fn diff_timeout_matches_python_text() {
        let resp = GitStoreError::DiffTimeout.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::REQUEST_TIMEOUT);
        // Body text must match Python exactly for callers that key on it.
        let body = tokio_test::block_on(async {
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap()
        });
        assert_eq!(&body[..], b"diff generation timed out");
    }

    #[test]
    fn git_diff_failed_matches_python_text() {
        let resp = GitStoreError::GitDiffFailed("fatal: no such ref".into()).into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let body = tokio_test::block_on(async {
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap()
        });
        assert_eq!(&body[..], b"git diff failed: fatal: no such ref");
    }

    #[test]
    fn incomplete_request_body_is_400_with_its_own_label() {
        let resp = GitStoreError::IncompleteRequestBody.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get(ERROR_CLASS_HEADER).unwrap(),
            "incomplete-request-body"
        );
        let body = tokio_test::block_on(async {
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap()
        });
        assert_eq!(&body[..], b"request body did not arrive in full");
    }

    #[test]
    fn repository_not_found_distinct_from_local_unavailable() {
        let resp = GitStoreError::RepositoryNotFound("foo".to_string()).into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers().get(ERROR_CLASS_HEADER).unwrap(),
            "repository-not-found"
        );
    }
}
