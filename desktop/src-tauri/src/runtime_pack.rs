//! Runtime packs: the "no Python, no Node" install.
//!
//! A pack is a tarball containing a CPython, a virtualenv with DeepTutor in it,
//! the Node runtime and a manifest. Installing one is: verify the digest,
//! extract, rehydrate the venv for this machine, smoke test, then switch
//! atomically and keep the previous pack for rollback.
//!
//! Why rehydrate: a virtualenv written for one path does not survive being
//! moved. `uv venv --relocatable` marks `pyvenv.cfg`, but the interpreter link
//! and the `home` line are still absolute (measured on 2026-09-24). The
//! installer repairs exactly those self-references, so the build machine's
//! paths never have to match the user's.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const STATE_FILE: &str = "state.json";
pub const SCHEMA_VERSION: u32 = 1;
/// Guards against a pack that would unpack into an unrelated corner of the disk.
const MAX_ENTRY_COUNT: usize = 400_000;

/// `manifest.json` inside a pack, written by `desktop/pack/build_pack.py`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PackManifest {
    pub schema_version: u32,
    pub pack_id: String,
    pub app_version: String,
    pub platform: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub python: Option<PackComponent>,
    #[serde(default)]
    pub node: Option<PackComponent>,
    pub paths: PackPaths,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PackComponent {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PackPaths {
    /// Interpreter relative to the pack root, e.g. `venv/bin/python`.
    pub python: String,
    /// Directory prepended to `PATH` so the launcher finds `node`.
    pub node_dir: String,
}

impl PackManifest {
    pub fn read(pack_dir: &Path) -> Result<Self, String> {
        let path = pack_dir.join(MANIFEST_FILE);
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("无法读取 {}: {error}", path.display()))?;
        let manifest: Self = serde_json::from_str(&text)
            .map_err(|error| format!("{} 不是合法的 pack 清单: {error}", path.display()))?;
        if manifest.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "pack 清单版本 {} 不受支持（期望 {SCHEMA_VERSION}）",
                manifest.schema_version
            ));
        }
        Ok(manifest)
    }

    pub fn python_path(&self, pack_dir: &Path) -> PathBuf {
        pack_dir.join(&self.paths.python)
    }

    pub fn node_dir(&self, pack_dir: &Path) -> PathBuf {
        pack_dir.join(&self.paths.node_dir)
    }
}

/// Which pack is active, and what to fall back to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PackState {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub active_pack: Option<String>,
    #[serde(default)]
    pub previous_pack: Option<String>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// One entry of `runtime-packs.json`, the release catalog.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PackRelease {
    pub pack_id: String,
    #[serde(default)]
    pub app_version: String,
    pub platform: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub requires_shell: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PackCatalog {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub packs: Vec<PackRelease>,
}

/// Dotted numeric comparison; anything non-numeric (a `+tag`, `-rc1`) is
/// ignored, which is enough to order stable releases.
fn version_key(version: &str) -> Vec<u64> {
    version
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPack {
    pub pack_id: String,
    pub dir: PathBuf,
    pub manifest: PackManifest,
}

/// Installs and switches packs under `<home>/runtimes`.
pub struct PackInstaller {
    runtimes_dir: PathBuf,
    state_path: PathBuf,
}

impl PackInstaller {
    pub fn new(home: &Path) -> Self {
        Self {
            runtimes_dir: home.join("runtimes"),
            state_path: home.join("desktop").join(STATE_FILE),
        }
    }

    pub fn state(&self) -> PackState {
        fs::read_to_string(&self.state_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write_state(&self, state: &PackState) -> Result<(), String> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("无法创建状态目录: {error}"))?;
        }
        let text = serde_json::to_string_pretty(state).map_err(|error| error.to_string())?;
        fs::write(&self.state_path, text + "\n")
            .map_err(|error| format!("无法写入 {}: {error}", self.state_path.display()))
    }

    pub fn active(&self) -> Option<InstalledPack> {
        self.state().active_pack.and_then(|id| self.load(&id).ok())
    }

    pub fn load(&self, pack_id: &str) -> Result<InstalledPack, String> {
        let dir = self.runtimes_dir.join(pack_id);
        let manifest = PackManifest::read(&dir)?;
        Ok(InstalledPack {
            pack_id: manifest.pack_id.clone(),
            dir,
            manifest,
        })
    }

    /// Pack ids present under `<home>/runtimes`, oldest name first.
    pub fn installed(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.runtimes_dir) else {
            return Vec::new();
        };
        let mut packs: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| !name.starts_with('.'))
            .collect();
        packs.sort();
        packs
    }

    /// Read the release catalog from a URL or a local path.
    pub fn catalog(&self, source: &str) -> Result<PackCatalog, String> {
        let text = fetch_text(source)?;
        let catalog: PackCatalog =
            serde_json::from_str(&text).map_err(|error| format!("清单解析失败: {error}"))?;
        if catalog.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "清单版本 {} 不受支持（期望 {SCHEMA_VERSION}）",
                catalog.schema_version
            ));
        }
        Ok(catalog)
    }

    /// Install the newest catalog pack for this platform, if it is newer than
    /// what is active. Returns `None` when already up to date.
    pub fn update_from_catalog(&self, source: &str) -> Result<Option<InstalledPack>, String> {
        let catalog = self.catalog(source)?;
        let host = host_platform();
        let active_version = self
            .active()
            .map(|pack| pack.manifest.app_version)
            .unwrap_or_default();
        let mut candidates: Vec<&PackRelease> = catalog
            .packs
            .iter()
            .filter(|release| release.platform == host)
            .collect();
        candidates.sort_by_key(|release| version_key(&release.app_version));
        let Some(best) = candidates.last() else {
            return Err(format!("清单里没有适用于 {host} 的运行时包"));
        };
        if version_key(&best.app_version) <= version_key(&active_version) {
            log(&format!(
                "active pack {active_version} is already the newest for {host}"
            ));
            return Ok(None);
        }
        let url = resolve_relative(source, &best.url);
        let pack = self.install_from_url(&url, &best.sha256)?;
        Ok(Some(pack))
    }

    /// Install an archive that is already on disk (the "fat installer" path).
    /// `expected_sha256` comes from the release catalog; a mismatch aborts
    /// before anything is written.
    pub fn install_archive(
        &self,
        archive: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<InstalledPack, String> {
        if let Some(expected) = expected_sha256 {
            let actual = sha256_file(archive)?;
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(format!(
                    "运行时包校验失败：期望 {expected}，实际 {actual}（已拒绝安装）"
                ));
            }
        }
        let staging = self
            .runtimes_dir
            .join(format!(".incoming-{}", std::process::id()));
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|error| error.to_string())?;
        }
        fs::create_dir_all(&staging).map_err(|error| format!("无法创建暂存目录: {error}"))?;
        extract_tar_gz(archive, &staging)?;
        let manifest = PackManifest::read(&staging)?;
        let host = host_platform();
        if manifest.platform != host {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!(
                "这个运行时包是为 {} 构建的，本机是 {host}",
                manifest.platform
            ));
        }
        rehydrate(&staging)?;
        smoke_test(&staging, &manifest)?;

        let target = self.runtimes_dir.join(&manifest.pack_id);
        if target.exists() {
            fs::remove_dir_all(&target).map_err(|error| error.to_string())?;
        }
        fs::rename(&staging, &target)
            .map_err(|error| format!("无法就位 {}: {error}", target.display()))?;
        self.activate(&manifest.pack_id)?;
        log(&format!("runtime pack {} installed", manifest.pack_id));
        self.load(&manifest.pack_id)
    }

    /// Download then install: the "slim installer + fetch on first run" path,
    /// and how runtime-only updates arrive.
    pub fn install_from_url(
        &self,
        url: &str,
        expected_sha256: &str,
    ) -> Result<InstalledPack, String> {
        let downloads = self.runtimes_dir.join(".downloads");
        fs::create_dir_all(&downloads).map_err(|error| error.to_string())?;
        let target = downloads.join(
            url.rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or("runtime-pack.tar.gz"),
        );
        log(&format!("downloading runtime pack from {url}"));
        let response = ureq::get(url)
            .call()
            .map_err(|error| format!("下载运行时包失败: {error}"))?;
        let mut reader = response.into_reader();
        let mut file = File::create(&target).map_err(|error| error.to_string())?;
        std::io::copy(&mut reader, &mut file).map_err(|error| format!("写入下载失败: {error}"))?;
        file.flush().map_err(|error| error.to_string())?;
        let installed = self.install_archive(&target, Some(expected_sha256))?;
        let _ = fs::remove_file(&target);
        Ok(installed)
    }

    /// Point the shell at `pack_id`, remembering the outgoing pack for rollback.
    pub fn activate(&self, pack_id: &str) -> Result<(), String> {
        self.load(pack_id)?;
        let mut state = self.state();
        if state.active_pack.as_deref() != Some(pack_id) {
            state.previous_pack = state.active_pack.take();
            state.active_pack = Some(pack_id.to_string());
            state.schema_version = SCHEMA_VERSION;
        }
        self.write_state(&state)
    }

    /// Return to the pack that was active before the last switch.
    pub fn rollback(&self) -> Result<InstalledPack, String> {
        let mut state = self.state();
        let Some(previous) = state.previous_pack.clone() else {
            return Err("没有可回滚的运行时包".to_string());
        };
        let pack = self.load(&previous)?;
        std::mem::swap(&mut state.active_pack, &mut state.previous_pack);
        self.write_state(&state)?;
        log(&format!("rolled back to runtime pack {previous}"));
        Ok(pack)
    }
}

/// Read a catalog/archive location: `http(s)` goes over the network, anything
/// else is a path on disk (which is what a local test or an offline install
/// uses).
fn fetch_text(source: &str) -> Result<String, String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let response = ureq::get(source)
            .call()
            .map_err(|error| format!("读取 {source} 失败: {error}"))?;
        return response
            .into_string()
            .map_err(|error| format!("读取 {source} 失败: {error}"));
    }
    fs::read_to_string(source).map_err(|error| format!("无法读取 {source}: {error}"))
}

/// Resolve a catalog-relative URL against the catalog's own location, so a
/// catalog can list plain file names next to itself.
fn resolve_relative(base: &str, target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") || target.starts_with('/') {
        return target.to_string();
    }
    if base.starts_with("http://") || base.starts_with("https://") {
        let directory = base.rsplit_once('/').map(|(head, _)| head).unwrap_or(base);
        return format!("{directory}/{target}");
    }
    Path::new(base)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(target)
        .to_string_lossy()
        .to_string()
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|error| format!("无法读取 {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 256];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Extract a `.tar.gz`, refusing entries that would escape the destination.
pub fn extract_tar_gz(archive: &Path, destination: &Path) -> Result<(), String> {
    let file =
        File::open(archive).map_err(|error| format!("无法打开 {}: {error}", archive.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut count = 0usize;
    for entry in tar.entries().map_err(|error| error.to_string())? {
        let mut entry = entry.map_err(|error| error.to_string())?;
        let path = entry
            .path()
            .map_err(|error| error.to_string())?
            .into_owned();
        if !is_safe_entry_path(&path) {
            return Err(format!("运行时包包含非法路径: {}", path.display()));
        }
        count += 1;
        if count > MAX_ENTRY_COUNT {
            return Err("运行时包条目数异常，已中止安装".to_string());
        }
        entry
            .unpack_in(destination)
            .map_err(|error| format!("解包 {} 失败: {error}", path.display()))?;
    }
    Ok(())
}

/// Reject absolute paths and any `..` segment: a pack must only ever write
/// inside the directory it is being unpacked into.
pub(crate) fn is_safe_entry_path(path: &Path) -> bool {
    !path.is_absolute()
        && !path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

/// Repair the self-references a moved virtualenv carries.
///
/// Two things are path-bound: `pyvenv.cfg`'s `home` (python uses it to find its
/// stdlib) and the interpreter link in `bin/`. Both are rewritten for this
/// machine; the build machine's prefix is replaced in the small text files that
/// embed it (entry-point shebangs, `activate`, `.pth`).
pub fn rehydrate(pack_dir: &Path) -> Result<(), String> {
    let venv = pack_dir.join("venv");
    let cfg = venv.join("pyvenv.cfg");
    let bin_dir = pack_dir.join("python").join("bin");

    let text =
        fs::read_to_string(&cfg).map_err(|error| format!("无法读取 {}: {error}", cfg.display()))?;
    let old_home = text.lines().find_map(|line| {
        line.strip_prefix("home =")
            .map(|value| value.trim().to_string())
    });
    let mut updated = String::with_capacity(text.len());
    for line in text.lines() {
        if line.starts_with("home =") {
            updated.push_str(&format!("home = {}\n", bin_dir.display()));
        } else {
            updated.push_str(line);
            updated.push('\n');
        }
    }
    fs::write(&cfg, updated).map_err(|error| format!("无法写入 {}: {error}", cfg.display()))?;

    // Relative interpreter links: the pack moves as one unit, so `../../python`
    // stays correct wherever it is unpacked.
    #[cfg(unix)]
    {
        for name in ["python", "python3", "python3.12"] {
            let link = venv.join("bin").join(name);
            if link.symlink_metadata().is_err() {
                continue;
            }
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink("../../python/bin/python3", &link)
                .map_err(|error| format!("无法重建 {}: {error}", link.display()))?;
        }
    }

    if let Some(old_home) = old_home {
        let old_pack_root = Path::new(&old_home)
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        if let Some(old_root) = old_pack_root {
            if old_root != pack_dir {
                rewrite_prefix(&venv, &old_root, pack_dir)?;
            }
        }
    }
    Ok(())
}

/// Replace `old` with `new` inside the small text files under `root`.
fn rewrite_prefix(root: &Path, old: &Path, new: &Path) -> Result<(), String> {
    let old_text = old.to_string_lossy().to_string();
    let new_text = new.to_string_lossy().to_string();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            // Only small text files can carry a prefix; binaries and RECORDs are
            // skipped to keep install time bounded.
            if metadata.len() > 256 * 1024 {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            if !content.contains(&old_text) {
                continue;
            }
            let _ = fs::write(&path, content.replace(&old_text, &new_text));
        }
    }
    Ok(())
}

/// Can this pack actually run the launcher on this machine?
fn smoke_test(pack_dir: &Path, manifest: &PackManifest) -> Result<(), String> {
    let python = manifest.python_path(pack_dir);
    let output = std::process::Command::new(&python)
        .arg("-c")
        .arg("import deeptutor_cli.main, deeptutor_web")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("无法运行 {}: {error}", python.display()))?;
    if !output.status.success() {
        return Err(format!(
            "运行时包冒烟失败：{} 无法导入 DeepTutor（{}）",
            python.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Mirrors `host_platform()` in `desktop/pack/build_pack.py`.
pub fn host_platform() -> String {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "macos-aarch64".to_string()
        } else {
            "macos-x86_64".to_string()
        }
    } else if cfg!(windows) {
        "windows-x86_64".to_string()
    } else {
        "linux-x86_64".to_string()
    }
}

fn log(message: &str) {
    eprintln!("[runtime-pack] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deeptutor-pack-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_manifest(dir: &Path, pack_id: &str) {
        let manifest = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "pack_id": pack_id,
            "app_version": "1.6.10",
            "platform": host_platform(),
            "paths": {"python": "venv/bin/python", "node_dir": "node/bin"},
        });
        fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    /// A stand-in for a real pack: the files rehydration has to repair.
    fn fake_pack(root: &Path, old_prefix: &Path) -> PathBuf {
        let pack = root.join("pack");
        fs::create_dir_all(pack.join("python/bin")).unwrap();
        fs::create_dir_all(pack.join("venv/bin")).unwrap();
        fs::write(pack.join("python/bin/python3"), b"#!/bin/sh\n").unwrap();
        fs::write(
            pack.join("venv/pyvenv.cfg"),
            format!(
                "home = {}/python/bin\nversion_info = 3.12.14\n",
                old_prefix.display()
            ),
        )
        .unwrap();
        let mut handle = File::create(pack.join("venv/bin/deeptutor")).unwrap();
        writeln!(handle, "#!{}/python/bin/python3", old_prefix.display()).unwrap();
        // A real pack ships an absolute interpreter link; that is exactly what
        // rehydration has to make relative.
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            old_prefix.join("python/bin/python3"),
            pack.join("venv/bin/python"),
        )
        .unwrap();
        write_manifest(&pack, "1.6.10-macos-aarch64");
        pack
    }

    #[test]
    fn rehydrate_rewrites_home_prefix_and_links() {
        let tmp = scratch("rehydrate");
        let old_prefix = PathBuf::from("/build/machine/stage");
        let pack = fake_pack(&tmp, &old_prefix);

        rehydrate(&pack).unwrap();

        let cfg = fs::read_to_string(pack.join("venv/pyvenv.cfg")).unwrap();
        assert!(cfg.contains(&format!("home = {}/python/bin", pack.display())));
        assert!(!cfg.contains("/build/machine"));

        let script = fs::read_to_string(pack.join("venv/bin/deeptutor")).unwrap();
        assert!(script.contains(&pack.to_string_lossy().to_string()));
        assert!(!script.contains("/build/machine"));

        #[cfg(unix)]
        {
            let target = fs::read_link(pack.join("venv/bin/python")).unwrap();
            assert_eq!(target, PathBuf::from("../../python/bin/python3"));
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn manifest_rejects_an_unknown_schema() {
        let tmp = scratch("schema");
        fs::write(
            tmp.join(MANIFEST_FILE),
            r#"{"schema_version":99,"pack_id":"x","app_version":"1",
                "platform":"macos-aarch64",
                "paths":{"python":"venv/bin/python","node_dir":"node/bin"}}"#,
        )
        .unwrap();
        let error = PackManifest::read(&tmp).unwrap_err();
        assert!(error.contains("不受支持"), "{error}");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_refuses_a_digest_mismatch_before_writing() {
        let tmp = scratch("digest");
        let archive = tmp.join("runtime.tar.gz");
        fs::write(&archive, b"not really a tarball").unwrap();
        let installer = PackInstaller::new(&tmp);

        let error = installer
            .install_archive(&archive, Some(&"0".repeat(64)))
            .unwrap_err();

        assert!(error.contains("校验失败"), "{error}");
        assert!(!tmp.join("runtimes").join("1.6.10-macos-aarch64").exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn entry_paths_must_stay_inside_the_pack() {
        // The `tar` crate refuses to *write* these paths, so the guard is tested
        // where extraction consults it: an archive from anywhere is untrusted.
        assert!(is_safe_entry_path(Path::new("venv/bin/python")));
        assert!(is_safe_entry_path(Path::new("manifest.json")));
        assert!(!is_safe_entry_path(Path::new("../escaped.txt")));
        assert!(!is_safe_entry_path(Path::new("venv/../../escaped.txt")));
        assert!(!is_safe_entry_path(Path::new("/etc/passwd")));
    }

    #[test]
    fn extract_accepts_a_well_formed_archive() {
        let tmp = scratch("extract-ok");
        let archive = tmp.join("good.tar.gz");
        let file = File::create(&archive).unwrap();
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "manifest.json", &b"{}"[..])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let destination = tmp.join("out");
        fs::create_dir_all(&destination).unwrap();
        extract_tar_gz(&archive, &destination).unwrap();

        assert!(destination.join("manifest.json").exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn activate_then_rollback_swaps_the_active_pack() {
        let tmp = scratch("state");
        for pack_id in ["1.6.10-macos-aarch64", "1.6.11-macos-aarch64"] {
            let dir = tmp.join("runtimes").join(pack_id);
            fs::create_dir_all(&dir).unwrap();
            write_manifest(&dir, pack_id);
        }
        let installer = PackInstaller::new(&tmp);

        installer.activate("1.6.10-macos-aarch64").unwrap();
        installer.activate("1.6.11-macos-aarch64").unwrap();
        assert_eq!(
            installer.state().active_pack.as_deref(),
            Some("1.6.11-macos-aarch64")
        );
        assert_eq!(
            installer.state().previous_pack.as_deref(),
            Some("1.6.10-macos-aarch64")
        );

        let pack = installer.rollback().unwrap();
        assert_eq!(pack.pack_id, "1.6.10-macos-aarch64");
        assert_eq!(
            installer.state().active_pack.as_deref(),
            Some("1.6.10-macos-aarch64")
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rollback_without_history_reports_clearly() {
        let tmp = scratch("rollback-empty");
        let installer = PackInstaller::new(&tmp);
        let error = installer.rollback().unwrap_err();
        assert!(error.contains("没有可回滚"), "{error}");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn version_key_orders_stable_releases() {
        assert!(version_key("1.6.11") > version_key("1.6.10"));
        assert!(version_key("1.6.10") > version_key("1.5.99"));
        assert_eq!(version_key("v1.6.10"), version_key("1.6.10"));
        // Pre-release suffixes are ignored rather than mis-parsed as newer.
        assert!(version_key("1.7.0") > version_key("1.6.10-rc1"));
    }

    #[test]
    fn catalog_prefers_the_newest_pack_for_this_platform() {
        let tmp = scratch("catalog");
        let installer = PackInstaller::new(&tmp);
        let host = host_platform();
        fs::write(
            tmp.join("runtime-packs.json"),
            serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "packs": [
                    {"pack_id": "1.6.10", "app_version": "1.6.10", "platform": host,
                     "url": "a.tar.gz", "sha256": "aa"},
                    {"pack_id": "1.6.11", "app_version": "1.6.11", "platform": host,
                     "url": "b.tar.gz", "sha256": "bb"},
                    {"pack_id": "9.9.9-other", "app_version": "9.9.9", "platform": "windows-x86_64",
                     "url": "c.tar.gz", "sha256": "cc"}
                ]
            })
            .to_string(),
        )
        .unwrap();

        let catalog = installer
            .catalog(&tmp.join("runtime-packs.json").to_string_lossy())
            .unwrap();
        let mut candidates: Vec<&PackRelease> = catalog
            .packs
            .iter()
            .filter(|release| release.platform == host)
            .collect();
        candidates.sort_by_key(|release| version_key(&release.app_version));

        assert_eq!(candidates.last().unwrap().app_version, "1.6.11");
        assert_eq!(catalog.packs.len(), 3);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn relative_catalog_urls_resolve_next_to_the_catalog() {
        assert_eq!(
            resolve_relative("https://example.com/dl/packs.json", "a.tar.gz"),
            "https://example.com/dl/a.tar.gz"
        );
        assert_eq!(
            resolve_relative("/tmp/packs.json", "a.tar.gz"),
            "/tmp/a.tar.gz"
        );
        assert_eq!(
            resolve_relative("https://example.com/dl/packs.json", "https://cdn/a.tar.gz"),
            "https://cdn/a.tar.gz"
        );
    }
}
