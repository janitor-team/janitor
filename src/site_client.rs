//! Shared HTTP client for talking to a janitor site instance's public API.
//!
//! Used by CLI tools such as `janitor-admin` and `janitor-package` that drive
//! the site over HTTP rather than talking to the database directly.

use reqwest::{Client, RequestBuilder, Url};

/// A thin wrapper around [`reqwest::Client`] that knows the base URL of a
/// janitor site instance and (optionally) HTTP basic auth credentials.
pub struct ApiClient {
    http: Client,
    base: Url,
    user: Option<String>,
    password: Option<String>,
}

impl ApiClient {
    pub fn new(base: Url, user: Option<String>, password: Option<String>) -> reqwest::Result<Self> {
        Ok(Self {
            http: Client::builder().build()?,
            base,
            user,
            password,
        })
    }

    pub fn request(&self, method: reqwest::Method, path: &str) -> Result<RequestBuilder, String> {
        let url = api_url(&self.base, path).map_err(|e| e.to_string())?;
        let mut req = self.http.request(method, url);
        if let Some(user) = &self.user {
            req = req.basic_auth(user, self.password.as_deref());
        }
        Ok(req)
    }
}

/// Join `path` onto `base`, treating `base` as a directory even if it has no
/// trailing slash.
pub fn api_url(base: &Url, path: &str) -> Result<Url, url::ParseError> {
    let base = if base.path().ends_with('/') {
        base.clone()
    } else {
        let mut b = base.clone();
        b.set_path(&format!("{}/", base.path()));
        b
    };
    base.join(path.trim_start_matches('/'))
}

/// Turn a non-2xx response into an `Err` carrying the status and body text.
pub async fn expect_success(resp: reqwest::Response) -> Result<reqwest::Response, String> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(format!("HTTP {}: {}", status, body.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_url_appends_when_base_has_no_trailing_slash() {
        let base = Url::parse("https://janitor.example/").unwrap();
        assert_eq!(
            api_url(&base, "cupboard/api/workers").unwrap().as_str(),
            "https://janitor.example/cupboard/api/workers"
        );
        let base = Url::parse("https://janitor.example").unwrap();
        assert_eq!(
            api_url(&base, "cupboard/api/workers").unwrap().as_str(),
            "https://janitor.example/cupboard/api/workers"
        );
    }

    #[test]
    fn api_url_preserves_subpath() {
        let base = Url::parse("https://example.com/janitor").unwrap();
        assert_eq!(
            api_url(&base, "api/queue").unwrap().as_str(),
            "https://example.com/janitor/api/queue"
        );
    }

    #[test]
    fn request_joins_path_and_sets_basic_auth() {
        let client = ApiClient::new(
            Url::parse("https://example.com/janitor").unwrap(),
            Some("zainab".to_string()),
            Some("hunter2".to_string()),
        )
        .unwrap();
        let req = client
            .request(reqwest::Method::POST, "/api/queue")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(req.method(), reqwest::Method::POST);
        assert_eq!(req.url().as_str(), "https://example.com/janitor/api/queue");
        assert_eq!(
            req.headers()
                .get(reqwest::header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic emFpbmFiOmh1bnRlcjI="
        );
    }

    #[test]
    fn request_omits_basic_auth_without_user() {
        let client =
            ApiClient::new(Url::parse("https://example.com/").unwrap(), None, None).unwrap();
        let req = client
            .request(reqwest::Method::GET, "api/queue")
            .unwrap()
            .build()
            .unwrap();
        assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[tokio::test]
    async fn expect_success_rejects_non_2xx_and_passes_2xx() {
        let ok = reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body("fine")
                .unwrap(),
        );
        assert!(expect_success(ok).await.is_ok());
        let bad = reqwest::Response::from(
            axum::http::Response::builder()
                .status(503)
                .body("  boom  ")
                .unwrap(),
        );
        assert_eq!(
            expect_success(bad).await.unwrap_err(),
            "HTTP 503 Service Unavailable: boom"
        );
    }
}
