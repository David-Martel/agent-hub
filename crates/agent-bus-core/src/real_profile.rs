//! The operating system's own answer for "where is this user's profile".
//!
//! `USERPROFILE` and `HOME` are ordinary environment variables, and agent
//! launchers deliberately repoint them at a per-session sandbox directory.
//! Reading the profile back from those variables therefore finds the sandbox,
//! not the account's real profile with its real `~/.config/agent-bus`. The
//! Windows known-folder API ignores the environment, so it still names the
//! real profile from inside such a sandbox.
//!
//! On Unix the conventional answer is `$HOME` itself, which is what
//! [`crate::settings::config_file_path`] already reads, so there is nothing
//! to add and [`real_profile_dir`] returns `None`.

use std::path::PathBuf;

/// The account's real profile directory, independent of `USERPROFILE`/`HOME`.
///
/// `None` when the platform has no environment-independent answer or the
/// lookup fails; callers then keep their environment-derived behavior.
#[must_use]
pub(crate) fn real_profile_dir() -> Option<PathBuf> {
    platform::real_profile_dir()
}

#[cfg(windows)]
mod platform {
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    /// A `GUID` laid out as in the Windows SDK.
    #[repr(C)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    /// `FOLDERID_Profile`, `{5E6C858F-0E22-4760-9AFE-EA3317B67173}`.
    static FOLDERID_PROFILE: Guid = Guid {
        data1: 0x5E6C_858F,
        data2: 0x0E22,
        data3: 0x4760,
        data4: [0x9A, 0xFE, 0xEA, 0x33, 0x17, 0xB6, 0x71, 0x73],
    };

    #[link(name = "shell32")]
    unsafe extern "system" {
        /// Returns an `HRESULT`; `path` receives a `CoTaskMemAlloc`ed UTF-16 string.
        fn SHGetKnownFolderPath(
            rfid: *const Guid,
            flags: u32,
            token: *mut c_void,
            path: *mut *mut u16,
        ) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoTaskMemFree(pv: *mut c_void);
    }

    pub(super) fn real_profile_dir() -> Option<PathBuf> {
        let mut wide: *mut u16 = std::ptr::null_mut();
        // SAFETY: `FOLDERID_PROFILE` is a valid, 'static GUID; flags 0 and a null token ask for the
        // current user's profile; `wide` is a valid out-pointer for the allocated string.
        let result = unsafe {
            SHGetKnownFolderPath(
                &raw const FOLDERID_PROFILE,
                0,
                std::ptr::null_mut(),
                &raw mut wide,
            )
        };
        if wide.is_null() {
            return None;
        }
        let path = (result >= 0).then(|| {
            let mut len = 0;
            // SAFETY: on success `wide` is a NUL-terminated UTF-16 string, so every index up to and
            // including the terminator is inside the allocation.
            while unsafe { *wide.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: the `len` units before the terminator were just read, and `wide` is not freed
            // until after this copy.
            PathBuf::from(OsString::from_wide(unsafe {
                std::slice::from_raw_parts(wide, len)
            }))
        });
        // SAFETY: `wide` came from `SHGetKnownFolderPath`, whose contract is to free it with
        // `CoTaskMemFree` whether or not the call succeeded; it is not used afterwards.
        unsafe { CoTaskMemFree(wide.cast()) };
        path
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::PathBuf;

    pub(super) const fn real_profile_dir() -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::process::Command;

    const CHILD_FLAG: &str = "AGENT_BUS_TEST_REAL_PROFILE_CHILD";

    /// Runs inside the re-executed test binary below; a no-op in a normal run.
    #[test]
    fn child_reports_profile() {
        if std::env::var_os(CHILD_FLAG).is_none() {
            return;
        }
        match real_profile_dir() {
            Some(dir) => println!("PROFILE={}", dir.display()),
            None => println!("PROFILE=<none>"),
        }
    }

    /// The sandbox case this module exists for: with `USERPROFILE` and `HOME`
    /// pointed at an empty directory, the answer must not follow them. The
    /// child process gets the overridden environment, so this test never
    /// mutates its own (other tests read these variables concurrently).
    #[cfg(windows)]
    #[test]
    fn profile_ignores_redirected_userprofile_and_home() {
        let sandbox = tempfile::tempdir().expect("sandbox directory");
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "real_profile::tests::child_reports_profile",
                "--nocapture",
            ])
            .env(CHILD_FLAG, "1")
            .env("USERPROFILE", sandbox.path())
            .env("HOME", sandbox.path())
            .output()
            .expect("re-run the test binary");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let reported = stdout
            .lines()
            .find_map(|line| line.strip_prefix("PROFILE="))
            .unwrap_or_else(|| panic!("child printed no PROFILE line:\n{stdout}"));
        assert_ne!(reported, "<none>", "known-folder lookup failed");
        let reported = PathBuf::from(reported);
        assert!(
            reported.is_absolute(),
            "profile must be absolute: {reported:?}"
        );
        assert!(reported.is_dir(), "profile must exist: {reported:?}");
        assert!(
            !reported.starts_with(sandbox.path()),
            "profile {reported:?} followed the redirected environment {:?}",
            sandbox.path()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn profile_is_not_overridden_off_windows() {
        assert!(real_profile_dir().is_none());
    }
}
