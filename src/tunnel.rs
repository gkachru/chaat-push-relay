//! The cloudflared child process.
//!
//! This module only runs a cloudflared the person already installed; it never
//! installs or updates one. Output lines are parsed for the public address and
//! never logged. Restart and backoff belong to the caller.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Which kind of tunnel to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelKind {
    /// A throwaway `trycloudflare.com` address.
    Quick,
    /// A tunnel the person set up. `tunnel` is a name, or a path ending in
    /// `.yml`, `.yaml` or `.json` that is used as `--config`.
    Named { tunnel: String, hostname: String },
}

/// Why a tunnel cannot be started. The texts are fixed and safe to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelError {
    NotFound,
    NoHostname,
}

impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TunnelError::NotFound => "cloudflared was not found",
            TunnelError::NoHostname => "Set the tunnel's public hostname",
        })
    }
}

impl std::error::Error for TunnelError {}

/// What the running tunnel reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelEvent {
    Up { url: String },
    Down { reason: String },
}

/// Finds cloudflared: an explicit path must exist; otherwise search `PATH`.
pub fn find_cloudflared(option: Option<&str>) -> Result<PathBuf, TunnelError> {
    find_in(option, std::env::var_os("PATH"))
}

fn find_in(option: Option<&str>, path: Option<OsString>) -> Result<PathBuf, TunnelError> {
    if let Some(given) = option.map(str::trim).filter(|s| !s.is_empty()) {
        let given = Path::new(given);
        return if given.is_file() {
            Ok(given.to_path_buf())
        } else {
            Err(TunnelError::NotFound)
        };
    }
    let path = path.ok_or(TunnelError::NotFound)?;
    for dir in std::env::split_paths(&path) {
        for name in ["cloudflared", "cloudflared.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(TunnelError::NotFound)
}

fn is_config_path(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [".yml", ".yaml", ".json"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// The cloudflared arguments for a tunnel that forwards to 127.0.0.1:`port`.
pub fn args(kind: &TunnelKind, port: u16) -> Vec<String> {
    let url = format!("http://127.0.0.1:{port}");
    let mut out: Vec<String> = ["tunnel", "--no-autoupdate"]
        .into_iter()
        .map(String::from)
        .collect();
    match kind {
        TunnelKind::Quick => {
            out.extend(["--protocol", "http2", "--url"].map(String::from));
            out.push(url);
        }
        TunnelKind::Named { tunnel, .. } if is_config_path(tunnel) => {
            out.extend(["--config".to_string(), tunnel.clone()]);
            out.extend(["--protocol", "http2", "run", "--url"].map(String::from));
            out.push(url);
        }
        TunnelKind::Named { tunnel, .. } => {
            out.extend(["--protocol", "http2", "run", "--url"].map(String::from));
            out.push(url);
            out.push(tunnel.clone());
        }
    }
    out
}

/// The `https://<label>.trycloudflare.com` address in an output line. The
/// `api.trycloudflare.com` host that cloudflared also prints is ignored.
pub fn quick_url(line: &str) -> Option<String> {
    const SUFFIX: &str = ".trycloudflare.com";
    let mut rest = line;
    while let Some(at) = rest.find("https://") {
        let after = &rest[at + "https://".len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
            .unwrap_or(after.len());
        let host = &after[..end];
        if let Some(label) = host.strip_suffix(SUFFIX)
            && !label.is_empty()
            && !label.contains('.')
            && !label.eq_ignore_ascii_case("api")
        {
            return Some(format!("https://{host}"));
        }
        rest = after;
    }
    None
}

/// True for the line cloudflared prints when a named tunnel is connected.
pub fn named_ready(line: &str) -> bool {
    line.contains("Registered tunnel connection")
}

/// How long to keep reading output after the child has exited.
const DRAIN: Duration = Duration::from_millis(500);

/// Runs cloudflared and reports `Up` once and `Down` when it ends. The child
/// is killed if this future is dropped.
pub async fn run(
    program: PathBuf,
    args: Vec<String>,
    kind: TunnelKind,
    events: mpsc::Sender<TunnelEvent>,
) {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: no console window for the child.
        command.creation_flags(0x0800_0000);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            let _ = events
                .send(TunnelEvent::Down {
                    reason: "cloudflared could not start".into(),
                })
                .await;
            return;
        }
    };
    let (lines_tx, mut lines) = mpsc::channel::<String>(64);
    if let Some(out) = child.stdout.take() {
        tokio::spawn(read_lines(out, lines_tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(read_lines(err, lines_tx.clone()));
    }
    drop(lines_tx);

    let mut up = false;
    let mut exited = false;
    loop {
        let line = if exited {
            match tokio::time::timeout(DRAIN, lines.recv()).await {
                Ok(Some(line)) => line,
                _ => break,
            }
        } else {
            tokio::select! {
                line = lines.recv() => match line {
                    Some(line) => line,
                    None => {
                        // Output closed; wait for the exit itself.
                        let _ = child.wait().await;
                        break;
                    }
                },
                _ = child.wait() => {
                    exited = true;
                    continue;
                }
            }
        };
        if up {
            continue;
        }
        let url = match &kind {
            TunnelKind::Quick => quick_url(&line),
            TunnelKind::Named { hostname, .. } => {
                named_ready(&line).then(|| format!("https://{hostname}"))
            }
        };
        if let Some(url) = url {
            up = true;
            if events.send(TunnelEvent::Up { url }).await.is_err() {
                return;
            }
        }
    }
    let _ = events
        .send(TunnelEvent::Down {
            reason: "cloudflared stopped".into(),
        })
        .await;
}

/// Longest piece of output read as one line.
const MAX_LINE: u64 = 16 * 1024;

async fn read_lines<R: AsyncRead + Unpin>(reader: R, tx: mpsc::Sender<String>) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Cap one read so a child that never prints a newline cannot grow
        // the buffer without bound; the rest arrives as the next line.
        match (&mut reader)
            .take(MAX_LINE)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                if tx.send(line).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn quick_args() {
        assert_eq!(
            args(&TunnelKind::Quick, 8080),
            strings(&[
                "tunnel",
                "--no-autoupdate",
                "--protocol",
                "http2",
                "--url",
                "http://127.0.0.1:8080"
            ])
        );
    }

    #[test]
    fn named_args_by_name() {
        let kind = TunnelKind::Named {
            tunnel: "chaat".into(),
            hostname: "push.example.com".into(),
        };
        assert_eq!(
            args(&kind, 9),
            strings(&[
                "tunnel",
                "--no-autoupdate",
                "--protocol",
                "http2",
                "run",
                "--url",
                "http://127.0.0.1:9",
                "chaat"
            ])
        );
    }

    #[test]
    fn named_args_by_config_file() {
        for path in ["C:\\t\\config.yml", "/t/c.yaml", "/t/c.JSON"] {
            let kind = TunnelKind::Named {
                tunnel: path.into(),
                hostname: "h.example.com".into(),
            };
            assert_eq!(
                args(&kind, 7),
                strings(&[
                    "tunnel",
                    "--no-autoupdate",
                    "--config",
                    path,
                    "--protocol",
                    "http2",
                    "run",
                    "--url",
                    "http://127.0.0.1:7"
                ])
            );
        }
    }

    #[test]
    fn quick_url_from_the_banner() {
        assert_eq!(
            quick_url("INF |  https://quiet-river-12ab.trycloudflare.com   |"),
            Some("https://quiet-river-12ab.trycloudflare.com".to_string())
        );
        assert_eq!(
            quick_url("x https://a-b.trycloudflare.com/path?q=1"),
            Some("https://a-b.trycloudflare.com".to_string())
        );
    }

    #[test]
    fn quick_url_ignores_api_host() {
        assert_eq!(
            quick_url("Requesting new quick Tunnel on https://api.trycloudflare.com ..."),
            None
        );
        // The real address on the same line still wins.
        assert_eq!(
            quick_url("https://api.trycloudflare.com then https://calm-fox-9.trycloudflare.com"),
            Some("https://calm-fox-9.trycloudflare.com".to_string())
        );
    }

    #[test]
    fn quick_url_rejects_lookalikes() {
        assert_eq!(quick_url("https://trycloudflare.com"), None);
        assert_eq!(quick_url("https://a.trycloudflare.com.evil.org"), None);
        assert_eq!(quick_url("https://a.b.trycloudflare.com"), None);
        assert_eq!(quick_url("http://a.trycloudflare.com"), None);
        assert_eq!(quick_url("nothing here"), None);
    }

    #[test]
    fn named_ready_matches_the_registration_line() {
        assert!(named_ready(
            "INF Registered tunnel connection connIndex=0 location=fra"
        ));
        assert!(!named_ready("INF Starting tunnel"));
    }

    #[test]
    fn error_texts_are_fixed() {
        assert_eq!(
            TunnelError::NotFound.to_string(),
            "cloudflared was not found"
        );
        assert_eq!(
            TunnelError::NoHostname.to_string(),
            "Set the tunnel's public hostname"
        );
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chaat-push-relay-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn find_uses_an_existing_option_path() {
        let dir = scratch_dir("opt");
        let file = dir.join("tool");
        std::fs::write(&file, b"").unwrap();
        let found = find_in(Some(file.to_str().unwrap()), None).unwrap();
        assert_eq!(found, file);
        let missing = dir.join("missing");
        assert_eq!(
            find_in(Some(missing.to_str().unwrap()), None),
            Err(TunnelError::NotFound)
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn find_searches_the_path_variable() {
        let dir = scratch_dir("path");
        let file = dir.join("cloudflared.exe");
        std::fs::write(&file, b"").unwrap();
        let var = std::env::join_paths([scratch_dir("empty"), dir.clone()]).unwrap();
        assert_eq!(find_in(None, Some(var.clone())).unwrap(), file);
        // An empty option means "use PATH".
        assert_eq!(find_in(Some("  "), Some(var)).unwrap(), file);
        assert_eq!(find_in(None, None), Err(TunnelError::NotFound));
        std::fs::remove_dir_all(dir).ok();
    }

    /// A stand-in for cloudflared that prints one line and exits.
    fn echo(text: &str) -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            (
                PathBuf::from("cmd"),
                vec!["/C".into(), format!("echo {text}")],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec!["-c".into(), format!("echo '{text}'")],
            )
        }
    }

    async fn collect(program: PathBuf, args: Vec<String>, kind: TunnelKind) -> Vec<TunnelEvent> {
        let (tx, mut rx) = mpsc::channel(8);
        run(program, args, kind, tx).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn run_reports_quick_url_then_stopped() {
        let (program, args) = echo("INF https://quiet-river-12ab.trycloudflare.com");
        let events = collect(program, args, TunnelKind::Quick).await;
        assert_eq!(
            events,
            vec![
                TunnelEvent::Up {
                    url: "https://quiet-river-12ab.trycloudflare.com".into()
                },
                TunnelEvent::Down {
                    reason: "cloudflared stopped".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn run_reports_named_hostname_once_registered() {
        let (program, args) = echo("INF Registered tunnel connection connIndex=0");
        let kind = TunnelKind::Named {
            tunnel: "t".into(),
            hostname: "push.example.com".into(),
        };
        let events = collect(program, args, kind).await;
        assert_eq!(
            events,
            vec![
                TunnelEvent::Up {
                    url: "https://push.example.com".into()
                },
                TunnelEvent::Down {
                    reason: "cloudflared stopped".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn run_without_a_url_only_reports_stopped() {
        let (program, args) = echo("hello");
        let events = collect(program, args, TunnelKind::Quick).await;
        assert_eq!(
            events,
            vec![TunnelEvent::Down {
                reason: "cloudflared stopped".into()
            }]
        );
    }

    #[tokio::test]
    async fn read_lines_caps_a_line_without_newline() {
        let data = vec![b'a'; MAX_LINE as usize * 2 + 5];
        let (tx, mut rx) = mpsc::channel(8);
        read_lines(&data[..], tx).await;
        let mut sizes = Vec::new();
        while let Ok(line) = rx.try_recv() {
            sizes.push(line.len());
        }
        assert_eq!(sizes, vec![MAX_LINE as usize, MAX_LINE as usize, 5]);
    }

    #[tokio::test]
    async fn run_reports_a_program_that_cannot_start() {
        let program = std::env::temp_dir().join("chaat-push-relay-no-such-program");
        let events = collect(program, Vec::new(), TunnelKind::Quick).await;
        assert_eq!(
            events,
            vec![TunnelEvent::Down {
                reason: "cloudflared could not start".into()
            }]
        );
    }
}
