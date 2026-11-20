//! The plugin protocol with Chaat: newline-delimited JSON, `"v": 1`.
//! See Chaat's `docs/plugins.md`. Fixtures in `tests/fixtures/plugin-protocol/`
//! are copies of Chaat's and must stay identical.

use serde_json::{Map, Value, json};

pub const PROTOCOL_VERSION: u64 = 1;

/// What Chaat sends us.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Hello,
    Options {
        values: Map<String, Value>,
    },
    /// The channels Chaat announces, one per account backend.
    PushChannels {
        channels: Vec<String>,
    },
    /// The person clicked an action button.
    Action {
        id: String,
    },
    Shutdown,
}

/// Parses one line. Unknown message types, other protocol versions and lines
/// that are not JSON objects give `None`; unknown fields are ignored and a
/// field of the wrong type reads as absent.
pub fn parse_incoming(line: &str) -> Option<Incoming> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("v")?.as_u64()? != PROTOCOL_VERSION {
        return None;
    }
    match value.get("type")?.as_str()? {
        "hello" => Some(Incoming::Hello),
        "options" => Some(Incoming::Options {
            values: value
                .get("values")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        }),
        "push_channels" => Some(Incoming::PushChannels {
            channels: value
                .get("channels")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        }),
        "action" => Some(Incoming::Action {
            id: value.get("id")?.as_str()?.to_string(),
        }),
        "shutdown" => Some(Incoming::Shutdown),
        _ => None,
    }
}

/// The `ready` message.
pub fn ready() -> Value {
    json!({
        "v": PROTOCOL_VERSION,
        "type": "ready",
        "capabilities": ["push"],
        "needs": ["push"],
        "name": "Push relay",
        "description": "Receives Microsoft Graph change notifications through a cloudflared tunnel on this computer and passes them to Chaat.",
        "options": [
            {
                "id": "tunnel", "kind": "choice", "label": "Tunnel",
                "section": "Tunnel",
                "help": "A quick tunnel needs no account but gets a new address each time. A named tunnel keeps its address.",
                "default": "quick",
                "choices": [
                    {"value": "quick", "label": "Quick tunnel"},
                    {"value": "named", "label": "Named tunnel"},
                ],
            },
            {
                "id": "cloudflared", "kind": "text", "label": "cloudflared program",
                "section": "Tunnel", "advanced": true,
                "placeholder": "cloudflared on PATH",
                "help": "The full path of cloudflared, if it is not on your PATH.",
            },
            {
                "id": "tunnel_name", "kind": "text", "label": "Tunnel name or config",
                "section": "Tunnel",
                "help": "A tunnel name, or the path of its config file",
                "visible_when": {"option": "tunnel", "equals": "named"},
            },
            {
                "id": "hostname", "kind": "text", "label": "Public hostname",
                "section": "Tunnel", "format": "hostname",
                "help": "The hostname the tunnel is published at.",
                "visible_when": {"option": "tunnel", "equals": "named"},
            },
            {
                "id": "check", "kind": "action", "label": "Check the tunnel",
                "section": "Tunnel",
            },
        ],
    })
}

/// `option_status`: `state` is one of "ok", "error", "working". Callers pass
/// fixed text only.
pub fn option_status(id: &str, state: &str, message: &str) -> Value {
    json!({
        "v": PROTOCOL_VERSION, "type": "option_status",
        "id": id, "state": state, "message": message,
    })
}

/// The tunnel is up at `url`.
pub fn push_ready(url: &str) -> Value {
    json!({"v": PROTOCOL_VERSION, "type": "push_ready", "url": url})
}

/// One notification body for `channel`.
pub fn push(channel: &str, body: &str) -> Value {
    json!({"v": PROTOCOL_VERSION, "type": "push", "channel": channel, "body": body})
}

/// The tunnel is down. Callers pass fixed text only.
pub fn push_down(reason: &str) -> Value {
    json!({"v": PROTOCOL_VERSION, "type": "push_down", "reason": reason})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/plugin-protocol/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path).unwrap().trim().to_string()
    }

    fn fixture_value(name: &str) -> Value {
        serde_json::from_str(&fixture(name)).unwrap()
    }

    #[test]
    fn every_copied_fixture_parses() {
        assert_eq!(
            parse_incoming(&fixture("hello.json")),
            Some(Incoming::Hello)
        );
        assert_eq!(
            parse_incoming(&fixture("shutdown.json")),
            Some(Incoming::Shutdown)
        );
        assert_eq!(
            parse_incoming(&fixture("action.json")),
            Some(Incoming::Action { id: "find".into() })
        );
        assert_eq!(
            parse_incoming(&fixture("push_channels.json")),
            Some(Incoming::PushChannels {
                channels: vec!["abcdefghijklmnopqrstuvwxyzABCDEFGHIJ0123456".into()]
            })
        );
        let Some(Incoming::Options { values }) = parse_incoming(&fixture("options.json")) else {
            panic!("options fixture did not parse");
        };
        assert_eq!(values["brightness"], "high");
    }

    #[test]
    fn unknown_types_versions_and_junk_are_ignored() {
        assert_eq!(parse_incoming(r#"{"v":1,"type":"presence"}"#), None);
        assert_eq!(parse_incoming(r#"{"v":2,"type":"hello"}"#), None);
        assert_eq!(parse_incoming(r#"{"type":"hello"}"#), None);
        assert_eq!(parse_incoming("not json"), None);
        assert_eq!(parse_incoming("[]"), None);
        // Wrong-typed channels read as none.
        assert_eq!(
            parse_incoming(r#"{"v":1,"type":"push_channels","channels":5}"#),
            Some(Incoming::PushChannels { channels: vec![] })
        );
    }

    #[test]
    fn sent_lines_match_the_fixtures() {
        assert_eq!(
            push_ready("https://example-words.trycloudflare.com"),
            fixture_value("push_ready.json")
        );
        assert_eq!(
            push_down("cloudflared stopped"),
            fixture_value("push_down.json")
        );
        assert_eq!(
            option_status("find", "ok", "Found 2 bulbs"),
            fixture_value("option_status.json")
        );
        let fixture = fixture_value("push.json");
        assert_eq!(
            push(
                fixture["channel"].as_str().unwrap(),
                fixture["body"].as_str().unwrap()
            ),
            fixture
        );
    }

    #[test]
    fn ready_matches_the_brief() {
        let ready = ready();
        assert_eq!(ready["v"], 1);
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["capabilities"], json!(["push"]));
        assert_eq!(ready["needs"], json!(["push"]));
        assert_eq!(ready["name"], "Push relay");
        assert!(ready.get("docs_url").is_none());
        let options = ready["options"].as_array().unwrap();
        let ids: Vec<&str> = options.iter().map(|o| o["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            ["tunnel", "cloudflared", "tunnel_name", "hostname", "check"]
        );
        assert_eq!(options[0]["default"], "quick");
        assert_eq!(options[1]["placeholder"], "cloudflared on PATH");
        assert_eq!(options[1]["advanced"], true);
        assert_eq!(
            options[2]["visible_when"],
            json!({"option": "tunnel", "equals": "named"})
        );
        assert_eq!(options[3]["format"], "hostname");
        assert_eq!(options[4]["kind"], "action");
        assert_eq!(options[4]["label"], "Check the tunnel");
        // Chaat's limits: ids of a-z, 0-9, `_` up to 32; labels up to 60;
        // help up to 200; sections up to 40; no default on an action.
        for o in options {
            let id = o["id"].as_str().unwrap();
            assert!(
                id.len() <= 32
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
            assert!(o["label"].as_str().unwrap().chars().count() <= 60);
            assert!(o["section"].as_str().unwrap().chars().count() <= 40);
            if let Some(h) = o.get("help") {
                assert!(h.as_str().unwrap().chars().count() <= 200);
            }
        }
        assert!(options[4].get("default").is_none());
    }
}
