//! The plugin's main loop: read Chaat's messages, keep the tunnel running,
//! forward notification bodies to Chaat as `push` lines, and run the "Check
//! the tunnel" button.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::select;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};

use crate::protocol::{self, Incoming};
use crate::receiver::{Forward, MAX_LINE_BODY, Receiver, split_body};
use crate::tunnel::{self, TunnelError, TunnelEvent, TunnelKind};

/// Lines longer than this are dropped by Chaat (its own limit for lines it
/// reads), so we never send one.
pub const MAX_LINE: usize = 64 * 1024;

/// Shown when the person has not entered a tunnel name.
pub const NO_TUNNEL_NAME_MESSAGE: &str = "Set the tunnel's name";
pub const NOT_UP_MESSAGE: &str = "The tunnel is not up yet";
pub const CHECK_FAILED_MESSAGE: &str = "The tunnel did not answer";
const CHECK_OK_MESSAGE: &str = "The tunnel answered";

/// First wait before restarting the tunnel; it doubles up to the maximum.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A tunnel that ran this long counts as healthy: the next wait starts over.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

pub type TunnelFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What the person configured for the tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSetup {
    pub kind: TunnelKind,
    /// The `cloudflared` option, when set.
    pub cloudflared: Option<String>,
}

/// Starts a tunnel to a local port. Tests replace it so they need no
/// cloudflared.
pub trait TunnelLauncher: Send + Sync + 'static {
    /// Returns the future that runs the tunnel and reports on `events`.
    /// Dropping the future stops the tunnel.
    fn launch(
        &self,
        setup: &TunnelSetup,
        port: u16,
        events: mpsc::Sender<TunnelEvent>,
    ) -> Result<TunnelFuture, TunnelError>;
}

/// The real thing: cloudflared.
pub struct Cloudflared;

impl TunnelLauncher for Cloudflared {
    fn launch(
        &self,
        setup: &TunnelSetup,
        port: u16,
        events: mpsc::Sender<TunnelEvent>,
    ) -> Result<TunnelFuture, TunnelError> {
        let program = tunnel::find_cloudflared(setup.cloudflared.as_deref())?;
        let args = tunnel::args(&setup.kind, port);
        Ok(Box::pin(tunnel::run(
            program,
            args,
            setup.kind.clone(),
            events,
        )))
    }
}

fn text(values: &serde_json::Map<String, Value>, id: &str) -> String {
    values
        .get(id)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Reads the tunnel options. A named tunnel without its name or hostname is
/// an error with a fixed text.
fn setup_from(values: &serde_json::Map<String, Value>) -> Result<TunnelSetup, &'static str> {
    let cloudflared = Some(text(values, "cloudflared")).filter(|s| !s.is_empty());
    let kind = if values.get("tunnel").and_then(Value::as_str) == Some("named") {
        let (tunnel, hostname) = (text(values, "tunnel_name"), text(values, "hostname"));
        if hostname.is_empty() {
            return Err("Set the tunnel's public hostname");
        }
        if tunnel.is_empty() {
            return Err(NO_TUNNEL_NAME_MESSAGE);
        }
        TunnelKind::Named { tunnel, hostname }
    } else {
        TunnelKind::Quick
    };
    Ok(TunnelSetup { kind, cloudflared })
}

/// The host part of a tunnel URL, for the status text.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

/// `push` lines for one forwarded body, each within `MAX_LINE`. JSON escaping
/// can grow a piece, so pieces are made smaller until every line fits.
fn push_lines(channel: &str, body: &str) -> Vec<String> {
    let mut max = MAX_LINE_BODY;
    while max >= 512 {
        let lines: Vec<String> = split_body(body, max)
            .iter()
            .map(|piece| protocol::push(channel, piece).to_string())
            .collect();
        if lines.iter().all(|l| l.len() < MAX_LINE) {
            return lines;
        }
        max /= 2;
    }
    Vec::new()
}

fn random_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| std::io::Error::other("no random numbers"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Posts a validation request to the tunnel for the check token and reports
/// whether the answer echoes the second token.
async fn probe(base: &str, token: &str) -> bool {
    let Ok(echo) = random_token() else {
        return false;
    };
    let url = format!("{base}/graph/{token}?validationToken={echo}");
    let attempt = async {
        let client = reqwest::Client::builder()
            .timeout(CHECK_TIMEOUT)
            .build()
            .ok()?;
        let response = client.post(url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        Some(response.text().await.ok()? == echo)
    };
    matches!(timeout(CHECK_TIMEOUT, attempt).await, Ok(Some(true)))
}

struct Plugin<W> {
    out: W,
    receiver: Receiver,
    launcher: Arc<dyn TunnelLauncher>,
    setup: Option<TunnelSetup>,
    task: Option<JoinHandle<()>>,
    up_url: Option<String>,
    backoff: Duration,
    started_at: Instant,
    restart_at: Option<Instant>,
    check_pending: bool,
    check_tx: mpsc::UnboundedSender<bool>,
}

impl<W: AsyncWrite + Unpin> Plugin<W> {
    async fn send(&mut self, message: Value) -> std::io::Result<()> {
        self.send_line(&message.to_string()).await
    }

    async fn send_line(&mut self, line: &str) -> std::io::Result<()> {
        self.out.write_all(line.as_bytes()).await?;
        self.out.write_all(b"\n").await?;
        self.out.flush().await
    }

    async fn status(&mut self, id: &str, state: &str, message: &str) -> std::io::Result<()> {
        self.send(protocol::option_status(id, state, message)).await
    }

    /// Stops the tunnel process (dropping its future kills the child).
    async fn stop_tunnel(&mut self, tunnel_rx: &mut Option<mpsc::Receiver<TunnelEvent>>) {
        *tunnel_rx = None;
        self.restart_at = None;
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }

    /// Starts the tunnel for the current setup, if there is a usable one.
    async fn start_tunnel(
        &mut self,
        tunnel_rx: &mut Option<mpsc::Receiver<TunnelEvent>>,
    ) -> std::io::Result<()> {
        let Some(setup) = self.setup.clone() else {
            return Ok(());
        };
        let (tx, rx) = mpsc::channel(16);
        match self.launcher.launch(&setup, self.receiver.port(), tx) {
            Ok(future) => {
                self.task = Some(tokio::spawn(future));
                *tunnel_rx = Some(rx);
                self.started_at = Instant::now();
                self.status("tunnel", "working", "Starting the tunnel")
                    .await
            }
            // No restart loop: a missing program stays missing until the
            // options change.
            Err(e) => self.status("tunnel", "error", &e.to_string()).await,
        }
    }

    async fn options(
        &mut self,
        values: &serde_json::Map<String, Value>,
        tunnel_rx: &mut Option<mpsc::Receiver<TunnelEvent>>,
    ) -> std::io::Result<()> {
        let new = match setup_from(values) {
            Ok(setup) => Some(setup),
            Err(message) => {
                self.change_tunnel(None, tunnel_rx).await?;
                return self.status("tunnel", "error", message).await;
            }
        };
        if new == self.setup {
            return Ok(());
        }
        self.change_tunnel(new, tunnel_rx).await?;
        self.start_tunnel(tunnel_rx).await
    }

    /// Replaces the setup, stopping the running tunnel.
    async fn change_tunnel(
        &mut self,
        new: Option<TunnelSetup>,
        tunnel_rx: &mut Option<mpsc::Receiver<TunnelEvent>>,
    ) -> std::io::Result<()> {
        let was_up = self.up_url.take().is_some();
        self.stop_tunnel(tunnel_rx).await;
        self.setup = new;
        self.backoff = FIRST_BACKOFF;
        if was_up {
            self.send(protocol::push_down("tunnel options changed"))
                .await?;
        }
        Ok(())
    }

    async fn tunnel_event(&mut self, event: TunnelEvent) -> std::io::Result<()> {
        match event {
            TunnelEvent::Up { url } => {
                self.up_url = Some(url.clone());
                self.send(protocol::push_ready(&url)).await?;
                let message = format!("Tunnel up at {}", host_of(&url));
                self.status("tunnel", "ok", &message).await
            }
            TunnelEvent::Down { reason } => {
                self.up_url = None;
                self.task = None;
                self.send(protocol::push_down(&reason)).await?;
                if self.started_at.elapsed() >= HEALTHY_RUN {
                    self.backoff = FIRST_BACKOFF;
                }
                self.restart_at = Some(Instant::now() + self.backoff);
                self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                self.status("tunnel", "error", &reason).await
            }
        }
    }

    async fn action(&mut self, id: &str) -> std::io::Result<()> {
        if id != "check" || self.check_pending {
            return Ok(());
        }
        let Some(base) = self.up_url.clone() else {
            return self.status("check", "error", NOT_UP_MESSAGE).await;
        };
        let Ok(token) = random_token() else {
            return self.status("check", "error", CHECK_FAILED_MESSAGE).await;
        };
        self.receiver.allow_check(Some(token.clone()));
        self.check_pending = true;
        let done = self.check_tx.clone();
        tokio::spawn(async move {
            let _ = done.send(probe(&base, &token).await);
        });
        self.status("check", "working", "Checking the tunnel").await
    }

    async fn check_done(&mut self, ok: bool) -> std::io::Result<()> {
        self.check_pending = false;
        self.receiver.allow_check(None);
        if ok {
            self.status("check", "ok", CHECK_OK_MESSAGE).await
        } else {
            self.status("check", "error", CHECK_FAILED_MESSAGE).await
        }
    }

    async fn forward(&mut self, forward: Forward) -> std::io::Result<()> {
        for line in push_lines(&forward.channel, &forward.body) {
            self.send_line(&line).await?;
        }
        Ok(())
    }
}

/// Runs the plugin until stdin closes or Chaat sends `shutdown`.
pub async fn serve<R, W>(
    input: R,
    output: W,
    launcher: Arc<dyn TunnelLauncher>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let (receiver, mut forwards) = Receiver::start().await?;
    let (lines_tx, mut lines) = mpsc::channel::<String>(64);
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(input);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > MAX_LINE => {}
                Ok(_) => {
                    if lines_tx.send(line).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let (check_tx, mut check_rx) = mpsc::unbounded_channel();
    let mut tunnel_rx: Option<mpsc::Receiver<TunnelEvent>> = None;
    let mut plugin = Plugin {
        out: output,
        receiver,
        launcher,
        setup: None,
        task: None,
        up_url: None,
        backoff: FIRST_BACKOFF,
        started_at: Instant::now(),
        restart_at: None,
        check_pending: false,
        check_tx,
    };

    let result = async {
        loop {
            let restart_at = plugin.restart_at;
            select! {
                line = lines.recv() => {
                    let Some(line) = line else { break };
                    match protocol::parse_incoming(line.trim()) {
                        Some(Incoming::Hello) => plugin.send(protocol::ready()).await?,
                        Some(Incoming::Options { values }) => {
                            plugin.options(&values, &mut tunnel_rx).await?;
                        }
                        Some(Incoming::PushChannels { channels }) => {
                            plugin.receiver.set_channels(channels);
                        }
                        Some(Incoming::Action { id }) => plugin.action(&id).await?,
                        Some(Incoming::Shutdown) => break,
                        None => {}
                    }
                }
                Some(forward) = forwards.recv() => plugin.forward(forward).await?,
                event = async {
                    match tunnel_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // A tunnel task that ended without saying so counts as down.
                    let event = event.unwrap_or(TunnelEvent::Down {
                        reason: "cloudflared stopped".into(),
                    });
                    if matches!(event, TunnelEvent::Down { .. }) {
                        tunnel_rx = None;
                    }
                    plugin.tunnel_event(event).await?;
                }
                _ = async {
                    match restart_at {
                        Some(at) => sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    plugin.restart_at = None;
                    plugin.start_tunnel(&mut tunnel_rx).await?;
                }
                Some(ok) = check_rx.recv() => plugin.check_done(ok).await?,
            }
        }
        Ok::<(), std::io::Error>(())
    }
    .await;

    plugin.stop_tunnel(&mut tunnel_rx).await;
    reader.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    /// One tunnel start, as the stand-in launcher saw it.
    struct Launch {
        setup: TunnelSetup,
        port: u16,
        events: mpsc::Sender<TunnelEvent>,
        dropped: Arc<AtomicBool>,
    }

    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct Stub {
        launches: mpsc::UnboundedSender<Launch>,
        error: Option<TunnelError>,
    }

    impl TunnelLauncher for Stub {
        fn launch(
            &self,
            setup: &TunnelSetup,
            port: u16,
            events: mpsc::Sender<TunnelEvent>,
        ) -> Result<TunnelFuture, TunnelError> {
            if let Some(e) = self.error {
                return Err(e);
            }
            let dropped = Arc::new(AtomicBool::new(false));
            let flag = DropFlag(dropped.clone());
            let _ = self.launches.send(Launch {
                setup: setup.clone(),
                port,
                events,
                dropped,
            });
            Ok(Box::pin(async move {
                let _flag = flag;
                std::future::pending::<()>().await;
            }))
        }
    }

    struct Harness {
        to_plugin: WriteHalf<DuplexStream>,
        from_plugin: BufReader<ReadHalf<DuplexStream>>,
        launches: mpsc::UnboundedReceiver<Launch>,
        done: JoinHandle<std::io::Result<()>>,
    }

    fn harness(error: Option<TunnelError>) -> Harness {
        let (c, d) = tokio::io::duplex(1 << 20);
        let (e, f) = tokio::io::duplex(1 << 20);
        let (launches_tx, launches) = mpsc::unbounded_channel();
        let done = tokio::spawn(serve(
            c,
            f,
            Arc::new(Stub {
                launches: launches_tx,
                error,
            }),
        ));
        Harness {
            to_plugin: tokio::io::split(d).1,
            from_plugin: BufReader::new(tokio::io::split(e).0),
            launches,
            done,
        }
    }

    impl Harness {
        async fn say(&mut self, line: &str) {
            self.to_plugin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn recv(&mut self) -> Value {
            let mut line = String::new();
            let n = timeout(
                Duration::from_secs(5),
                self.from_plugin.read_line(&mut line),
            )
            .await
            .expect("plugin said nothing")
            .unwrap();
            assert!(n > 0, "plugin closed its output");
            assert!(line.len() < MAX_LINE, "line over the limit");
            serde_json::from_str(&line).unwrap()
        }

        async fn launch(&mut self) -> Launch {
            timeout(Duration::from_secs(5), self.launches.recv())
                .await
                .expect("no tunnel started")
                .unwrap()
        }

        /// Sends default options and waits for the quick tunnel to start.
        async fn start(&mut self) -> Launch {
            self.say(r#"{"v":1,"type":"options","values":{}}"#).await;
            let launch = self.launch().await;
            let status = self.recv().await;
            assert_eq!(status["message"], "Starting the tunnel");
            launch
        }
    }

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/plugin-protocol/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(std::fs::read_to_string(path).unwrap().trim()).unwrap()
    }

    const CHANNEL: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJ0123456";

    async fn http_post(port: u16, path: &str, body: &str) -> u16 {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut answer)
            .await
            .unwrap();
        answer
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap()
    }

    #[tokio::test]
    async fn hello_gets_ready() {
        let mut h = harness(None);
        h.say(&fixture("hello.json").to_string()).await;
        assert_eq!(h.recv().await, protocol::ready());
    }

    #[tokio::test]
    async fn tunnel_up_sends_push_ready_and_down_sends_push_down() {
        let mut h = harness(None);
        let launch = h.start().await;
        assert_eq!(launch.setup.kind, TunnelKind::Quick);
        launch
            .events
            .send(TunnelEvent::Up {
                url: "https://example-words.trycloudflare.com".into(),
            })
            .await
            .unwrap();
        assert_eq!(h.recv().await, fixture("push_ready.json"));
        let status = h.recv().await;
        assert_eq!(status["id"], "tunnel");
        assert_eq!(status["state"], "ok");
        assert_eq!(
            status["message"],
            "Tunnel up at example-words.trycloudflare.com"
        );
        launch
            .events
            .send(TunnelEvent::Down {
                reason: "cloudflared stopped".into(),
            })
            .await
            .unwrap();
        assert_eq!(h.recv().await, fixture("push_down.json"));
    }

    #[tokio::test]
    async fn channels_from_chaat_reach_the_receiver() {
        let mut h = harness(None);
        let launch = h.start().await;
        // Unknown channel: 404 and nothing forwarded.
        let path = format!("/graph/{CHANNEL}");
        assert_eq!(http_post(launch.port, &path, "{}").await, 404);
        h.say(&fixture("push_channels.json").to_string()).await;
        // Wait until the channel is known (the message is read asynchronously).
        let body = fixture("push.json")["body"].as_str().unwrap().to_string();
        let mut status = 404;
        for _ in 0..50 {
            status = http_post(launch.port, &path, &body).await;
            if status == 202 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(status, 202);
        assert_eq!(h.recv().await, fixture("push.json"));
    }

    #[tokio::test]
    async fn push_lines_fit_the_line_limit() {
        let mut h = harness(None);
        let launch = h.start().await;
        h.say(&fixture("push_channels.json").to_string()).await;
        let entry = |n: usize| {
            format!(
                r#"{{"subscriptionId":"sub-{n}","clientState":"state","changeType":"created","resource":"chats('19:x')/messages('{n}')","pad":"{}"}}"#,
                "a".repeat(900)
            )
        };
        let entries: Vec<String> = (0..220).map(entry).collect();
        let body = format!(r#"{{"value":[{}]}}"#, entries.join(","));
        assert!(body.len() > 200 * 1024 && body.len() < 512 * 1024);
        let path = format!("/graph/{CHANNEL}");
        let mut status = 404;
        for _ in 0..50 {
            status = http_post(launch.port, &path, &body).await;
            if status == 202 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(status, 202);
        let mut seen = 0;
        let mut lines = 0;
        while seen < entries.len() {
            let line = h.recv().await; // recv asserts the line is under 64 KiB
            assert_eq!(line["type"], "push");
            assert_eq!(line["channel"], CHANNEL);
            let piece: Value = serde_json::from_str(line["body"].as_str().unwrap()).unwrap();
            seen += piece["value"].as_array().unwrap().len();
            lines += 1;
        }
        assert!(lines > 1);
        assert_eq!(seen, entries.len());
    }

    #[test]
    fn escaping_that_grows_a_piece_is_split_again() {
        // Every entry is mostly quotes, so escaping doubles it.
        let entry = format!(r#"{{"q":"{}"}}"#, "\\\"".repeat(2000));
        let body = format!(r#"{{"value":[{}]}}"#, vec![entry; 40].join(","));
        let lines = push_lines(CHANNEL, &body);
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|l| l.len() < MAX_LINE));
    }

    #[tokio::test]
    async fn check_action_reports_ok_and_error() {
        let mut h = harness(None);
        let launch = h.start().await;
        // Before the tunnel is up.
        h.say(&fixture("action.json").to_string().replace("find", "check"))
            .await;
        let status = h.recv().await;
        assert_eq!(status["id"], "check");
        assert_eq!(status["state"], "error");
        assert_eq!(status["message"], NOT_UP_MESSAGE);
        // Up at the local receiver, over plain http.
        launch
            .events
            .send(TunnelEvent::Up {
                url: format!("http://127.0.0.1:{}", launch.port),
            })
            .await
            .unwrap();
        h.recv().await; // push_ready
        h.recv().await; // tunnel status
        h.say(r#"{"v":1,"type":"action","id":"check"}"#).await;
        let working = h.recv().await;
        assert_eq!(working["state"], "working");
        let ok = h.recv().await;
        assert_eq!(ok["id"], "check");
        assert_eq!(ok["state"], "ok");
        // Up at a closed port.
        let closed = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        launch
            .events
            .send(TunnelEvent::Up {
                url: format!("http://127.0.0.1:{closed}"),
            })
            .await
            .unwrap();
        h.recv().await;
        h.recv().await;
        h.say(r#"{"v":1,"type":"action","id":"check"}"#).await;
        assert_eq!(h.recv().await["state"], "working");
        let failed = h.recv().await;
        assert_eq!(failed["state"], "error");
        assert_eq!(failed["message"], CHECK_FAILED_MESSAGE);
    }

    #[tokio::test]
    async fn missing_cloudflared_is_an_error_without_a_restart_loop() {
        let mut h = harness(Some(TunnelError::NotFound));
        h.say(r#"{"v":1,"type":"options","values":{}}"#).await;
        let status = h.recv().await;
        assert_eq!(status["id"], "tunnel");
        assert_eq!(status["state"], "error");
        assert_eq!(status["message"], "cloudflared was not found");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(h.launches.try_recv().is_err());
    }

    #[tokio::test]
    async fn named_tunnel_without_hostname_is_an_error() {
        let mut h = harness(None);
        h.say(r#"{"v":1,"type":"options","values":{"tunnel":"named","tunnel_name":"chaat"}}"#)
            .await;
        let status = h.recv().await;
        assert_eq!(status["state"], "error");
        assert_eq!(status["message"], "Set the tunnel's public hostname");
        assert!(h.launches.try_recv().is_err());
    }

    #[tokio::test]
    async fn changed_tunnel_options_restart_the_tunnel() {
        let mut h = harness(None);
        let first = h.start().await;
        // Same options: nothing happens.
        h.say(r#"{"v":1,"type":"options","values":{"tunnel":"quick"}}"#)
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(h.launches.try_recv().is_err());
        first
            .events
            .send(TunnelEvent::Up {
                url: "https://a.trycloudflare.com".into(),
            })
            .await
            .unwrap();
        h.recv().await;
        h.recv().await;
        h.say(
            r#"{"v":1,"type":"options","values":{"tunnel":"named","tunnel_name":"chaat","hostname":"push.example.com"}}"#,
        )
        .await;
        let second = h.launch().await;
        assert_eq!(
            second.setup.kind,
            TunnelKind::Named {
                tunnel: "chaat".into(),
                hostname: "push.example.com".into()
            }
        );
        assert!(first.dropped.load(Ordering::SeqCst));
        assert_eq!(h.recv().await["type"], "push_down");
    }

    #[tokio::test(start_paused = true)]
    async fn the_tunnel_restarts_with_doubling_backoff() {
        let mut h = harness(None);
        h.say(r#"{"v":1,"type":"options","values":{}}"#).await;
        let down = |l: &Launch| {
            let events = l.events.clone();
            async move {
                events
                    .send(TunnelEvent::Down {
                        reason: "cloudflared stopped".into(),
                    })
                    .await
                    .unwrap();
            }
        };
        let first = h.launch().await;
        down(&first).await;
        tokio::time::advance(Duration::from_millis(900)).await;
        tokio::task::yield_now().await;
        assert!(h.launches.try_recv().is_err());
        let second = h.launch().await;
        down(&second).await;
        tokio::time::advance(Duration::from_millis(1900)).await;
        tokio::task::yield_now().await;
        assert!(h.launches.try_recv().is_err());
        let third = h.launch().await;
        // A run of a minute resets the wait to one second.
        tokio::time::advance(Duration::from_secs(61)).await;
        down(&third).await;
        tokio::time::advance(Duration::from_millis(1100)).await;
        let fourth = h.launch().await;
        drop(fourth);
    }

    #[tokio::test]
    async fn exits_when_stdin_closes() {
        let mut h = harness(None);
        let launch = h.start().await;
        drop(h.to_plugin);
        let result = timeout(Duration::from_secs(5), h.done)
            .await
            .expect("serve did not exit")
            .unwrap();
        assert!(result.is_ok());
        assert!(launch.dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn exits_on_shutdown() {
        let mut h = harness(None);
        let launch = h.start().await;
        h.say(&fixture("shutdown.json").to_string()).await;
        let result = timeout(Duration::from_secs(5), h.done)
            .await
            .expect("serve did not exit")
            .unwrap();
        assert!(result.is_ok());
        assert!(launch.dropped.load(Ordering::SeqCst));
    }
}
