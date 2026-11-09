use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "config.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server_url: Option<String>,
    pub login_name: Option<String>,
    pub user_id: Option<String>,
    pub local_root: Option<PathBuf>,
    pub paused: bool,
}

impl Config {
    /// Loads `config.json` from `dir`. A missing or unreadable file gives the default config.
    pub fn load(dir: &Path) -> Config {
        let path = dir.join(FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::error!("bad config {}: {e}", path.display());
                Config::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                tracing::error!("cannot read config {}: {e}", path.display());
                Config::default()
            }
        }
    }

    /// Atomically writes `config.json` (temp file + rename).
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, dir.join(FILE))?;
        Ok(())
    }

    pub fn is_logged_in(&self) -> bool {
        self.server_url.is_some() && self.login_name.is_some() && self.user_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("ncsync-cfg-{}", uuid::Uuid::new_v4()));
        assert!(Config::load(&dir).server_url.is_none());
        let cfg = Config {
            server_url: Some("https://x".into()),
            paused: true,
            ..Default::default()
        };
        cfg.save(&dir).unwrap();
        let back = Config::load(&dir);
        assert_eq!(back.server_url.as_deref(), Some("https://x"));
        assert!(back.paused);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
