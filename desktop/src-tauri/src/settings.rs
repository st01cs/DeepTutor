//! Shell-owned preferences, and the bootstrap pointer that can move the data
//! directory.
//!
//! Two files live under `<home>/desktop/`:
//!
//! * `shell.json` — user preferences the desktop shell owns (close to tray,
//!   notifications, the first-run answers, the runtime-pack catalog URL). The
//!   web app's own settings stay in `data/user/settings/*.json`, exactly where
//!   Web/CLI mode keeps them; nothing here duplicates them.
//! * `bootstrap.json` — a one-line pointer to a non-default data directory,
//!   written by the first-run wizard. It has to live in the *platform default*
//!   home, because that is the only location the shell can find before it knows
//!   where the data lives.
//!
//! Both are written atomically: a crash mid-write must leave the previous file
//! readable, otherwise the next launch would fall back to defaults and silently
//! forget the user's choices.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bumped only when the meaning of a field changes; a mismatch resets to
/// defaults instead of guessing.
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
pub const BOOTSTRAP_SCHEMA_VERSION: u32 = 1;

/// Desktop-shell preferences. Every field is `#[serde(default)]` so a file
/// written by an older shell still loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellSettings {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    /// Closing the window hides it instead of quitting the app.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    /// Post a system notification when a round finishes in the background.
    #[serde(default = "default_true")]
    pub notifications: bool,
    /// Preferred UI language; empty means "whatever the web app already uses".
    #[serde(default)]
    pub locale: String,
    /// Set once the first-run wizard has been answered (or skipped).
    #[serde(default)]
    pub first_run_completed: bool,
    /// Runtime-pack catalog the "check updates" action reads.
    #[serde(default)]
    pub pack_catalog: Option<String>,
}

fn schema_version() -> u32 {
    SETTINGS_SCHEMA_VERSION
}

fn default_true() -> bool {
    true
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            close_to_tray: true,
            notifications: true,
            locale: String::new(),
            first_run_completed: false,
            pack_catalog: None,
        }
    }
}

impl ShellSettings {
    pub fn path(home: &Path) -> PathBuf {
        home.join("desktop").join("shell.json")
    }

    /// Read preferences, falling back to defaults for anything unreadable.
    ///
    /// A corrupt or future-versioned file is *not* an error the user should
    /// have to clear by hand: the shell logs it and keeps working with defaults.
    pub fn load(home: &Path) -> Self {
        let path = Self::path(home);
        let Ok(text) = fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str::<ShellSettings>(&text) {
            Ok(settings) if settings.schema_version == SETTINGS_SCHEMA_VERSION => settings,
            Ok(settings) => {
                eprintln!(
                    "shell settings schema {} unsupported (expected {}); using defaults",
                    settings.schema_version, SETTINGS_SCHEMA_VERSION
                );
                Self::default()
            }
            Err(error) => {
                eprintln!("shell settings unreadable ({error}); using defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self, home: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        atomic_write(&Self::path(home), &format!("{text}\n"))
    }

    /// True when the shell still has to ask the first-run questions.
    pub fn needs_first_run(&self) -> bool {
        !self.first_run_completed
    }
}

/// Pointer to a non-default data directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bootstrap {
    #[serde(default = "bootstrap_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub home: Option<String>,
}

fn bootstrap_schema_version() -> u32 {
    BOOTSTRAP_SCHEMA_VERSION
}

impl Bootstrap {
    /// Always resolved against the *platform default* home, never the effective
    /// one — otherwise a broken bootstrap would compound itself.
    pub fn path(default_home: &Path) -> PathBuf {
        default_home.join("desktop").join("bootstrap.json")
    }

    /// The configured data directory, when it is usable.
    ///
    /// A pointer at a directory that has since been deleted or unmounted is
    /// ignored, so the app falls back to the default home instead of creating a
    /// brand-new empty profile on a missing volume.
    pub fn read_home(default_home: &Path) -> Option<PathBuf> {
        let text = fs::read_to_string(Self::path(default_home)).ok()?;
        let bootstrap: Bootstrap = serde_json::from_str(&text).ok()?;
        if bootstrap.schema_version != BOOTSTRAP_SCHEMA_VERSION {
            return None;
        }
        let home = bootstrap.home?.trim().to_string();
        if home.is_empty() {
            return None;
        }
        let path = PathBuf::from(home);
        if path.is_dir() {
            Some(path)
        } else {
            None
        }
    }

    pub fn write_home(default_home: &Path, target: &Path) -> Result<(), String> {
        let bootstrap = Bootstrap {
            schema_version: BOOTSTRAP_SCHEMA_VERSION,
            home: Some(target.to_string_lossy().into_owned()),
        };
        let text = serde_json::to_string_pretty(&bootstrap).map_err(|error| error.to_string())?;
        atomic_write(&Self::path(default_home), &format!("{text}\n"))
    }

    /// Drop the pointer so the next launch uses the platform default again.
    pub fn clear(default_home: &Path) -> Result<(), String> {
        let path = Self::path(default_home);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("无法删除 {}: {error}", path.display())),
        }
    }
}

/// Write `contents` to `path` through a sibling temporary file.
///
/// The rename is what makes this safe: a reader either sees the old file or the
/// new one, never a truncated `{`.
pub fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建 {}: {error}", parent.display()))?;
    }
    let temporary = path.with_extension("json.tmp");
    {
        use std::io::Write as _;
        let mut handle = fs::File::create(&temporary)
            .map_err(|error| format!("无法写入 {}: {error}", temporary.display()))?;
        handle
            .write_all(contents.as_bytes())
            .map_err(|error| format!("无法写入 {}: {error}", temporary.display()))?;
        handle
            .sync_all()
            .map_err(|error| format!("无法同步 {}: {error}", temporary.display()))?;
    }
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("无法替换 {}: {error}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "deeptutor-shell-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("temp home");
        home
    }

    #[test]
    fn missing_file_yields_defaults_and_asks_for_first_run() {
        let home = temp_home("missing");
        let settings = ShellSettings::load(&home);
        assert_eq!(settings, ShellSettings::default());
        assert!(settings.close_to_tray);
        assert!(settings.notifications);
        assert!(settings.needs_first_run());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let home = temp_home("roundtrip");
        let settings = ShellSettings {
            close_to_tray: false,
            notifications: false,
            locale: "en".to_string(),
            first_run_completed: true,
            pack_catalog: Some("https://example.test/runtime-packs.json".to_string()),
            ..ShellSettings::default()
        };
        settings.save(&home).expect("save");
        assert_eq!(ShellSettings::load(&home), settings);
        assert!(!ShellSettings::load(&home).needs_first_run());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn partial_json_keeps_defaults_for_absent_fields() {
        let home = temp_home("partial");
        let path = ShellSettings::path(&home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"close_to_tray\": false}\n").unwrap();
        let settings = ShellSettings::load(&home);
        assert!(!settings.close_to_tray);
        // Absent booleans default to *on*, not to `false`.
        assert!(settings.notifications);
        assert_eq!(settings.locale, "");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn corrupt_or_future_file_falls_back_to_defaults() {
        let home = temp_home("corrupt");
        let path = ShellSettings::path(&home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(ShellSettings::load(&home), ShellSettings::default());
        fs::write(&path, "{\"schema_version\": 99}").unwrap();
        assert_eq!(ShellSettings::load(&home), ShellSettings::default());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn atomic_write_leaves_no_temporary_behind() {
        let home = temp_home("atomic");
        let path = home.join("desktop").join("shell.json");
        atomic_write(&path, "{\"schema_version\": 1}").expect("write");
        assert!(path.is_file());
        assert!(!path.with_extension("json.tmp").exists());
        atomic_write(&path, "{\"schema_version\": 1, \"locale\": \"zh-CN\"}").expect("rewrite");
        assert!(fs::read_to_string(&path).unwrap().contains("zh-CN"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn bootstrap_points_at_an_existing_directory_only() {
        let default_home = temp_home("bootstrap");
        let target = default_home.join("elsewhere");
        // Nothing written yet.
        assert_eq!(Bootstrap::read_home(&default_home), None);
        // A pointer at a directory that does not exist is ignored rather than
        // silently creating a second profile.
        Bootstrap::write_home(&default_home, &target).expect("write");
        assert_eq!(Bootstrap::read_home(&default_home), None);
        fs::create_dir_all(&target).unwrap();
        assert_eq!(Bootstrap::read_home(&default_home), Some(target.clone()));
        Bootstrap::clear(&default_home).expect("clear");
        assert_eq!(Bootstrap::read_home(&default_home), None);
        // Clearing twice is fine.
        Bootstrap::clear(&default_home).expect("clear again");
        let _ = fs::remove_dir_all(&default_home);
    }
}
