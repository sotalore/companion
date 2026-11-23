//! The site API, as `sotalore/sotalore`'s `docs/api.md` describes it. We use two endpoints:
//! `GET /api/v1/me` (does the token work, and does it have `upload`?) and the three upload endpoints.
//!
//! Retry rules (from CLAUDE.md and the contract): the API answers a repeated file with the first
//! import (`200`), so retrying an upload is safe. We retry network errors and 5xx with backoff, and never
//! retry a 4xx: then the file or the request is the problem, and the user needs to see the message.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long to wait before each retry. Five retries after the first try, about a minute in all.
pub const RETRY_DELAYS: &[Duration] = &[
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

/// What can go wrong talking to the site.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// The site answered with an error status. `message` is its `error` (or `errors`) text.
    Status { status: u16, message: String },
    /// We couldn't reach the site, or it timed out.
    Network(String),
}

impl ApiError {
    /// Only network errors and 5xx are worth another try.
    pub fn is_retryable(&self) -> bool {
        match self {
            ApiError::Network(_) => true,
            ApiError::Status { status, .. } => *status >= 500,
        }
    }
}

// `Display` is Rust's `to_s`: how the error reads in the UI.
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Status { status, message } => write!(f, "{status}: {message}"),
            ApiError::Network(msg) => write!(f, "couldn't reach the site: {msg}"),
        }
    }
}

/// `GET /me`'s answer. Fields we don't need are skipped (the contract says to ignore unknown fields).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Me {
    pub account: Account,
    pub token: TokenInfo,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Account {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenInfo {
    pub name: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<String>,
}

impl Me {
    pub fn can_upload(&self) -> bool {
        self.token.scopes.iter().any(|s| s == "upload")
    }
}

/// A successful upload.
#[derive(Debug, Clone, PartialEq)]
pub struct Uploaded {
    /// `true` for `201 Created`; `false` for `200 OK`, the site already had this exact file.
    pub created: bool,
    pub import_id: Option<i64>,
    /// Where an admin reviews the import on the site.
    pub review_url: Option<String>,
    /// A few words on what was in it, e.g. "55 recipes".
    pub summary: String,
}

pub struct Client {
    http: reqwest::Client,
    /// e.g. `https://www.sotalore.com/api/v1`, with no trailing slash.
    api_base: String,
    token: String,
}

impl Client {
    pub fn new(site_url: &str, token: &str) -> Client {
        let http = reqwest::Client::builder()
            .user_agent(concat!("sotalore-companion/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("the HTTP client's settings are valid");
        Client {
            http,
            api_base: format!("{}/api/v1", site_url.trim_end_matches('/')),
            token: token.to_string(),
        }
    }

    /// `GET /me`: who the token acts as and what scopes it has. Not retried; the UI shows the result.
    pub async fn me(&self) -> Result<Me, ApiError> {
        let response = self
            .http
            .get(format!("{}/me", self.api_base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(network_error)?;
        let body = check_status(response).await?;
        serde_json::from_str(&body)
            .map_err(|e| ApiError::Status { status: 200, message: format!("unexpected answer from /me: {e}") })
    }

    /// One try at an upload: posts the file's exact bytes as `multipart/form-data` in a `file` field.
    pub async fn upload_once(&self, endpoint: &str, filename: &str, bytes: Vec<u8>) -> Result<Uploaded, ApiError> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str("application/json")
            .expect("application/json is a valid MIME type");
        let form = reqwest::multipart::Form::new().part("file", part);
        let response = self
            .http
            .post(format!("{}{}", self.api_base, endpoint))
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status().as_u16();
        let body = check_status(response).await?;
        Ok(parse_import(status, &body))
    }

    /// An upload with retries: network errors and 5xx are tried again after each of `delays`; any
    /// other error comes back at once. `on_retry` is called before each wait, so the UI can say so.
    pub async fn upload(
        &self,
        endpoint: &str,
        filename: &str,
        bytes: &[u8],
        delays: &[Duration],
        mut on_retry: impl FnMut(&ApiError, Duration),
    ) -> Result<Uploaded, ApiError> {
        let mut attempt = 0;
        loop {
            match self.upload_once(endpoint, filename, bytes.to_vec()).await {
                Ok(uploaded) => return Ok(uploaded),
                Err(err) if err.is_retryable() && attempt < delays.len() => {
                    on_retry(&err, delays[attempt]);
                    tokio::time::sleep(delays[attempt]).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

fn network_error(err: reqwest::Error) -> ApiError {
    // `without_url` keeps the URL out of the message; it's long and the user knows it.
    ApiError::Network(err.without_url().to_string())
}

/// Returns the body of a 2xx response, or the site's error message for anything else.
async fn check_status(response: reqwest::Response) -> Result<String, ApiError> {
    let status = response.status();
    let body = response.text().await.map_err(network_error)?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(ApiError::Status { status: status.as_u16(), message: error_message(&body, status) })
    }
}

/// The API's errors are `{ "error": "..." }` or `{ "errors": [...] }`. A path outside the API gets
/// the site's ordinary (non-JSON) 404 page, so fall back to the status's name.
fn error_message(body: &str, status: reqwest::StatusCode) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: Option<String>,
        errors: Option<Vec<String>>,
    }
    match serde_json::from_str::<ErrorBody>(body) {
        Ok(ErrorBody { error: Some(e), .. }) => e,
        Ok(ErrorBody { errors: Some(list), .. }) if !list.is_empty() => list.join("; "),
        _ => status.canonical_reason().unwrap_or("error").to_string(),
    }
}

/// Picks out what the UI shows from an import. Recipe imports count `entries`, item catalogs `items`,
/// finds `scenes`. The body is kept loose (a `Value`) so new fields never break us.
fn parse_import(status: u16, body: &str) -> Uploaded {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let count = |key: &str| json.get(key).and_then(|v| v.as_i64());
    let summary = if let Some(n) = count("entries") {
        format!("{n} recipes")
    } else if let Some(n) = count("items") {
        format!("{n} items")
    } else if let Some(n) = count("scenes") {
        format!("{n} scenes")
    } else {
        String::new()
    };
    Uploaded {
        created: status == 201,
        import_id: json.get("id").and_then(|v| v.as_i64()),
        review_url: json.get("review_url").and_then(|v| v.as_str()).map(String::from),
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Matcher;

    const NO_WAIT: &[Duration] = &[Duration::from_millis(1), Duration::from_millis(1)];

    #[tokio::test]
    async fn me_reads_the_account_and_scopes() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api/v1/me")
            .match_header("authorization", "Bearer slt_test")
            .with_body(r#"{ "account": { "id": 7, "name": "Companion" },
                "token": { "name": "companion", "scopes": [ "upload" ], "expires_at": null } }"#)
            .create_async()
            .await;
        let me = Client::new(&server.url(), "slt_test").me().await.unwrap();
        assert_eq!(me.account.name, "Companion");
        assert!(me.can_upload());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn me_reports_a_bad_token() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/me")
            .with_status(401)
            .with_body(r#"{ "error": "A valid, unexpired token is required" }"#)
            .create_async()
            .await;
        let err = Client::new(&server.url(), "slt_bad").me().await.unwrap_err();
        assert_eq!(err, ApiError::Status { status: 401, message: "A valid, unexpired token is required".into() });
    }

    #[tokio::test]
    async fn uploads_the_file_as_multipart_and_reads_the_import() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/v1/recipe_imports")
            .match_header("authorization", "Bearer slt_test")
            .match_header("content-type", Matcher::Regex("^multipart/form-data".into()))
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(r#"name="file"; filename="sotalore-recipes-new.json""#.into()),
                Matcher::Regex(r#"\{"type":"recipes"\}"#.into()),
            ]))
            .with_status(201)
            .with_body(r#"{ "id": 12, "entries": 55, "review_url": "https://example.test/adm/recipe_imports/12" }"#)
            .create_async()
            .await;
        let up = Client::new(&server.url(), "slt_test")
            .upload("/recipe_imports", "sotalore-recipes-new.json", br#"{"type":"recipes"}"#, NO_WAIT, |_, _| {})
            .await
            .unwrap();
        assert_eq!(
            up,
            Uploaded {
                created: true,
                import_id: Some(12),
                review_url: Some("https://example.test/adm/recipe_imports/12".into()),
                summary: "55 recipes".into(),
            }
        );
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn a_repeated_file_is_a_200_not_an_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/v1/finds_imports")
            .with_status(200)
            .with_body(r#"{ "id": 3, "scenes": 4, "review_url": "https://example.test/adm/finds/3" }"#)
            .create_async()
            .await;
        let up = Client::new(&server.url(), "t")
            .upload("/finds_imports", "f.json", b"{}", NO_WAIT, |_, _| {})
            .await
            .unwrap();
        assert!(!up.created);
        assert_eq!(up.summary, "4 scenes");
    }

    #[tokio::test]
    async fn never_retries_a_4xx() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/v1/item_catalog_imports")
            .with_status(422)
            .with_body(r#"{ "error": "Couldn't import that: wrong type" }"#)
            .expect(1)
            .create_async()
            .await;
        let mut retries = 0;
        let err = Client::new(&server.url(), "t")
            .upload("/item_catalog_imports", "f.json", b"{}", NO_WAIT, |_, _| retries += 1)
            .await
            .unwrap_err();
        assert_eq!(err, ApiError::Status { status: 422, message: "Couldn't import that: wrong type".into() });
        assert_eq!(retries, 0);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn retries_a_5xx_until_it_works() {
        let mut server = mockito::Server::new_async().await;
        // mockito answers with the first mock made until its `expect` count is used up, then the next.
        let failing = server
            .mock("POST", "/api/v1/recipe_imports")
            .with_status(503)
            .expect(2)
            .create_async()
            .await;
        let ok = server
            .mock("POST", "/api/v1/recipe_imports")
            .with_status(201)
            .with_body(r#"{ "id": 1, "entries": 2 }"#)
            .expect(1)
            .create_async()
            .await;
        let mut retries = 0;
        let up = Client::new(&server.url(), "t")
            .upload("/recipe_imports", "f.json", b"{}", NO_WAIT, |_, _| retries += 1)
            .await
            .unwrap();
        assert!(up.created);
        assert_eq!(retries, 2);
        failing.assert_async().await;
        ok.assert_async().await;
    }

    #[tokio::test]
    async fn gives_up_after_the_last_retry() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/v1/recipe_imports")
            .with_status(500)
            .expect(3) // the first try and two retries
            .create_async()
            .await;
        let err = Client::new(&server.url(), "t")
            .upload("/recipe_imports", "f.json", b"{}", NO_WAIT, |_, _| {})
            .await
            .unwrap_err();
        assert_eq!(err, ApiError::Status { status: 500, message: "Internal Server Error".into() });
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn retries_network_errors() {
        // Nothing listens on this port, so every try is a network error.
        let mut retries = 0;
        let err = Client::new("http://127.0.0.1:9", "t")
            .upload("/recipe_imports", "f.json", b"{}", NO_WAIT, |_, _| retries += 1)
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Network(_)));
        assert_eq!(retries, 2);
    }
}
