//! Private regular journal files; no inherited permissions before data.
use anyhow::{Context, Result, bail};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::path::{Component, Path};

pub(crate) fn plain_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("absolute outbox path required");
    }
    for component in path.components() {
        match component {
            Component::ParentDir | Component::CurDir => bail!("relative outbox component refused"),
            Component::Normal(value) => {
                let value = value.to_str().context("outbox path encoding")?;
                if value.is_empty() || value.ends_with(['.', ' ']) || value.contains([':', '\0']) {
                    bail!("unsafe outbox component");
                }
                let basename = value.split('.').next().unwrap_or("").to_ascii_uppercase();
                if matches!(basename.as_str(), "$NULL" | "CON" | "AUX" | "PRN" | "NUL")
                    || (basename.len() == 4
                        && (basename.starts_with("COM") || basename.starts_with("LPT"))
                        && matches!(basename.as_bytes()[3], b'1'..=b'9'))
                {
                    bail!("reserved outbox component");
                }
            }
            Component::Prefix(prefix) => {
                #[cfg(windows)]
                if !matches!(prefix.kind(), std::path::Prefix::Disk(_)) {
                    bail!("UNC outbox path refused");
                }
                #[cfg(not(windows))]
                let _ = prefix;
            }
            Component::RootDir => {}
        }
    }
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!("outbox symlink refused");
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        bail!("outbox reparse path refused");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("outbox ancestor metadata"),
        }
    }
    Ok(())
}

/// Create one private directory leaf without modifying an existing directory.
pub(crate) fn create_directory(path: &Path) -> Result<()> {
    plain_path(path)?;
    let parent = path.parent().context("outbox directory parent required")?;
    if !parent.is_dir() {
        bail!("existing outbox directory ancestor required");
    }
    platform::create_directory(path)?;
    plain_path(path)
}

/// Open an existing private regular file or exclusively create it before bytes.
pub(crate) fn open(path: &Path) -> Result<File> {
    plain_path(path)?;
    let parent = path.parent().context("outbox parent required")?;
    if !parent.is_dir() {
        bail!("existing private outbox directory required");
    }
    let _parent = platform::validate_directory(parent)?;
    let file = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("regular outbox file required");
            }
            platform::open_existing(path)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match platform::create(path) {
                Ok(file) => file,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    platform::open_existing(path)?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error).context("outbox file metadata"),
    };
    platform::validate_file(&file)?;
    plain_path(path)?;
    Ok(file)
}

#[cfg(unix)]
mod platform {
    use super::{Context, File, OpenOptions, Path, Result, bail, fs};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    fn validate_metadata(metadata: &fs::Metadata) -> Result<()> {
        // SAFETY: geteuid takes no arguments and returns the current effective UID.
        let uid = unsafe { geteuid() };
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            bail!("owner-only Unix outbox required");
        }
        Ok(())
    }
    pub(super) fn validate_directory(path: &Path) -> Result<File> {
        let file = File::open(path)?;
        if !file.metadata()?.is_dir() {
            bail!("outbox parent must be a directory");
        }
        validate_metadata(&file.metadata()?)?;
        Ok(file)
    }
    pub(super) fn create_directory(path: &Path) -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create private outbox directory"),
        }
        validate_directory(path).map(|_| ())
    }
    pub(super) fn validate_file(file: &File) -> Result<()> {
        if !file.metadata()?.is_file() || file.metadata()?.nlink() != 1 {
            bail!("regular outbox required");
        }
        validate_metadata(&file.metadata()?)
    }
    pub(super) fn create(path: &Path) -> Result<File> {
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?)
    }
    pub(super) fn open_existing(path: &Path) -> Result<File> {
        let expected = fs::symlink_metadata(path)?;
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let observed = file.metadata()?;
        if !expected.is_file()
            || expected.dev() != observed.dev()
            || expected.ino() != observed.ino()
        {
            bail!("outbox file identity changed");
        }
        Ok(file)
    }
}

#[cfg(windows)]
mod platform {
    use super::{Context, File, Path, Result, bail};
    use std::ffi::c_void;
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    };

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
        fn GetTokenInformation(
            token: *mut c_void,
            class: u32,
            data: *mut c_void,
            size: u32,
            needed: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *const c_void, text: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
        fn CreateFileW(
            path: *const u16,
            access: u32,
            share: u32,
            attributes: *const SecurityAttributes,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
    }

    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: This wrapper exclusively owns the valid token handle returned by Win32.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct LocalMemory(*mut c_void);
    impl Drop for LocalMemory {
        fn drop(&mut self) {
            // SAFETY: This wrapper owns the LocalAlloc-backed allocation returned by Win32.
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn owner_sid() -> Result<String> {
        let mut token = std::ptr::null_mut();
        // TOKEN_QUERY, on the current process pseudo-handle. The returned
        // token is owned here and remains alive until the SID is copied.
        // SAFETY: Current-process pseudo-handle is valid; token is writable output storage.
        if unsafe { OpenProcessToken(GetCurrentProcess(), 8, &raw mut token) } == 0 {
            return Err(std::io::Error::last_os_error()).context("query current Windows token");
        }
        let token = Handle(token);
        let mut needed = 0;
        // SAFETY: Owned token is valid; null/zero buffer queries size into writable needed.
        unsafe { GetTokenInformation(token.0, 1, std::ptr::null_mut(), 0, &raw mut needed) };
        if needed == 0 || needed > 1_048_576 {
            bail!("invalid Windows token-user buffer size");
        }
        // usize storage ensures pointer alignment for TOKEN_USER's first SID
        // pointer. Win32 fills it and owns the SID within this buffer.
        let mut buffer =
            vec![0_usize; usize::try_from(needed)?.div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: Token is valid; aligned buffer has at least needed writable bytes.
        if unsafe {
            GetTokenInformation(
                token.0,
                1,
                buffer.as_mut_ptr().cast(),
                needed,
                &raw mut needed,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("read Windows token user");
        }
        // SAFETY: Successful TOKEN_USER readback initialized this aligned first SID pointer.
        let sid = unsafe { *buffer.as_ptr().cast::<*const c_void>() };
        let mut text = std::ptr::null_mut();
        // SAFETY: SID remains inside live token-user buffer; text is writable output storage.
        if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
            return Err(std::io::Error::last_os_error()).context("format Windows user SID");
        }
        let owned = LocalMemory(text.cast());
        let mut length = 0;
        // SAFETY: Successful SID conversion returns NUL-terminated UTF-16 held by owned.
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: Scan established length initialized UTF-16 units in the still-owned allocation.
        let value = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })?;
        drop(owned);
        Ok(value)
    }

    fn private_attributes() -> Result<(LocalMemory, SecurityAttributes)> {
        let sid = owner_sid()?;
        // Explicit current-user owner and protected DACL: no inherited or
        // other principal can read bytes, even before the first write.
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;;FA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: SDDL is NUL-terminated; descriptor is writable and optional size may be null.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("construct private Windows DACL");
        }
        let owned = LocalMemory(descriptor);
        let attributes = SecurityAttributes {
            length: u32::try_from(std::mem::size_of::<SecurityAttributes>())?,
            descriptor,
            inherit: 0,
        };
        Ok((owned, attributes))
    }

    #[repr(C)]
    struct FileInformation {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[repr(C)]
    struct AclSizeInformation {
        count: u32,
        used: u32,
        free: u32,
    }
    #[repr(C)]
    struct AceHeader {
        kind: u8,
        flags: u8,
        size: u16,
    }
    #[repr(C)]
    struct AllowedAce {
        header: AceHeader,
        mask: u32,
        sid_start: u32,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSidToSidW(text: *const u16, sid: *mut *mut c_void) -> i32;
        fn GetSecurityInfo(
            handle: *mut c_void,
            kind: u32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn GetSecurityDescriptorControl(
            descriptor: *const c_void,
            control: *mut u16,
            revision: *mut u32,
        ) -> i32;
        fn GetAclInformation(acl: *const c_void, data: *mut c_void, size: u32, class: u32) -> i32;
        fn GetAce(acl: *const c_void, index: u32, output: *mut *mut c_void) -> i32;
        fn IsValidAcl(acl: *const c_void) -> i32;
        fn IsValidSid(sid: *const c_void) -> i32;
        fn EqualSid(first: *const c_void, second: *const c_void) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: *mut c_void, info: *mut FileInformation) -> i32;
        fn GetFileType(handle: *mut c_void) -> u32;
        fn CreateDirectoryW(path: *const u16, attributes: *const SecurityAttributes) -> i32;
    }

    fn wide(path: &Path) -> Result<Vec<u16>> {
        let value: Vec<u16> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            bail!("outbox path contains NUL");
        }
        Ok(value.into_iter().chain(Some(0)).collect())
    }

    fn information(file: &File) -> Result<FileInformation> {
        let mut info = std::mem::MaybeUninit::<FileInformation>::uninit();
        // SAFETY: File owns a live handle; correctly sized output is writable.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(std::io::Error::last_os_error()).context("outbox held-handle metadata");
        }
        // SAFETY: Successful API initialized all BY_HANDLE_FILE_INFORMATION fields.
        Ok(unsafe { info.assume_init() })
    }

    fn validate(file: &File, directory: bool) -> Result<()> {
        let info = information(file)?;
        // SAFETY: File owns a live Win32 handle.
        if unsafe { GetFileType(file.as_raw_handle()) } != 1
            || info.attributes & 0x400 != 0
            || (info.attributes & 0x10 != 0) != directory
            || (!directory && info.links != 1)
        {
            bail!("plain single-link outbox disk file required");
        }
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: Held handle is live; OWNER|DACL output fields are writable.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                1,
                5,
                &raw mut owner,
                std::ptr::null_mut(),
                &raw mut dacl,
                std::ptr::null_mut(),
                &raw mut descriptor,
            )
        };
        if status != 0 {
            return Err(std::io::Error::from_raw_os_error(i32::try_from(status)?))
                .context("read held outbox security descriptor");
        }
        let descriptor = LocalMemory(descriptor);
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: Descriptor is live LocalAlloc-backed output; typed fields writable.
        if unsafe {
            GetSecurityDescriptorControl(descriptor.0, &raw mut control, &raw mut revision)
        } == 0
            || control & 0x1004 != 0x1004
            || owner.is_null()
            || dacl.is_null()
        {
            bail!("protected present outbox DACL required");
        }
        // SAFETY: Owner/DACL refer to the still-owned Win32 security descriptor.
        if unsafe { IsValidSid(owner) } == 0 || unsafe { IsValidAcl(dacl) } == 0 {
            bail!("valid outbox owner and DACL required");
        }
        let text: Vec<u16> = owner_sid()?.encode_utf16().chain(Some(0)).collect();
        let mut user = std::ptr::null_mut();
        // SAFETY: NUL-terminated valid SID string; user is writable output.
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &raw mut user) } == 0 {
            return Err(std::io::Error::last_os_error()).context("read current user SID");
        }
        let user = LocalMemory(user);
        // SAFETY: Both valid SIDs belong to live owned allocations.
        if unsafe { EqualSid(owner, user.0) } == 0 {
            bail!("outbox owner must be current user");
        }
        let mut acl = AclSizeInformation {
            count: 0,
            used: 0,
            free: 0,
        };
        // SAFETY: Valid ACL and correctly sized aligned output.
        if unsafe {
            GetAclInformation(
                dacl,
                (&raw mut acl).cast(),
                u32::try_from(std::mem::size_of::<AclSizeInformation>())?,
                2,
            )
        } == 0
            || acl.count != 1
        {
            bail!("sole current-user outbox ACE required");
        }
        let mut raw = std::ptr::null_mut();
        // SAFETY: Valid ACL has exactly one ACE; output pointer writable.
        if unsafe { GetAce(dacl, 0, &raw mut raw) } == 0 || raw.is_null() {
            bail!("outbox ACE readback failed");
        }
        // SAFETY: GetAce on a validated ACL returns a DWORD-aligned ACE_HEADER.
        let header = unsafe { &*raw.cast::<AceHeader>() };
        let offset = std::mem::offset_of!(AllowedAce, sid_start);
        if header.kind != 0 || header.flags != 0 || usize::from(header.size) < offset + 8 {
            bail!("explicit noninheritable allowed outbox ACE required");
        }
        // SAFETY: ACE type and minimum structure size established above.
        let allowed = unsafe { &*raw.cast::<AllowedAce>() };
        let sid = (&raw const allowed.sid_start).cast::<c_void>();
        // SAFETY: Checked ACE size includes the SID eight-byte header.
        let count = unsafe { *sid.cast::<u8>().add(1) };
        if count > 15 || offset + 8 + usize::from(count) * 4 > usize::from(header.size) {
            bail!("outbox ACE SID bounds invalid");
        }
        // SAFETY: Entire SID fits in the valid ACE; user is a still-live validated SID.
        if allowed.mask != 0x001f_01ff
            // SAFETY: Complete SID bounds were checked within the owned descriptor.
            || unsafe { IsValidSid(sid) } == 0
            // SAFETY: Both validated SIDs remain alive in their owned allocations.
            || unsafe { EqualSid(sid, user.0) } == 0
        {
            bail!("sole full-control current-user outbox ACE required");
        }
        Ok(())
    }

    fn open_handle(path: &Path, directory: bool, create: bool) -> Result<File> {
        let name = wide(path)?;
        let attributes = if create {
            Some(private_attributes()?)
        } else {
            None
        };
        let security = attributes
            .as_ref()
            .map_or(std::ptr::null(), |(_, value)| &raw const *value);
        // File read/write permits journal reads and locks; READ|WRITE sharing permits
        // competing writers, while omitting DELETE prevents namespace replacement.
        let access = if directory { 0x0002_0080 } else { 0xc000_0000 };
        let flags = if directory { 0x0220_0000 } else { 0x0020_0080 };
        // SAFETY: NUL-terminated name and optional owned descriptor live through call.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                3,
                security,
                if create { 1 } else { 3 },
                flags,
                std::ptr::null_mut(),
            )
        };
        if handle == -1_isize as *mut c_void {
            return Err(std::io::Error::last_os_error()).context("open private Windows outbox");
        }
        // SAFETY: Successful API transfers exactly one valid owned file handle.
        let file = unsafe { File::from_raw_handle(handle) };
        validate(&file, directory)?;
        Ok(file)
    }

    pub(super) fn validate_directory(path: &Path) -> Result<File> {
        open_handle(path, true, false)
    }
    pub(super) fn validate_file(file: &File) -> Result<()> {
        validate(file, false)
    }
    pub(super) fn create(path: &Path) -> Result<File> {
        open_handle(path, false, true)
    }
    pub(super) fn open_existing(path: &Path) -> Result<File> {
        let file = open_handle(path, false, false)?;
        let second = open_handle(path, false, false)?;
        let first = information(&file)?;
        let second = information(&second)?;
        if (first.volume, first.index_high, first.index_low)
            != (second.volume, second.index_high, second.index_low)
        {
            bail!("outbox file identity changed");
        }
        Ok(file)
    }

    pub(super) fn create_directory(path: &Path) -> Result<()> {
        let name = wide(path)?;
        let (_owned, attributes) = private_attributes()?;
        // SAFETY: NUL-terminated name and descriptor remain live during call.
        if unsafe { CreateDirectoryW(name.as_ptr(), &raw const attributes) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error).context("create private outbox directory");
            }
        }
        validate_directory(path).map(|_| ())
    }
}

#[cfg(all(test, unix))]
mod unix_alias_tests {
    #[test]
    fn outbox_private_unix_hardlink_alias_refuses_without_changing_bytes() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("private");
        super::create_directory(&parent).unwrap();
        let path = parent.join("journal");
        let mut file = super::open(&path).unwrap();
        file.write_all(b"retained original").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let alias = parent.join("alias");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(super::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained original");
        assert_eq!(std::fs::read(&alias).unwrap(), b"retained original");
    }
}
