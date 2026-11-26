//! Remembered tunnel IDs.
//!
//! Starting a named tunnel by name makes cloudflared ask Cloudflare's API
//! which ID the name has. When that API fails (it answered 503 on
//! 2026-11-26) the tunnel cannot start, although starting by ID needs only
//! the local credentials file. So the ID cloudflared prints when a tunnel by
//! name comes up is remembered here, and later starts use it. A start by a
//! remembered ID that fails before the tunnel is up forgets it, and the next
//! start goes by name again.
//!
//! The file maps tunnel names to IDs, nothing else. IDs are not secret: they
//! name the credentials file and appear in the DNS record.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FILE: &str = "tunnel-ids.json";

/// Where the plugin keeps its few files: `%LOCALAPPDATA%` on Windows,
/// `~/Library/Application Support` on macOS, the XDG state directory
/// elsewhere.
pub fn data_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(state)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".local/state")
    };
    Some(base.join("chaat-push-relay"))
}

/// Whether `text` is a tunnel ID (a UUID), so it needs no lookup.
pub fn is_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Tunnel names and the IDs they had, kept in one file.
#[derive(Debug, Clone)]
pub struct IdCache {
    path: PathBuf,
}

impl IdCache {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE),
        }
    }

    /// The cache in [`data_dir`], when there is one.
    pub fn default_location() -> Option<Self> {
        data_dir().map(|dir| Self::in_dir(&dir))
    }

    fn read(&self) -> BTreeMap<String, String> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn write(&self, ids: &BTreeMap<String, String>) {
        let Some(dir) = self.path.parent() else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        // Through a temporary file and a rename, so a crash leaves the old
        // file or the new one.
        let temp = self.path.with_extension("json.tmp");
        let Ok(text) = serde_json::to_vec_pretty(ids) else {
            return;
        };
        if std::fs::write(&temp, text).is_ok() && std::fs::rename(&temp, &self.path).is_err() {
            std::fs::remove_file(&temp).ok();
        }
    }

    /// The ID remembered for `name`.
    pub fn get(&self, name: &str) -> Option<String> {
        self.read().remove(name).filter(|id| is_id(id))
    }

    pub fn remember(&self, name: &str, id: &str) {
        let mut ids = self.read();
        if is_id(id) && ids.get(name).map(String::as_str) != Some(id) {
            ids.insert(name.to_owned(), id.to_owned());
            self.write(&ids);
        }
    }

    pub fn forget(&self, name: &str) {
        let mut ids = self.read();
        if ids.remove(name).is_some() {
            self.write(&ids);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "aac14e53-a718-4a8b-a55f-2980aaf379cd";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chaat-push-relay-ids-{name}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn ids_are_uuids() {
        assert!(is_id(ID));
        assert!(is_id(&ID.to_uppercase()));
        assert!(!is_id("chaat-relay"));
        assert!(!is_id("aac14e53a718-4a8b-a55f-2980aaf379cd-"));
        assert!(!is_id(&ID.replace('a', "g")));
    }

    #[test]
    fn remembers_and_forgets_by_name() {
        let dir = scratch("cache");
        let cache = IdCache::in_dir(&dir);
        assert_eq!(cache.get("chaat-relay"), None);
        cache.remember("chaat-relay", ID);
        assert_eq!(cache.get("chaat-relay"), Some(ID.to_owned()));
        // Another instance reads the same file.
        assert_eq!(
            IdCache::in_dir(&dir).get("chaat-relay"),
            Some(ID.to_owned())
        );
        cache.forget("chaat-relay");
        assert_eq!(cache.get("chaat-relay"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_damaged_file_or_entry_is_ignored() {
        let dir = scratch("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), b"not json").unwrap();
        let cache = IdCache::in_dir(&dir);
        assert_eq!(cache.get("t"), None);
        std::fs::write(dir.join(FILE), br#"{"t": "not-an-id"}"#).unwrap();
        assert_eq!(cache.get("t"), None);
        cache.remember("t", "not-an-id");
        assert_eq!(cache.get("t"), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
