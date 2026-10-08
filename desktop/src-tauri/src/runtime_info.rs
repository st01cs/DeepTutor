//! Reader for the launcher's `--runtime-info` state file.
//!
//! The contract lives on the Python side (`deeptutor/runtime/launcher.py`,
//! `RuntimeInfoWriter`); this module only mirrors it. `schema_version` is
//! checked by the supervisor so a drifted payload fails loudly instead of
//! silently navigating the window to a stale port.

use std::fs;
use std::path::Path;

use serde::Deserialize;

/// Mirrors `launcher.RUNTIME_INFO_SCHEMA_VERSION`.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeInfo {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub frontend_url: Option<String>,
    #[serde(default)]
    pub backend_url: Option<String>,
    #[serde(default)]
    pub backend_port: Option<u16>,
    #[serde(default)]
    pub frontend_port: Option<u16>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub pid: Option<i32>,
}

impl RuntimeInfo {
    /// Best-effort read: a half-written or absent file simply means "not yet".
    pub fn read(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }
}
