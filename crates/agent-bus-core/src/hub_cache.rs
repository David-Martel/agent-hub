//! Last-good hub resolution cache shared by CLI invocations (agent-hub#79).
//!
//! Off the fleet LAN every dead candidate costs a connect timeout on EVERY
//! CLI command, because each invocation is a fresh process with nothing to
//! remember the previous answer. This module persists the last candidate that
//! answered `/health` in a small state file under the platform cache
//! directory, trusted for a short TTL.
//!
//! The file holds only the candidate URL, its role, a fingerprint of the
//! configured candidate list and a timestamp. It never holds a token, and
//! every failure (unwritable directory, corrupt file, concurrent writer) is
//! non-fatal: the cache is an optimisation, never a dependency.

use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::hub_candidates::{
    CandidateAuth, CredentialEnv, HubCandidate, HubRole, validate_hub_base_url,
};

const CACHE_VERSION: u32 = 1;
const MAX_CACHE_BYTES: u64 = 131_072;
const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheEntry {
    version: u32,
    fingerprint: String,
    url: String,
    role: HubRole,
    resolved_at_unix_ms: u64,
}

/// Default time-to-live of a cached resolution, in seconds.
pub const DEFAULT_TTL_SECONDS: u64 = 60;

/// A cached resolution that is still fresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedHub {
    /// The candidate URL that answered last time.
    pub url: String,
    /// The role it had.
    pub role: HubRole,
    /// How old the entry is.
    pub age: Duration,
}

/// The operating-system family that decides the default cache directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOs {
    /// `%LOCALAPPDATA%`.
    Windows,
    /// `~/Library/Caches`.
    MacOs,
    /// `$XDG_CACHE_HOME` or `~/.cache`.
    Unix,
}

impl CacheOs {
    /// The family this binary was compiled for.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Unix
        }
    }
}

/// A file-backed last-good resolution cache with a TTL. A zero TTL disables
/// it entirely.
#[derive(Debug, Clone)]
pub struct HubCache {
    path: PathBuf,
    ttl: Duration,
    #[cfg(test)]
    explicit_config: Option<ExplicitConfigPath>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct ExplicitConfigPath(Option<PathBuf>);

impl HubCache {
    /// A cache stored at `path`, trusting entries for `ttl`.
    #[must_use]
    pub const fn new(path: PathBuf, ttl: Duration) -> Self {
        Self {
            path,
            ttl,
            #[cfg(test)]
            explicit_config: None,
        }
    }

    #[cfg(test)]
    const fn with_config_path(path: PathBuf, ttl: Duration, config: Option<PathBuf>) -> Self {
        Self {
            path,
            ttl,
            explicit_config: Some(ExplicitConfigPath(config)),
        }
    }

    fn ensure_safe_mutation_path(&self) -> io::Result<()> {
        // Inject only external path selection for the filesystem controls.
        // The mutation guard and actual writer remain unchanged.
        #[cfg(test)]
        let config = match &self.explicit_config {
            Some(config) => config.0.clone(),
            None => crate::settings::config_file_path(),
        };
        #[cfg(not(test))]
        let config = crate::settings::config_file_path();
        if let Some(config) = config
            && paths_alias(&self.path, &config)?
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "hub cache path aliases credential configuration",
            ));
        }
        Ok(())
    }

    /// `false` when the TTL is zero.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    /// The cached resolution for `fingerprint`, if one exists, matches, and
    /// is younger than the TTL as of `now`. Any unreadable, corrupt,
    /// wrong-version, mismatched or future-dated entry is a miss.
    #[must_use]
    pub fn load(&self, fingerprint: &str, now: SystemTime) -> Option<CachedHub> {
        if !self.enabled() {
            return None;
        }
        let file = std::fs::File::open(&self.path).ok()?;
        let mut bytes = Vec::new();
        file.take(MAX_CACHE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_CACHE_BYTES {
            return None;
        }
        let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
        if entry.version != CACHE_VERSION
            || entry.fingerprint != fingerprint
            || validate_hub_base_url(&entry.url).is_err()
        {
            return None;
        }
        let resolved =
            SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(entry.resolved_at_unix_ms))?;
        let age = now.duration_since(resolved).ok()?;
        if age >= self.ttl {
            return None;
        }
        Some(CachedHub {
            url: entry.url,
            role: entry.role,
            age,
        })
    }

    /// Record `url` as the last good hub for `fingerprint`. A no-op when the
    /// cache is disabled. Writes atomically (temp file, then rename).
    ///
    /// # Errors
    /// The underlying I/O error; callers treat it as non-fatal.
    pub fn store(
        &self,
        fingerprint: &str,
        url: &str,
        role: HubRole,
        now: SystemTime,
    ) -> io::Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        self.ensure_safe_mutation_path()?;
        validate_hub_base_url(url)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let elapsed = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_error| {
                io::Error::new(io::ErrorKind::InvalidInput, "cache time precedes epoch")
            })?;
        let timestamp = u64::try_from(elapsed.as_millis()).map_err(|_error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache time exceeds supported range",
            )
        })?;
        let leaf = self
            .path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache needs a filename"))?;
        if cfg!(windows) && reserved_windows_leaf(&leaf.to_string_lossy()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reserved cache filename",
            ));
        }
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".hub-resolution-{}.tmp", uuid::Uuid::new_v4()));
        let entry = CacheEntry {
            version: CACHE_VERSION,
            fingerprint: fingerprint.to_owned(),
            url: url.to_owned(),
            role,
            resolved_at_unix_ms: timestamp,
        };
        let bytes = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let cleanup = TemporaryCacheFile(temporary.clone());
        let write_result = file.write_all(&bytes).and_then(|()| file.sync_all());
        drop(file);
        write_result?;
        std::fs::rename(&temporary, &self.path)?;
        drop(cleanup);
        Ok(())
    }

    /// Delete the cache file. Absence is not an error.
    pub fn invalidate(&self) {
        if !self.enabled() {
            return;
        }
        if let Err(error) = self.ensure_safe_mutation_path() {
            tracing::debug!(kind = ?error.kind(), "hub cache invalidation refused");
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(kind = ?error.kind(), "hub cache invalidation failed");
        }
    }
}

// Canonicalize the existing ancestor before normalizing the missing suffix.
// This preserves symlink/junction semantics instead of blindly collapsing
// `link/..`. Unresolvable I/O errors refuse mutation rather than trusting a
// string comparison. Cooperative ownership is still required for path swaps.
fn mutation_path_identity(path: &Path) -> io::Result<PathBuf> {
    let mut ancestor = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&ancestor) {
            Ok(mut canonical) => {
                if !suffix.is_empty() && !std::fs::metadata(&canonical)?.is_dir() {
                    return Err(io::Error::from(io::ErrorKind::NotADirectory));
                }
                for component in suffix.into_iter().rev() {
                    if component == std::ffi::OsStr::new("..") {
                        canonical.pop();
                    } else if component != std::ffi::OsStr::new(".") {
                        canonical.push(component);
                    }
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A dangling link exists even though its target cannot be
                // canonicalized. Treating it as a missing ordinary suffix
                // could permit creation of its target as a cache file.
                match std::fs::symlink_metadata(&ancestor) {
                    Ok(metadata) if is_reparse_path(&metadata) => {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "unresolved symlink or reparse cache/configuration path",
                        ));
                    }
                    Ok(_) => return Err(error),
                    Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {}
                    Err(metadata_error) => return Err(metadata_error),
                }
                let Some(component) = ancestor.components().next_back() else {
                    return Err(error);
                };
                if matches!(component, Component::Prefix(_) | Component::RootDir) {
                    return Err(error);
                }
                suffix.push(component.as_os_str().to_os_string());
                if !ancestor.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_reparse_path(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        // FILE_ATTRIBUTE_REPARSE_POINT also covers junctions and other tags,
        // not only the symbolic-link tag recognized by FileType.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn paths_alias(cache: &Path, config: &Path) -> io::Result<bool> {
    let cache = mutation_path_identity(cache)?;
    let config = mutation_path_identity(config)?;
    #[cfg(windows)]
    {
        // The supported config/cache leaves and environment routing contract
        // use ASCII names; existing filesystem aliases are canonicalized above.
        Ok(cache
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&config.as_os_str().to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        Ok(cache == config)
    }
}

/// A stable fingerprint of the configured candidate list (URLs, roles,
/// credential SOURCES, sites and hub names, in order). A change to any of them invalidates the
/// cache. It is a hash, and it never covers a token value.
#[must_use]
pub fn candidates_fingerprint(candidates: &[HubCandidate]) -> String {
    fn field(hash: &mut Sha256, bytes: &[u8]) {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    let mut hash = Sha256::new();
    field(&mut hash, b"agent-bus-hub-candidates-v1");
    for candidate in candidates {
        field(&mut hash, candidate.url.as_bytes());
        field(&mut hash, candidate.role.as_str().as_bytes());
        match &candidate.auth {
            CandidateAuth::Global => field(&mut hash, b"global"),
            CandidateAuth::TokenFile(path) => {
                field(&mut hash, b"file");
                field(&mut hash, path.as_bytes());
            }
            CandidateAuth::TokenEnv(name) => {
                field(&mut hash, b"env");
                field(&mut hash, name.as_bytes());
            }
        }
        field(&mut hash, b"sites");
        for site in &candidate.sites {
            field(&mut hash, site.as_bytes());
        }
        field(
            &mut hash,
            candidate.hub.as_deref().unwrap_or_default().as_bytes(),
        );
    }
    hash.finalize()
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect()
}

/// The default cache file location: `AGENT_BUS_HUB_CACHE_FILE` when set,
/// else `<platform cache dir>/agent-bus/hub-resolution.json`.
#[must_use]
pub fn default_cache_path(os: CacheOs, env: &dyn CredentialEnv) -> Option<PathBuf> {
    let nonblank = |name: &str| {
        env.var(name)
            .filter(|value| !value.trim().is_empty())
            .map(|value| PathBuf::from(value.trim()))
    };
    if let Some(path) = nonblank("AGENT_BUS_HUB_CACHE_FILE") {
        return Some(path);
    }
    let root = match os {
        CacheOs::Windows => nonblank("LOCALAPPDATA").or_else(|| {
            env.home_dir()
                .map(|home| home.join("AppData").join("Local"))
        }),
        CacheOs::MacOs => env
            .home_dir()
            .map(|home| home.join("Library").join("Caches")),
        CacheOs::Unix => nonblank("XDG_CACHE_HOME")
            // Interpret XDG paths with Unix semantics even when callers are
            // inspecting another platform's configuration from Windows.
            .filter(|path| path.as_os_str().to_string_lossy().starts_with('/'))
            .or_else(|| env.home_dir().map(|home| home.join(".cache"))),
    }?;
    Some(root.join("agent-bus").join("hub-resolution.json"))
}

fn reserved_windows_leaf(leaf: &str) -> bool {
    let trimmed = leaf.trim_end_matches(['.', ' ']);
    let stem = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(stem.as_str(), "$NULL" | "AUX" | "CON" | "NUL" | "PRN")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

#[derive(Debug)]
struct TemporaryCacheFile(PathBuf);

impl Drop for TemporaryCacheFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(kind = ?error.kind(), "hub cache temporary cleanup failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const TTL: Duration = Duration::from_secs(60);

    #[test]
    fn invalid_store_inputs_do_not_create_cache_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        for url in [
            "http://user:secret@hub.lan",
            "http://hub.lan/?secret=1",
            "file:///secret",
        ] {
            assert!(cache.store("fp", url, HubRole::Fallback, t0()).is_err());
            assert!(!cache.path.parent().expect("parent").exists());
        }
        assert!(
            cache
                .store(
                    "fp",
                    "http://hub.lan",
                    HubRole::Fallback,
                    SystemTime::UNIX_EPOCH - Duration::from_secs(1)
                )
                .is_err()
        );
        assert!(!cache.path.parent().expect("parent").exists());
    }

    #[test]
    fn failed_atomic_replacement_preserves_destination_and_removes_owned_temporary_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        std::fs::create_dir_all(&cache.path).expect("directory destination");
        let marker = cache.path.join("preserved");
        std::fs::write(&marker, b"owned-by-another-object").expect("marker");
        assert!(
            cache
                .store("fp", "http://hub.lan", HubRole::Fallback, t0())
                .is_err()
        );
        assert_eq!(
            std::fs::read(&marker).expect("preserved"),
            b"owned-by-another-object"
        );
        let entries: Vec<_> = std::fs::read_dir(cache.path.parent().expect("parent"))
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("hub-resolution.json")]
        );
    }

    #[test]
    fn oversized_unknown_field_duplicate_field_and_unsafe_url_cache_entries_are_misses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        cache
            .store("fp", "http://hub.lan", HubRole::Fallback, t0())
            .expect("seed");
        let original = std::fs::read_to_string(&cache.path).expect("read");
        let mut entry: serde_json::Value = serde_json::from_str(&original).expect("json");
        entry["url"] = serde_json::Value::String("http://user:secret@hub.lan".to_owned());
        let unsafe_url = serde_json::to_string(&entry).expect("serialize");
        let unknown = original.replacen('{', "{\"unknown\":1,", 1);
        let duplicate = original.replacen('{', "{\"version\":1,", 1);
        for text in [
            unsafe_url,
            unknown,
            duplicate,
            " ".repeat(usize::try_from(MAX_CACHE_BYTES + 1).expect("small size")),
        ] {
            std::fs::write(&cache.path, text).expect("write");
            assert!(cache.load("fp", t0()).is_none());
        }
    }

    #[test]
    fn disabled_invalidation_leaves_existing_cache_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, Duration::ZERO);
        std::fs::create_dir_all(cache.path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&cache.path, b"untouched").expect("seed");
        cache.invalidate();
        assert_eq!(std::fs::read(&cache.path).expect("read"), b"untouched");
    }

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_hours(500_000)
    }

    fn cache_in(dir: &tempfile::TempDir, ttl: Duration) -> HubCache {
        HubCache::with_config_path(
            dir.path().join("agent-bus").join("hub-resolution.json"),
            ttl,
            None,
        )
    }

    fn assert_config_alias_is_preserved(cache_path: PathBuf, config_path: &Path) {
        let before = std::fs::read(config_path).expect("synthetic config preimage");
        let cache = HubCache::with_config_path(cache_path, TTL, Some(config_path.to_path_buf()));
        let error = cache
            .store("fp", "http://hub.lan:8400", HubRole::Fallback, t0())
            .expect_err("config alias refused before cache publication");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read(config_path).expect("after rejected store"),
            before
        );
        cache.invalidate();
        assert_eq!(
            std::fs::read(config_path).expect("after rejected invalidate"),
            before
        );
    }

    #[test]
    fn cache_config_alias_store_and_invalidate_preserve_exact_bytes() {
        let directory = tempfile::tempdir().expect("synthetic config directory");
        let config = directory.path().join("config.json");
        std::fs::write(&config, b"synthetic credential config bytes").expect("config fixture");
        assert_config_alias_is_preserved(config.clone(), &config);
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("list directory")
                .count(),
            1
        );
    }

    #[test]
    fn cache_canonical_config_alias_store_and_invalidate_preserve_exact_bytes() {
        let directory = tempfile::tempdir().expect("synthetic config directory");
        let config = directory.path().join("config.json");
        std::fs::write(&config, b"synthetic canonical config bytes").expect("config fixture");
        std::fs::create_dir(directory.path().join("subdir")).expect("existing subdir");
        let alias = directory.path().join("subdir/../config.json");
        assert_config_alias_is_preserved(alias, &config);
    }

    #[test]
    fn cache_relative_config_alias_store_and_invalidate_preserve_exact_bytes() {
        #[cfg(not(windows))]
        let directory = tempfile::tempdir().expect("synthetic config directory");
        #[cfg(windows)]
        let directory = tempfile::tempdir_in(std::env::current_dir().expect("current directory"))
            .expect("owned same-drive temporary config directory");
        let config = directory.path().join("config.json");
        std::fs::write(&config, b"synthetic relative config bytes").expect("config fixture");
        let current = std::env::current_dir().expect("current directory");
        let canonical_config = std::fs::canonicalize(&config).expect("config identity");
        let canonical_current = std::fs::canonicalize(current).expect("current identity");
        let current_parts: Vec<_> = canonical_current.components().collect();
        let config_parts: Vec<_> = canonical_config.components().collect();
        let common = current_parts
            .iter()
            .zip(&config_parts)
            .take_while(|(a, b)| a == b)
            .count();
        assert!(
            common > 0,
            "fixture and worktree must share a filesystem root"
        );
        let mut relative = PathBuf::new();
        for _ in common..current_parts.len() {
            relative.push("..");
        }
        for component in &config_parts[common..] {
            relative.push(component.as_os_str());
        }
        assert!(!relative.is_absolute());
        assert_config_alias_is_preserved(relative, &config);
    }

    #[test]
    fn cache_missing_config_alias_does_not_create_or_delete_configuration() {
        let directory = tempfile::tempdir().expect("synthetic config directory");
        let config = directory.path().join("missing/config.json");
        let cache = HubCache::with_config_path(config.clone(), TTL, Some(config.clone()));
        assert_eq!(
            cache
                .store("fp", "http://hub.lan", HubRole::Fallback, t0())
                .expect_err("missing alias")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        cache.invalidate();
        assert!(!config.exists());
        assert!(!config.parent().expect("parent").exists());
    }

    #[test]
    fn cache_missing_parent_dotdot_alias_preserves_existing_configuration() {
        let directory = tempfile::tempdir().expect("synthetic config directory");
        let config = directory.path().join("config.json");
        std::fs::write(&config, b"synthetic missing parent config bytes").expect("config fixture");
        let alias = directory.path().join("missing/../config.json");
        assert_config_alias_is_preserved(alias, &config);
        assert!(!directory.path().join("missing").exists());
    }

    #[cfg(any(unix, windows))]
    fn create_owned_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn cache_dangling_config_symlink_store_preserves_link_and_missing_target() {
        let directory = tempfile::tempdir().expect("synthetic dangling config directory");
        let target = directory.path().join("cache.json");
        let config = directory.path().join("config.json");
        create_owned_file_symlink(&target, &config).expect("owned dangling config symlink");
        let before = std::fs::read_link(&config).expect("original link target");
        let cache = HubCache::with_config_path(target.clone(), TTL, Some(config.clone()));
        let error = cache
            .store("fp", "http://hub.lan", HubRole::Fallback, t0())
            .expect_err("dangling config alias refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        cache.invalidate();
        assert_eq!(std::fs::read_link(&config).expect("preserved link"), before);
        assert!(!target.exists());
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("owned directory")
                .count(),
            1
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn cache_dangling_config_symlink_invalidation_preserves_existing_cache() {
        let directory = tempfile::tempdir().expect("synthetic dangling config directory");
        let missing = directory.path().join("missing.json");
        let config = directory.path().join("config.json");
        create_owned_file_symlink(&missing, &config).expect("owned dangling config symlink");
        let before = std::fs::read_link(&config).expect("original link target");
        let target = directory.path().join("existing-cache.json");
        std::fs::write(&target, b"existing cache exact bytes").expect("cache fixture");
        let cache = HubCache::with_config_path(target.clone(), TTL, Some(config.clone()));
        cache.invalidate();
        assert_eq!(
            std::fs::read(&target).expect("cache preserved"),
            b"existing cache exact bytes"
        );
        assert_eq!(
            std::fs::read_link(&config).expect("config link preserved"),
            before
        );
        assert!(!missing.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn cache_dangling_config_parent_link_refuses_target_directory_creation() {
        let directory = tempfile::tempdir().expect("synthetic dangling parent directory");
        let missing = directory.path().join("missing-directory");
        let link = directory.path().join("config-parent-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&missing, &link).expect("owned dangling parent symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&missing, &link).expect("owned dangling parent symlink");
        let config = link.join("config.json");
        let target = missing.join("config.json");
        let cache = HubCache::with_config_path(target, TTL, Some(config));
        assert_eq!(
            cache
                .store("fp", "http://hub.lan", HubRole::Fallback, t0())
                .expect_err("dangling parent refused")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        cache.invalidate();
        assert_eq!(
            std::fs::read_link(&link).expect("parent link preserved"),
            missing
        );
        assert!(!missing.exists());
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("owned directory")
                .count(),
            1
        );
    }

    #[test]
    fn cache_alias_resolution_error_refuses_store_and_invalidate() {
        let directory = tempfile::tempdir().expect("synthetic config directory");
        let parent = directory.path().join("foreign-parent-file");
        std::fs::write(&parent, b"foreign parent exact bytes").expect("parent fixture");
        let config = parent.join("config.json");
        let target = directory.path().join("cache.json");
        std::fs::write(&target, b"existing separate cache bytes").expect("cache fixture");
        let cache = HubCache::with_config_path(target.clone(), TTL, Some(config));
        assert!(
            cache
                .store("fp", "http://hub.lan", HubRole::Fallback, t0())
                .is_err()
        );
        cache.invalidate();
        assert_eq!(
            std::fs::read(&target).expect("cache preserved"),
            b"existing separate cache bytes"
        );
        assert_eq!(
            std::fs::read(parent).expect("parent preserved"),
            b"foreign parent exact bytes"
        );
    }

    #[test]
    fn cache_separate_from_configuration_still_stores_loads_and_invalidates() {
        let directory = tempfile::tempdir().expect("synthetic config directory");
        let config = directory.path().join("config.json");
        std::fs::write(&config, b"synthetic separate config bytes").expect("config fixture");
        let target = directory.path().join("cache/hub-resolution.json");
        let cache = HubCache::with_config_path(target.clone(), TTL, Some(config.clone()));
        cache
            .store("fp", "http://hub.lan", HubRole::Fallback, t0())
            .expect("separate cache store");
        let hit = cache.load("fp", t0()).expect("separate cache load");
        assert_eq!(hit.url, "http://hub.lan");
        assert_eq!(hit.role, HubRole::Fallback);
        cache.invalidate();
        assert!(!target.exists());
        assert_eq!(
            std::fs::read(config).expect("config preserved"),
            b"synthetic separate config bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_symlink_config_alias_preserves_config_and_link() {
        let directory = tempfile::tempdir().expect("synthetic config directory");
        let config = directory.path().join("config.json");
        std::fs::write(&config, b"synthetic symlink config bytes").expect("config fixture");
        let link = directory.path().join("cache-link.json");
        std::os::unix::fs::symlink(&config, &link).expect("owned symlink");
        assert_config_alias_is_preserved(link.clone(), &config);
        assert!(
            std::fs::symlink_metadata(link)
                .expect("preserved symlink")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(windows)]
    #[test]
    fn cache_ascii_case_config_alias_preserves_configuration() {
        let directory = tempfile::tempdir().expect("synthetic config directory");
        let config = directory.path().join("config.json");
        std::fs::write(&config, b"synthetic Windows case config bytes").expect("config fixture");
        assert_config_alias_is_preserved(directory.path().join("CONFIG.JSON"), &config);
    }

    #[test]
    fn a_stored_resolution_is_loaded_within_the_ttl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        cache
            .store("fp", "http://hub.lan:8400", HubRole::Fallback, t0())
            .expect("store");
        let hit = cache
            .load("fp", t0() + Duration::from_secs(10))
            .expect("fresh entry must hit");
        assert_eq!(hit.url, "http://hub.lan:8400");
        assert_eq!(hit.role, HubRole::Fallback);
        assert_eq!(hit.age, Duration::from_secs(10));
    }

    #[test]
    fn an_entry_at_or_past_the_ttl_is_a_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        cache
            .store("fp", "http://hub.lan:8400", HubRole::Authoritative, t0())
            .expect("store");
        assert!(cache.load("fp", t0() + Duration::from_secs(59)).is_some());
        assert!(
            cache.load("fp", t0() + TTL).is_none(),
            "the TTL boundary itself is expired"
        );
        assert!(cache.load("fp", t0() + Duration::from_secs(3600)).is_none());
    }

    #[test]
    fn a_zero_ttl_disables_the_cache_completely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = cache_in(&dir, TTL);
        live.store("fp", "http://hub.lan:8400", HubRole::Fallback, t0())
            .expect("store");

        let disabled = cache_in(&dir, Duration::ZERO);
        assert!(!disabled.enabled());
        assert!(live.enabled());
        assert!(
            disabled.load("fp", t0()).is_none(),
            "a disabled cache must not serve an existing entry"
        );

        let dir2 = tempfile::tempdir().expect("tempdir");
        let never_written = cache_in(&dir2, Duration::ZERO);
        never_written
            .store("fp", "http://hub.lan:8400", HubRole::Fallback, t0())
            .expect("a disabled store is a no-op, not an error");
        assert!(
            !dir2.path().join("agent-bus").exists(),
            "a disabled cache must not touch the file system"
        );
    }

    #[test]
    fn a_different_fingerprint_is_a_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        cache
            .store("fp-a", "http://hub.lan:8400", HubRole::Fallback, t0())
            .expect("store");
        assert!(cache.load("fp-b", t0()).is_none());
        assert!(cache.load("fp-a", t0()).is_some(), "positive control");
    }

    #[test]
    fn unreadable_corrupt_or_foreign_files_are_misses_not_panics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        let path = dir.path().join("agent-bus").join("hub-resolution.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        for content in [
            "",
            "not json",
            "[]",
            "{}",
            r#"{"version": 999, "fingerprint": "fp", "url": "http://x", "role": "fallback", "resolved_at_unix_ms": 1}"#,
            r#"{"version": 1, "fingerprint": "fp", "url": "http://x", "role": "bogus", "resolved_at_unix_ms": 1}"#,
            r#"{"version": 1, "fingerprint": "fp", "url": 5, "role": "fallback", "resolved_at_unix_ms": 1}"#,
        ] {
            std::fs::write(&path, content).expect("write");
            assert!(cache.load("fp", t0()).is_none(), "{content:?}");
        }
        std::fs::remove_file(&path).expect("remove");
        assert!(cache.load("fp", t0()).is_none(), "absent file is a miss");
    }

    #[test]
    fn a_future_dated_entry_is_a_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        cache
            .store(
                "fp",
                "http://hub.lan:8400",
                HubRole::Fallback,
                t0() + Duration::from_secs(500),
            )
            .expect("store");
        assert!(
            cache.load("fp", t0()).is_none(),
            "a clock that went backwards must not extend the TTL"
        );
    }

    #[test]
    fn invalidate_removes_the_entry_and_tolerates_absence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        cache.invalidate();
        cache
            .store("fp", "http://hub.lan:8400", HubRole::Fallback, t0())
            .expect("store");
        assert!(cache.load("fp", t0()).is_some());
        cache.invalidate();
        assert!(cache.load("fp", t0()).is_none());
    }

    #[test]
    fn store_creates_parent_directories_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        cache
            .store("fp", "http://hub.lan:8400", HubRole::Fallback, t0())
            .expect("store");
        cache
            .store("fp", "http://other.lan:8400", HubRole::Fallback, t0())
            .expect("overwrite");
        let names: Vec<String> = std::fs::read_dir(dir.path().join("agent-bus"))
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["hub-resolution.json".to_owned()], "{names:?}");
        assert_eq!(
            cache.load("fp", t0()).expect("hit").url,
            "http://other.lan:8400",
            "the second store wins"
        );
    }

    #[test]
    fn the_file_contains_only_non_secret_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_in(&dir, TTL);
        cache
            .store("fp", "http://hub.lan:8400", HubRole::Cloud, t0())
            .expect("store");
        let text =
            std::fs::read_to_string(dir.path().join("agent-bus").join("hub-resolution.json"))
                .expect("read");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "fingerprint",
                "resolved_at_unix_ms",
                "role",
                "url",
                "version"
            ]
        );
        assert_eq!(value["role"], "cloud");
    }

    fn candidate(url: &str, role: HubRole, auth: CandidateAuth) -> HubCandidate {
        HubCandidate {
            url: url.to_owned(),
            role,
            auth,
            sites: Vec::new(),
            hub: None,
        }
    }

    fn base_list() -> Vec<HubCandidate> {
        vec![
            candidate("http://a:1", HubRole::Authoritative, CandidateAuth::Global),
            candidate("http://b:1", HubRole::Fallback, CandidateAuth::Global),
        ]
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_every_routing_input() {
        let base = candidates_fingerprint(&base_list());
        assert_eq!(base, candidates_fingerprint(&base_list()), "stable");
        assert!(!base.is_empty());

        let mut reordered = base_list();
        reordered.reverse();
        assert_ne!(base, candidates_fingerprint(&reordered), "order");

        let mut other_url = base_list();
        other_url[1].url = "http://c:1".to_owned();
        assert_ne!(base, candidates_fingerprint(&other_url), "url");

        let mut other_role = base_list();
        other_role[1].role = HubRole::Cloud;
        assert_ne!(base, candidates_fingerprint(&other_role), "role");

        let mut other_auth = base_list();
        other_auth[1].auth = CandidateAuth::TokenEnv("T".to_owned());
        assert_ne!(
            base,
            candidates_fingerprint(&other_auth),
            "credential source"
        );

        let mut other_file = base_list();
        other_file[1].auth = CandidateAuth::TokenFile("/t".to_owned());
        assert_ne!(
            candidates_fingerprint(&other_auth),
            candidates_fingerprint(&other_file),
            "env vs file source"
        );

        assert_ne!(
            candidates_fingerprint(&[]),
            base,
            "an empty list is its own fingerprint"
        );
    }

    #[derive(Default)]
    struct PathEnv {
        vars: HashMap<String, String>,
        home: Option<PathBuf>,
    }

    impl CredentialEnv for PathEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
        fn read_to_string(&self, _path: &Path) -> io::Result<String> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }
        fn home_dir(&self) -> Option<PathBuf> {
            self.home.clone()
        }
    }

    fn env_with(vars: &[(&str, &str)], home: Option<&str>) -> PathEnv {
        PathEnv {
            vars: vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            home: home.map(PathBuf::from),
        }
    }

    fn tail() -> PathBuf {
        PathBuf::from("agent-bus").join("hub-resolution.json")
    }

    #[test]
    fn explicit_override_wins_on_every_platform() {
        for os in [CacheOs::Windows, CacheOs::MacOs, CacheOs::Unix] {
            let env = env_with(
                &[("AGENT_BUS_HUB_CACHE_FILE", "/custom/c.json")],
                Some("/h"),
            );
            assert_eq!(
                default_cache_path(os, &env),
                Some(PathBuf::from("/custom/c.json")),
                "{os:?}"
            );
        }
        let blank = env_with(&[("AGENT_BUS_HUB_CACHE_FILE", "  ")], Some("/h"));
        assert_ne!(
            default_cache_path(CacheOs::Unix, &blank),
            Some(PathBuf::from("  ")),
            "a blank override is ignored"
        );
    }

    #[test]
    fn windows_uses_localappdata_then_the_home_appdata_fallback() {
        let env = env_with(
            &[("LOCALAPPDATA", "C:/Users/u/AppData/Local")],
            Some("C:/Users/u"),
        );
        assert_eq!(
            default_cache_path(CacheOs::Windows, &env),
            Some(PathBuf::from("C:/Users/u/AppData/Local").join(tail()))
        );
        let no_local = env_with(&[], Some("C:/Users/u"));
        assert_eq!(
            default_cache_path(CacheOs::Windows, &no_local),
            Some(
                PathBuf::from("C:/Users/u")
                    .join("AppData")
                    .join("Local")
                    .join(tail())
            )
        );
    }

    #[test]
    fn macos_uses_library_caches() {
        let env = env_with(&[], Some("/Users/u"));
        assert_eq!(
            default_cache_path(CacheOs::MacOs, &env),
            Some(PathBuf::from("/Users/u/Library/Caches").join(tail()))
        );
    }

    #[test]
    fn unix_uses_xdg_cache_home_then_dot_cache() {
        let xdg = env_with(&[("XDG_CACHE_HOME", "/xdg")], Some("/home/u"));
        assert_eq!(
            default_cache_path(CacheOs::Unix, &xdg),
            Some(PathBuf::from("/xdg").join(tail()))
        );
        let none = env_with(&[], Some("/home/u"));
        assert_eq!(
            default_cache_path(CacheOs::Unix, &none),
            Some(PathBuf::from("/home/u/.cache").join(tail()))
        );
        let relative = env_with(&[("XDG_CACHE_HOME", "relative/dir")], Some("/home/u"));
        assert_eq!(
            default_cache_path(CacheOs::Unix, &relative),
            Some(PathBuf::from("/home/u/.cache").join(tail())),
            "a relative XDG_CACHE_HOME is ignored per the XDG spec"
        );
        let windows_path = env_with(&[("XDG_CACHE_HOME", "C:/cache")], Some("/home/u"));
        assert_eq!(
            default_cache_path(CacheOs::Unix, &windows_path),
            Some(PathBuf::from("/home/u/.cache").join(tail())),
            "a Windows drive path is not an absolute Unix XDG path"
        );
    }

    #[test]
    fn no_home_and_no_cache_variables_means_no_default_path() {
        let env = env_with(&[], None);
        for os in [CacheOs::Windows, CacheOs::MacOs, CacheOs::Unix] {
            assert_eq!(default_cache_path(os, &env), None, "{os:?}");
        }
    }
}
