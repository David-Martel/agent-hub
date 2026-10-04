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

use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::hub_candidates::{CredentialEnv, HubCandidate, HubRole};

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
}

impl HubCache {
    /// A cache stored at `path`, trusting entries for `ttl`.
    #[must_use]
    pub const fn new(path: PathBuf, ttl: Duration) -> Self {
        Self { path, ttl }
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
        let _ = (fingerprint, now);
        todo!("stub")
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
        let _ = (fingerprint, url, role, now);
        todo!("stub")
    }

    /// Delete the cache file. Absence is not an error.
    pub fn invalidate(&self) {
        todo!("stub")
    }
}

/// A stable fingerprint of the configured candidate list (URLs, roles and
/// credential SOURCES, in order). A change to any of them invalidates the
/// cache. It is a hash, and it never covers a token value.
#[must_use]
pub fn candidates_fingerprint(candidates: &[HubCandidate]) -> String {
    let _ = candidates;
    todo!("stub")
}

/// The default cache file location: `AGENT_BUS_HUB_CACHE_FILE` when set,
/// else `<platform cache dir>/agent-bus/hub-resolution.json`.
#[must_use]
pub fn default_cache_path(os: CacheOs, env: &dyn CredentialEnv) -> Option<PathBuf> {
    let _ = (os, env);
    todo!("stub")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub_candidates::CandidateAuth;
    use std::collections::HashMap;
    use std::path::Path;

    const TTL: Duration = Duration::from_secs(60);

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn cache_in(dir: &tempfile::TempDir, ttl: Duration) -> HubCache {
        HubCache::new(
            dir.path().join("agent-bus").join("hub-resolution.json"),
            ttl,
        )
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
    }

    #[test]
    fn no_home_and_no_cache_variables_means_no_default_path() {
        let env = env_with(&[], None);
        for os in [CacheOs::Windows, CacheOs::MacOs, CacheOs::Unix] {
            assert_eq!(default_cache_path(os, &env), None, "{os:?}");
        }
    }
}
