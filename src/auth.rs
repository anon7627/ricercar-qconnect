//! Stored Qobuz account session (in the data directory the host gives the plugin).
//!
//! Only the user auth token is kept (never the password), in a file readable
//! by the owner alone.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Credentials {
    pub user_id: u64,
    pub email: String,
    pub display_name: String,
    /// Qobuz `user_auth_token`.
    pub token: String,
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("account.json")
}

impl Credentials {
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let path = path(data_dir);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mode = std::fs::metadata(&path)?.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!("{} is readable by other users; run `chmod 600` on it", path.display());
        }
        let creds = serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(creds))
    }

    pub fn save(&self, data_dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(data_dir).context("creating data dir")?;
        let path = path(data_dir);
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Returns whether something was deleted.
    pub fn delete(data_dir: &Path) -> Result<bool> {
        match std::fs::remove_file(path(data_dir)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_private_permissions() {
        let dir = std::env::temp_dir().join(format!("qconnect-auth-{}", uuid::Uuid::new_v4()));
        let c = Credentials { user_id: 42, email: "a@b.c".into(), display_name: "A".into(), token: "tok".into() };
        let path = c.save(&dir).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(Credentials::load(&dir).unwrap(), Some(c));
        assert!(Credentials::delete(&dir).unwrap());
        assert_eq!(Credentials::load(&dir).unwrap(), None);
        assert!(!Credentials::delete(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
