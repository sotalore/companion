//! The site API, as `sotalore/sotalore`'s `docs/api.md` describes it. We use:
//! - `GET /api/v1/me`: does the token work, and which scopes does it have?
//! - the three export uploads (scope `upload`);
//! - `GET /scenes` (scope `read`) and `POST /scenes/:id/images` (scope `images`) for screenshots;
//! - `POST /scenes/:id/notes` (scope `notes`) for spoken notes' transcripts, which are private.
//!
//! Retry rules (from CLAUDE.md and the contract): the API answers a repeated file with the first
//! import, and a repeated image with the first image (`200`), so retrying a send is safe. We retry
//! network errors and 5xx with backoff, and never retry a 4xx: then the file or the request is the
//! problem, and the user needs to see the message.

use serde::{Deserialize, Deserializer, Serialize};
use std::future::Future;
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

/// A scene on the site (`GET /scenes`). Only what the companion shows; other fields are skipped.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SiteScene {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub game_name: Option<String>,
    #[serde(default, deserialize_with = "lenient_text")]
    pub region: Option<String>,
    #[serde(default, deserialize_with = "lenient_text")]
    pub level: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Reads a field as text whatever JSON type it comes as (a string, a number...), or `None` for null.
/// Only used for fields we just display, so a type we didn't expect can't break the scene list.
fn lenient_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s),
        other => Some(other.to_string()),
    })
}

/// An image added to a scene's gallery (`POST /scenes/:id/images`).
#[derive(Debug, Clone, PartialEq)]
pub struct SceneImage {
    /// `true` for `201 Created`; `false` for `200 OK`, the scene already had these exact bytes.
    pub created: bool,
    /// The full image's link.
    pub url: Option<String>,
    /// The scene's page on the site.
    pub scene_url: Option<String>,
}

/// A spoken note's transcript, as `POST /scenes/:id/notes` takes it. It borrows the note's fields
/// (`&'a str`) rather than copying them, like passing a Ruby hash of the note's own strings.
#[derive(Debug, Serialize)]
pub struct NewNote<'a> {
    pub text: &'a str,
    /// When recording started, ISO 8601 with an offset.
    pub recorded_at: &'a str,
    /// The note's own id, made when it was recorded and the same on every retry.
    pub client_id: &'a str,
    /// Whole seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcriber: Option<&'a str>,
}

/// A note the site has (`POST /scenes/:id/notes`).
#[derive(Debug, Clone, PartialEq)]
pub struct SceneNote {
    /// `true` for `201 Created`; `false` for `200 OK`, the site already had this note, same text.
    pub created: bool,
    pub id: Option<i64>,
    /// The scene's page on the site.
    pub scene_url: Option<String>,
}

/// Runs `attempt` until it works, retrying network errors and 5xx after each of `delays`. Any other
/// error comes back at once. `on_retry` is called before each wait, so the UI can say so.
/// `attempt` is a closure that makes a fresh request each time, like a Ruby block called in a loop.
pub async fn with_retries<T, Fut>(
    delays: &[Duration],
    mut on_retry: impl FnMut(&ApiError, Duration),
    mut attempt: impl FnMut() -> Fut,
) -> Result<T, ApiError>
where
    Fut: Future<Output = Result<T, ApiError>>,
{
    let mut tries = 0;
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(err) if err.is_retryable() && tries < delays.len() => {
                on_retry(&err, delays[tries]);
                tokio::time::sleep(delays[tries]).await;
                tries += 1;
            }
            Err(err) => return Err(err),
        }
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

    /// An upload with retries (see `with_retries`).
    pub async fn upload(
        &self,
        endpoint: &str,
        filename: &str,
        bytes: &[u8],
        delays: &[Duration],
        on_retry: impl FnMut(&ApiError, Duration),
    ) -> Result<Uploaded, ApiError> {
        with_retries(delays, on_retry, || self.upload_once(endpoint, filename, bytes.to_vec())).await
    }

    /// `GET /scenes`, by the game's internal name (`game_name`, exact) or a name search (`q`).
    /// Only the first page (up to 100): one exact match, or enough to pick from. Not retried.
    pub async fn scenes(&self, param: &str, value: &str) -> Result<Vec<SiteScene>, ApiError> {
        #[derive(Deserialize)]
        struct Page {
            scenes: Vec<SiteScene>,
        }
        let response = self
            .http
            .get(format!("{}/scenes", self.api_base))
            .query(&[(param, value), ("per", "100")])
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(network_error)?;
        let body = check_status(response).await?;
        serde_json::from_str::<Page>(&body)
            .map(|page| page.scenes)
            .map_err(|e| ApiError::Status { status: 200, message: format!("unexpected answer from /scenes: {e}") })
    }

    /// `GET /skills`: every skill the site knows, and its game school renames (scope `read`).
    pub async fn skills(&self) -> Result<crate::skills::SiteSkills, ApiError> {
        let response = self
            .http
            .get(format!("{}/skills", self.api_base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(network_error)?;
        let body = check_status(response).await?;
        serde_json::from_str(&body)
            .map_err(|e| ApiError::Status { status: 200, message: format!("unexpected answer from /skills: {e}") })
    }

    /// One try at adding an image to a scene's gallery. `taken_at` is ISO 8601 with an offset.
    pub async fn add_scene_image_once(
        &self,
        scene_id: i64,
        filename: &str,
        bytes: Vec<u8>,
        taken_at: &str,
    ) -> Result<SceneImage, ApiError> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str(image_mime(filename))
            .expect("image MIME types are valid");
        let form = reqwest::multipart::Form::new().part("file", part).text("taken_at", taken_at.to_string());
        let response = self
            .http
            .post(format!("{}/scenes/{scene_id}/images", self.api_base))
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status().as_u16();
        let body = check_status(response).await?;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let text = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).map(String::from);
        Ok(SceneImage {
            created: status == 201,
            url: text(json.get("url")),
            scene_url: text(json.get("scene").and_then(|s| s.get("url"))),
        })
    }

    /// Adds an image with retries (see `with_retries`). A repeat is a `200`, so retrying is safe.
    pub async fn add_scene_image(
        &self,
        scene_id: i64,
        filename: &str,
        bytes: &[u8],
        taken_at: &str,
        delays: &[Duration],
        on_retry: impl FnMut(&ApiError, Duration),
    ) -> Result<SceneImage, ApiError> {
        with_retries(delays, on_retry, || self.add_scene_image_once(scene_id, filename, bytes.to_vec(), taken_at))
            .await
    }

    /// One try at adding a note to a scene. A `409` (the `client_id` was sent before with another
    /// scene or text) comes back as an error like any 4xx: never retried.
    pub async fn add_scene_note_once(&self, scene_id: i64, note: &NewNote<'_>) -> Result<SceneNote, ApiError> {
        let response = self
            .http
            .post(format!("{}/scenes/{scene_id}/notes", self.api_base))
            .bearer_auth(&self.token)
            .json(note)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status().as_u16();
        let body = check_status(response).await?;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        Ok(SceneNote {
            created: status == 201,
            id: json.get("id").and_then(|v| v.as_i64()),
            scene_url: json.get("scene").and_then(|s| s.get("url")).and_then(|v| v.as_str()).map(String::from),
        })
    }

    /// Adds a note with retries (see `with_retries`). The same `client_id` and text again is a `200`
    /// with the first note, so retrying is safe.
    pub async fn add_scene_note(
        &self,
        scene_id: i64,
        note: &NewNote<'_>,
        delays: &[Duration],
        on_retry: impl FnMut(&ApiError, Duration),
    ) -> Result<SceneNote, ApiError> {
        with_retries(delays, on_retry, || self.add_scene_note_once(scene_id, note)).await
    }
}

/// The MIME type for an image file, from its extension. The site works the type out from the bytes
/// anyway; this is only the label on the upload.
fn image_mime(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else {
        "image/png"
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

    fn note() -> NewNote<'static> {
        NewNote {
            text: "The inn sells bread.",
            recorded_at: "2026-11-23T17:00:00-08:00",
            client_id: "abc123",
            duration: Some(6),
            transcriber: Some("whisper-small.en"),
        }
    }

    #[tokio::test]
    async fn sends_a_note_as_json_and_retries_a_503() {
        let mut server = mockito::Server::new_async().await;
        // mockito answers with the oldest matching mock first: one 503, then the note.
        let busy = server.mock("POST", "/api/v1/scenes/41/notes").with_status(503).expect(1).create_async().await;
        let ok = server
            .mock("POST", "/api/v1/scenes/41/notes")
            .match_header("content-type", "application/json")
            .match_body(Matcher::Json(serde_json::json!({
                "text": "The inn sells bread.", "recorded_at": "2026-11-23T17:00:00-08:00",
                "client_id": "abc123", "duration": 6, "transcriber": "whisper-small.en"
            })))
            .with_status(201)
            .with_body(r#"{ "id": 5, "scene": { "id": 41, "name": "Owl's Head", "url": "https://x.test/scenes/41" } }"#)
            .create_async()
            .await;
        let mut retries = 0;
        let made = Client::new(&server.url(), "slt_test")
            .add_scene_note(41, &note(), NO_WAIT, |_, _| retries += 1)
            .await
            .unwrap();
        assert_eq!(made, SceneNote { created: true, id: Some(5), scene_url: Some("https://x.test/scenes/41".into()) });
        assert_eq!(retries, 1);
        busy.assert_async().await;
        ok.assert_async().await;
    }

    #[tokio::test]
    async fn a_conflicting_note_is_not_retried() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/v1/scenes/41/notes")
            .with_status(409)
            .with_body(r#"{ "error": "That client_id was already sent", "note": { "id": 5 } }"#)
            .expect(1)
            .create_async()
            .await;
        let err = Client::new(&server.url(), "slt_test").add_scene_note(41, &note(), NO_WAIT, |_, _| {}).await.unwrap_err();
        assert_eq!(err, ApiError::Status { status: 409, message: "That client_id was already sent".into() });
        mock.assert_async().await;
    }

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
        assert_eq!(me.token.scopes, vec!["upload"]);
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
    async fn finds_scenes_by_game_name() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api/v1/scenes")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("game_name".into(), "OwlsHead_01".into()),
                Matcher::UrlEncoded("per".into(), "100".into()),
            ]))
            .with_body(r#"{ "scenes": [ { "id": 41, "name": "Owl's Head", "game_name": "OwlsHead_01",
                "region": "Novia", "scene_type": "town", "level": 2, "level_plus": false, "parent": null,
                "images": 3, "url": "https://example.test/scenes/41" } ],
                "page": { "number": 1, "per": 100, "total_count": 1, "total_pages": 1 } }"#)
            .create_async()
            .await;
        let scenes = Client::new(&server.url(), "t").scenes("game_name", "OwlsHead_01").await.unwrap();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].id, 41);
        assert_eq!(scenes[0].level.as_deref(), Some("2")); // a number, read as text
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn adds_an_image_with_its_time_and_retries_a_5xx() {
        let mut server = mockito::Server::new_async().await;
        let failing = server.mock("POST", "/api/v1/scenes/41/images").with_status(502).expect(1).create_async().await;
        let ok = server
            .mock("POST", "/api/v1/scenes/41/images")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(r#"name="file"; filename="SotA_11-23-26_12-00_1.png""#.into()),
                Matcher::Regex(r"(?i)content-type: image/png".into()),
                Matcher::Regex(r#"name="taken_at"\r\n\r\n2026-11-23T12:00:05-08:00"#.into()),
            ]))
            .with_status(201)
            .with_body(r#"{ "id": 9, "scene": { "id": 41, "name": "Owl's Head", "url": "https://example.test/scenes/41" },
                "url": "https://example.test/img/9", "preview_url": "https://example.test/img/9/p" }"#)
            .expect(1)
            .create_async()
            .await;
        let image = Client::new(&server.url(), "t")
            .add_scene_image(41, "SotA_11-23-26_12-00_1.png", b"png bytes", "2026-11-23T12:00:05-08:00", NO_WAIT, |_, _| {})
            .await
            .unwrap();
        assert_eq!(
            image,
            SceneImage {
                created: true,
                url: Some("https://example.test/img/9".into()),
                scene_url: Some("https://example.test/scenes/41".into()),
            }
        );
        failing.assert_async().await;
        ok.assert_async().await;
    }

    #[tokio::test]
    async fn reads_the_sites_skills() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/skills")
            .match_header("authorization", "Bearer t")
            .with_body(r#"{ "skills": [ { "id": 1, "key": "combat~blades~thrust", "name": "Thrust", "activity": "adventuring",
                "category": "combat", "school": "blades", "xp_factor": 1.0, "game_id": 10, "game_key": "Thrust" } ],
                "game_schools": { "moon": "lunar" } }"#)
            .create_async()
            .await;
        let site = Client::new(&server.url(), "t").skills().await.unwrap();
        assert_eq!(site.skills[0].game_id, Some(10));
        assert_eq!(site.game_schools["moon"], "lunar");
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
