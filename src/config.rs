//! Streaming quality and the persisted Qobuz Connect device identity.

use std::path::Path;

use anyhow::{Context, Result};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

/// Qobuz streaming quality, ordered from lowest to highest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    Mp3,
    Lossless,
    HiRes96,
    HiRes192,
}

impl Quality {
    const ALL: [Quality; 4] = [Quality::HiRes192, Quality::HiRes96, Quality::Lossless, Quality::Mp3];

    /// Qobuz REST API `format_id`.
    pub fn format_id(self) -> i32 {
        match self {
            Quality::Mp3 => 5,
            Quality::Lossless => 6,
            Quality::HiRes96 => 7,
            Quality::HiRes192 => 27,
        }
    }

    pub fn from_format_id(id: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.format_id() == id)
    }

    /// QConnect protocol `audio_quality` value.
    pub fn protocol(self) -> i32 {
        match self {
            Quality::Mp3 => 1,
            Quality::Lossless => 2,
            Quality::HiRes96 => 3,
            Quality::HiRes192 => 4,
        }
    }

    pub fn from_protocol(value: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.protocol() == value)
    }

    /// This quality followed by every lower one, best first.
    pub fn fallback_chain(self) -> impl Iterator<Item = Quality> {
        Self::ALL.into_iter().filter(move |q| *q <= self)
    }
}

/// Persisted device identity.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceIdentity {
    pub uuid: String,
}

impl DeviceIdentity {
    /// Load identity from disk, or create (and persist) a new one.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("device.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path).context("reading device.json")?;
            let id: DeviceIdentity = serde_json::from_str(&raw).context("parsing device.json")?;
            return Ok(id);
        }
        let id = Self { uuid: uuid::Uuid::new_v4().to_string() };
        std::fs::create_dir_all(data_dir).context("creating data dir")?;
        std::fs::write(&path, serde_json::to_string_pretty(&id)?).context("writing device.json")?;
        Ok(id)
    }

    /// Parse the UUID string into 16 raw bytes.
    pub fn uuid_bytes(&self) -> [u8; 16] {
        uuid::Uuid::parse_str(&self.uuid)
            .map(|u| u.into_bytes())
            // Fallback for a hand-edited, non-UUID id: md5 of the string.
            .unwrap_or_else(|_| Md5::digest(self.uuid.as_bytes()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_chain_starts_at_requested_quality() {
        let chain: Vec<_> = Quality::HiRes96.fallback_chain().collect();
        assert_eq!(chain, [Quality::HiRes96, Quality::Lossless, Quality::Mp3]);
    }

    #[test]
    fn protocol_and_format_ids_round_trip() {
        for q in Quality::ALL {
            assert_eq!(Quality::from_format_id(q.format_id()), Some(q));
            assert_eq!(Quality::from_protocol(q.protocol()), Some(q));
        }
    }

    #[test]
    fn non_uuid_override_hashes_to_stable_bytes() {
        let a = DeviceIdentity { uuid: "living-room".into() };
        assert_eq!(a.uuid_bytes(), a.clone().uuid_bytes());
        assert_ne!(a.uuid_bytes(), [0u8; 16]);
    }
}
