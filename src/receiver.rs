//! Local HTTP receiver for Graph change notifications.
//!
//! Binds 127.0.0.1 only; the tunnel is what makes it reachable. A request is
//! accepted only for a channel Chaat announced, so the channel acts as the
//! shared secret. Bodies are never logged.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Largest request body the receiver reads; more is answered with 413.
pub const MAX_BODY: usize = 512 * 1024;
/// Largest body sent to Chaat in one protocol line.
pub const MAX_LINE_BODY: usize = 48 * 1024;

/// Capacity of the queue to the serve loop; a full queue drops notifications.
const QUEUE: usize = 256;

/// One accepted notification body and the channel it arrived on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub channel: String,
    pub body: String,
}

struct Shared {
    channels: Mutex<HashSet<String>>,
    check: Mutex<Option<String>>,
    tx: mpsc::Sender<Forward>,
}

/// The running receiver. Dropping it stops the server.
pub struct Receiver {
    port: u16,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl Receiver {
    /// Binds 127.0.0.1 on a free port and starts serving.
    pub async fn start() -> std::io::Result<(Receiver, mpsc::Receiver<Forward>)> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let (tx, rx) = mpsc::channel(QUEUE);
        let shared = Arc::new(Shared {
            channels: Mutex::new(HashSet::new()),
            check: Mutex::new(None),
            tx,
        });
        let app = Router::new()
            .route("/graph/{channel}", post(notify).fallback(not_found))
            .route("/lifecycle/{channel}", post(notify).fallback(not_found))
            .fallback(not_found)
            .with_state(shared.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((Receiver { port, shared, task }, rx))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Replaces the announced channels. Malformed channels are dropped.
    pub fn set_channels(&self, channels: Vec<String>) {
        let set = channels.into_iter().filter(|c| is_channel(c)).collect();
        *self.shared.channels.lock().unwrap() = set;
    }

    /// Sets (or clears) the token that validation requests may use as a channel.
    pub fn allow_check(&self, token: Option<String>) {
        *self.shared.check.lock().unwrap() = token;
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A channel is 16 to 64 characters of `A-Za-z0-9-_`.
fn is_channel(text: &str) -> bool {
    (16..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn notify(
    State(shared): State<Arc<Shared>>,
    Path(channel): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    request: Request<Body>,
) -> Response {
    let announced = shared.channels.lock().unwrap().contains(&channel);
    if let Some(token) = query.get("validationToken") {
        let checking = shared.check.lock().unwrap().as_deref() == Some(channel.as_str());
        if !(announced || checking) {
            return StatusCode::NOT_FOUND.into_response();
        }
        return ([(header::CONTENT_TYPE, "text/plain")], token.clone()).into_response();
    }
    if !announced {
        return StatusCode::NOT_FOUND.into_response();
    }
    let bytes = match axum::body::to_bytes(request.into_body(), MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let body = String::from_utf8_lossy(&bytes).into_owned();
    // A full queue drops the notification; Chaat fetches when the person looks.
    let _ = shared.tx.try_send(Forward { channel, body });
    StatusCode::ACCEPTED.into_response()
}

/// Splits a notification body so each piece fits `max` bytes. A body within
/// `max` is returned as is. A larger `{"value":[..]}` body is repacked into
/// several bodies of the same shape; an entry that alone exceeds `max` is
/// dropped, and anything that is not such JSON yields nothing.
pub fn split_body(body: &str, max: usize) -> Vec<String> {
    if body.len() <= max {
        return vec![body.to_string()];
    }
    let Ok(serde_json::Value::Object(mut object)) = serde_json::from_str(body) else {
        return Vec::new();
    };
    let Some(serde_json::Value::Array(entries)) = object.remove("value") else {
        return Vec::new();
    };
    const OPEN: &str = "{\"value\":[";
    const CLOSE: &str = "]}";
    let fits = |current: &str, text: &str| {
        let comma = usize::from(!current.is_empty());
        OPEN.len() + current.len() + comma + text.len() + CLOSE.len() <= max
    };
    let mut out = Vec::new();
    let mut current = String::new();
    for entry in entries {
        let text = entry.to_string();
        if !fits("", &text) {
            continue;
        }
        if !fits(&current, &text) {
            out.push(format!("{OPEN}{current}{CLOSE}"));
            current.clear();
        }
        if !current.is_empty() {
            current.push(',');
        }
        current.push_str(&text);
    }
    if !current.is_empty() {
        out.push(format!("{OPEN}{current}{CLOSE}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CH: &str = "abcdefghijklmnop_-0123456789ABCD";

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    async fn up() -> (Receiver, mpsc::Receiver<Forward>, String) {
        let (receiver, rx) = Receiver::start().await.unwrap();
        receiver.set_channels(vec![CH.to_string()]);
        let base = format!("http://127.0.0.1:{}", receiver.port());
        (receiver, rx, base)
    }

    #[tokio::test]
    async fn validation_echoes_the_token_as_text() {
        let (_r, _rx, base) = up().await;
        let response = client()
            .post(format!("{base}/graph/{CH}?validationToken=abc%20def"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let kind = response.headers().get("content-type").unwrap();
        assert!(kind.to_str().unwrap().starts_with("text/plain"));
        assert_eq!(response.text().await.unwrap(), "abc def");
    }

    #[tokio::test]
    async fn validation_for_unknown_channel_is_404() {
        let (_r, _rx, base) = up().await;
        let response = client()
            .post(format!(
                "{base}/lifecycle/zzzzzzzzzzzzzzzzzzzz?validationToken=x"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(response.text().await.unwrap(), "");
    }

    #[tokio::test]
    async fn validation_accepts_the_check_token() {
        let (r, _rx, base) = up().await;
        let token = "checktokencheck0123456789";
        let url = format!("{base}/graph/{token}?validationToken=t2");
        assert_eq!(client().post(&url).send().await.unwrap().status(), 404);
        r.allow_check(Some(token.to_string()));
        let response = client().post(&url).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "t2");
        r.allow_check(None);
        assert_eq!(client().post(&url).send().await.unwrap().status(), 404);
    }

    #[tokio::test]
    async fn notification_is_accepted_and_forwarded() {
        let (_r, mut rx, base) = up().await;
        let response = client()
            .post(format!("{base}/graph/{CH}"))
            .body("{\"value\":[]}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 202);
        let forward = rx.recv().await.unwrap();
        assert_eq!(forward.channel, CH);
        assert_eq!(forward.body, "{\"value\":[]}");
    }

    #[tokio::test]
    async fn lifecycle_route_forwards_too() {
        let (_r, mut rx, base) = up().await;
        let response = client()
            .post(format!("{base}/lifecycle/{CH}"))
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 202);
        assert_eq!(rx.recv().await.unwrap().body, "x");
    }

    #[tokio::test]
    async fn oversize_body_is_413() {
        let (_r, mut rx, base) = up().await;
        let response = client()
            .post(format!("{base}/graph/{CH}"))
            .body(vec![b'a'; MAX_BODY + 1])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unknown_channel_and_other_routes_are_404() {
        let (_r, mut rx, base) = up().await;
        let c = client();
        let unknown = c
            .post(format!("{base}/graph/zzzzzzzzzzzzzzzzzzzz"))
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status(), 404);
        assert_eq!(unknown.text().await.unwrap(), "");
        let get = c.get(format!("{base}/graph/{CH}")).send().await.unwrap();
        assert_eq!(get.status(), 404);
        let other = c.post(format!("{base}/other/{CH}")).send().await.unwrap();
        assert_eq!(other.status(), 404);
        let root = c.post(format!("{base}/")).send().await.unwrap();
        assert_eq!(root.status(), 404);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn set_channels_replaces_and_filters() {
        let (r, _rx, base) = up().await;
        r.set_channels(vec!["short".into(), "has space in it 0123456789".into()]);
        let c = client();
        let short = c.post(format!("{base}/graph/short")).body("x").send().await;
        assert_eq!(short.unwrap().status(), 404);
        let gone = c.post(format!("{base}/graph/{CH}")).body("x").send().await;
        assert_eq!(gone.unwrap().status(), 404);
    }

    #[test]
    fn split_keeps_a_small_body() {
        assert_eq!(
            split_body("{\"value\":[1]}", 100),
            vec!["{\"value\":[1]}".to_string()]
        );
    }

    #[test]
    fn split_packs_entries_within_the_limit() {
        let entry = format!("\"{}\"", "a".repeat(38)); // 40 bytes
        let body = format!("{{\"value\":[{entry},{entry},{entry}]}}");
        let parts = split_body(&body, 100);
        assert_eq!(parts.len(), 2);
        let mut total = 0;
        for part in &parts {
            assert!(part.len() <= 100);
            let v: serde_json::Value = serde_json::from_str(part).unwrap();
            total += v["value"].as_array().unwrap().len();
        }
        assert_eq!(total, 3);
    }

    #[test]
    fn split_packs_at_the_exact_boundary() {
        let entry = format!("\"{}\"", "a".repeat(38)); // 40 bytes
        let body = format!("{{\"value\":[{entry},{entry},{entry}]}}");
        // Two entries make exactly 93 bytes.
        let parts = split_body(&body, 93);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 93);
        let parts = split_body(&body, 92);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() == 52));
        // One byte short of a single entry drops everything.
        assert!(split_body(&body, 51).is_empty());
        assert_eq!(split_body(&body, 52).len(), 3);
    }

    #[tokio::test]
    async fn body_of_exactly_max_is_accepted() {
        let (_r, mut rx, base) = up().await;
        let response = client()
            .post(format!("{base}/graph/{CH}"))
            .body(vec![b'a'; MAX_BODY])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 202);
        assert_eq!(rx.recv().await.unwrap().body.len(), MAX_BODY);
    }

    #[tokio::test]
    async fn validation_decodes_the_url_encoded_token() {
        let (_r, _rx, base) = up().await;
        let response = client()
            .post(format!(
                "{base}/graph/{CH}?validationToken=Validation%3A%20Testing%20client%20application"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.text().await.unwrap(),
            "Validation: Testing client application"
        );
    }

    #[tokio::test]
    async fn encoded_traversal_channels_are_404() {
        let (_r, mut rx, base) = up().await;
        let c = client();
        for path in ["graph/..%2f..%2fx", "graph/%2e%2e", "graph/%00"] {
            let r = c.post(format!("{base}/{path}")).body("x").send().await;
            assert_eq!(r.unwrap().status(), 404, "{path}");
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn split_drops_an_entry_that_alone_is_too_big() {
        let big = format!("\"{}\"", "b".repeat(200));
        let body = format!("{{\"value\":[1,{big},2]}}");
        assert_eq!(split_body(&body, 40), vec!["{\"value\":[1,2]}".to_string()]);
    }

    #[test]
    fn split_gives_nothing_for_oversize_non_json() {
        assert!(split_body("not json", 3).is_empty());
        assert!(split_body("{\"other\":[1,2,3,4,5,6]}", 5).is_empty());
    }
}
