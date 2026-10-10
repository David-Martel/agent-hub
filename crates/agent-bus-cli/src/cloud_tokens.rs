//! Operator tooling for `agentbus.dtmventures.com` cloud credentials.
//!
//! Config-file driven (manifest + token map + client token file). Never prints
//! secret token values. Refuses reuse of the on-site hub `auth_token`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    cloud_base_url: String,
    #[serde(default)]
    worker_name: Option<String>,
    storage: Storage,
    #[serde(default)]
    anti_lockout: AntiLockout,
    #[serde(default)]
    bitwarden: Bitwarden,
    identities: Vec<Identity>,
    #[serde(default)]
    deployed_map_authority: Option<DeployedMapAuthority>,
    #[serde(default)]
    revocation: Option<Revocation>,
}

#[derive(Debug, Clone, Deserialize)]
struct Storage {
    tokens_map_path: String,
    client_token_path: String,
    #[serde(default = "default_role")]
    client_token_role: String,
    client_token_agent: String,
    #[serde(default)]
    client_token_host: Option<String>,
}

fn default_role() -> String {
    "agent".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
struct AntiLockout {
    #[serde(default = "default_true")]
    keep_previous_map_backup: bool,
    #[serde(default = "default_backup_dir")]
    backup_dir: String,
    #[serde(default = "default_true")]
    require_bw_item_before_wrangler_put: bool,
}

impl Default for AntiLockout {
    fn default() -> Self {
        Self {
            keep_previous_map_backup: true,
            backup_dir: default_backup_dir(),
            require_bw_item_before_wrangler_put: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_backup_dir() -> String {
    "~/.config/agent-bus/token-backups".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
struct Identity {
    id: String,
    role: String,
    agent: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    hub: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenMeta {
    agent: String,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hub: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct DeployedMapAuthority {
    source_kind: String,
    authority_reference: String,
    baseline_path: String,
    baseline_sha256: String,
    candidate_sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Revocation {
    retired: Vec<RetiredToken>,
    exclusive_lease_path: String,
    exclusive_lease_sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RevocationLease {
    status: String,
    owner: String,
    resource: String,
    expires_at_utc: String,
    evidence_reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevocationPhase {
    BeforeUpload,
    AfterUpload,
}

#[derive(Debug, Clone, Deserialize)]
struct RetiredToken {
    identity_id: String,
    token_sha256: String,
    cutover_receipt_path: String,
    cutover_receipt_sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CutoverReceipt {
    identity_id: String,
    retired_token_sha256: String,
    replacement_token_sha256: String,
    baseline_sha256: String,
    cloud_base_url: String,
    replacement_authenticated: bool,
    retired_client_disconnected: bool,
    evidence_reference: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Bitwarden {
    #[serde(default = "default_item_name")]
    item_name: String,
}

fn default_item_name() -> String {
    "agentbus.dtmventures.com AGENT_BUS_TOKENS".into()
}

impl Default for Bitwarden {
    fn default() -> Self {
        Self {
            item_name: default_item_name(),
        }
    }
}

/// Subcommand actions for `agent-bus cloud-tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CloudTokensAction {
    InitManifest,
    Mint,
    Rotate,
    Activate,
    WriteClient,
    Status,
    Smoke,
    WranglerHint,
    BwUpsert,
    WranglerPut,
    Revoke,
}

fn home_dir() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(p));
    }
    bail!("neither USERPROFILE nor HOME is set");
}

fn expand_path(raw: &str) -> Result<PathBuf> {
    let home = home_dir()?;
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else if raw == "~" {
        home
    } else {
        PathBuf::from(raw)
    };
    Ok(expanded)
}

fn default_manifest_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/agent-bus/cloud-tokens.manifest.json"))
}

fn default_example_path(repo_hint: Option<&Path>) -> PathBuf {
    if let Some(root) = repo_hint {
        return root.join("config/cloud-tokens.manifest.example.json");
    }
    // Fall back relative to CARGO_MANIFEST_DIR at compile time for tests,
    // and cwd-relative for operators running from the repo root.
    PathBuf::from("config/cloud-tokens.manifest.example.json")
}

fn sha256_hex16(data: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    let full = hasher.finalize();
    hex_bytes(&full[..8])
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut output, byte| {
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
        output
    })
}

fn onsite_hub_token_fp() -> Result<Option<String>> {
    let cfg = home_dir()?.join(".config/agent-bus/config.json");
    if !cfg.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&cfg).with_context(|| format!("read {}", cfg.display()))?;
    let v: Value = serde_json::from_str(&raw).context("parse on-site config.json")?;
    let Some(tok) = v.get("auth_token").and_then(Value::as_str) else {
        return Ok(None);
    };
    if tok.is_empty() {
        return Ok(None);
    }
    Ok(Some(sha256_hex16(tok)))
}

fn new_token_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0_u8; bytes];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .context("OS token entropy unavailable")?;
    Ok(hex_bytes(&buf))
}

fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    validate_secret_path(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp = path.with_file_name(format!(
        ".cloud-secret-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut created = false;
    let outcome: Result<()> = (|| {
        let mut f = create_private_file(&tmp).context("create private staging file")?;
        created = true;
        f.write_all(contents.as_bytes())?;
        if !contents.ends_with('\n') {
            f.write_all(b"\n")?;
        }
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path).context("publish private secret file")?;
        Ok(())
    })();
    if outcome.is_err() && created && tmp.exists() {
        fs::remove_file(&tmp).context("remove failed private staging file")?;
    }
    outcome?;
    println!("Wrote owner-restricted file: {}", path.display());
    Ok(())
}

fn validate_secret_path(path: &Path) -> Result<()> {
    let leaf = path
        .file_name()
        .context("secret path has no filename")?
        .to_string_lossy();
    let base = leaf
        .trim_end_matches(['.', ' '])
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if base == "$NULL" {
        bail!("literal null-sink filenames are refused");
    }
    #[cfg(windows)]
    {
        if leaf.contains(':')
            || matches!(base.as_str(), "AUX" | "CON" | "NUL" | "PRN")
            || (1..=9).any(|n| base == format!("COM{n}") || base == format!("LPT{n}"))
        {
            bail!("reserved Windows device filenames are refused");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    validate_secret_path(path)?;
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

#[cfg(windows)]
fn create_private_file(path: &Path) -> Result<fs::File> {
    validate_secret_path(path)?;
    windows_private_file::create(path)
}

#[cfg(not(any(unix, windows)))]
fn create_private_file(_path: &Path) -> Result<fs::File> {
    bail!("private secret creation is unsupported on this platform")
}

#[cfg(windows)]
mod windows_private_file {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};

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

    fn private_descriptor() -> Result<LocalMemory> {
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
        Ok(LocalMemory(descriptor))
    }

    #[cfg(test)]
    pub(super) fn inspect_private_descriptor(inspect: impl FnOnce(*const c_void)) -> Result<()> {
        let descriptor = private_descriptor()?;
        inspect(descriptor.0);
        Ok(())
    }

    pub(super) fn create(path: &Path) -> Result<fs::File> {
        let owned = private_descriptor()?;
        let attributes = SecurityAttributes {
            length: u32::try_from(std::mem::size_of::<SecurityAttributes>())?,
            descriptor: owned.0,
            inherit: 0,
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            bail!("secret filename contains NUL");
        }
        let wide: Vec<u16> = wide.into_iter().chain(Some(0)).collect();
        // GENERIC_WRITE, no sharing, CREATE_NEW, normal + OPEN_REPARSE_POINT.
        // Win32 copies the security descriptor during this call.
        // SAFETY: wide is NUL-terminated and attributes/descriptor remain live during this call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0x4000_0000,
                0,
                &raw const attributes,
                1,
                0x0020_0080,
                std::ptr::null_mut(),
            )
        };
        if handle == -1_isize as *mut c_void {
            return Err(std::io::Error::last_os_error()).context("create owner-only Windows file");
        }
        drop(owned);
        // A successful CreateFileW transfers exactly one valid owned handle.
        // SAFETY: Excluding INVALID_HANDLE_VALUE leaves one owned file handle to transfer.
        Ok(unsafe { fs::File::from_raw_handle(handle) })
    }
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse manifest {}", path.display()))
}

fn ensure_manifest(manifest: &Path, example: &Path) -> Result<Manifest> {
    if !manifest.is_file() {
        if !example.is_file() {
            bail!(
                "missing manifest {} and example {}",
                manifest.display(),
                example.display()
            );
        }
        if let Some(parent) = manifest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(example, manifest).context("copy example manifest")?;
        println!("Initialized manifest: {}", manifest.display());
    }
    load_manifest(manifest)
}

fn action_manifest(manifest: &Path, example: &Path, dry_run: bool) -> Result<Manifest> {
    if dry_run && !manifest.is_file() {
        load_manifest(example)
    } else {
        ensure_manifest(manifest, example)
    }
}

/// Release the map lock even when a subprocess inherited its file description.
struct TokenMapLock(fs::File);

impl Drop for TokenMapLock {
    fn drop(&mut self) {
        // Closing one descriptor does not release a Unix lock while a clone survives.
        let _ = self.0.unlock();
    }
}

fn lock_token_map(path: &Path) -> Result<TokenMapLock> {
    validate_secret_path(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_file_name(".cloud-token-operations.lock");
    let file = match create_private_file(&lock_path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            if fs::symlink_metadata(&lock_path)?.file_type().is_symlink() {
                bail!("token lock must not be a symlink");
            }
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)?
        }
        Err(error) => return Err(error),
    };
    file.try_lock()
        .map_err(|_error| anyhow::anyhow!("another cloud-token operation holds the map lock"))?;
    Ok(TokenMapLock(file))
}

fn assert_no_onsite_reuse<T>(map: &BTreeMap<String, T>, onsite: Option<&str>) -> Result<()> {
    let Some(ofp) = onsite else {
        return Ok(());
    };
    for tok in map.keys() {
        if sha256_hex16(tok) == ofp {
            bail!("refusing token map: cloud token fingerprint matches on-site hub auth_token");
        }
    }
    Ok(())
}

fn cmd_init_manifest(manifest: &Path, example: &Path) -> Result<()> {
    let _ = ensure_manifest(manifest, example)?;
    Ok(())
}

fn cmd_mint(manifest: &Path, example: &Path, dry_run: bool) -> Result<()> {
    cmd_mint_mode(manifest, example, dry_run, false)
}

fn read_token_map(path: &Path) -> Result<BTreeMap<String, Value>> {
    let raw = fs::read_to_string(path).context("read token map")?;
    let map: BTreeMap<String, Value> =
        serde_json::from_str(&raw).map_err(|_error| anyhow::anyhow!("parse token map object"))?;
    validate_token_map(&map)?;
    Ok(map)
}

fn validate_token_map(map: &BTreeMap<String, Value>) -> Result<()> {
    if map.is_empty() {
        bail!("operational token map must not be empty; revocation is a separate operation");
    }
    for (token, value) in map {
        if token.len() < 32 || token.chars().any(char::is_whitespace) {
            bail!("invalid token-map key");
        }
        let meta: TokenMeta = serde_json::from_value(value.clone())
            .map_err(|_error| anyhow::anyhow!("invalid token metadata"))?;
        if meta.agent.is_empty()
            || !matches!(meta.role.as_str(), "agent" | "hub" | "operator")
            || (meta.role == "hub" && meta.hub.as_deref().is_none_or(str::is_empty))
        {
            bail!("invalid token identity binding");
        }
    }
    Ok(())
}

fn recovered_token_map(item: &Value) -> Result<BTreeMap<String, Value>> {
    let notes = item
        .get("notes")
        .and_then(Value::as_str)
        .context("existing recovery item has no notes")?;
    let recovered: BTreeMap<String, Value> = serde_json::from_str(notes)
        .map_err(|_error| anyhow::anyhow!("parse recovered token map"))?;
    validate_token_map(&recovered)?;
    Ok(recovered)
}

fn require_recovery_entries_preserved(
    existing: &Value,
    candidate: &BTreeMap<String, Value>,
) -> Result<()> {
    let recovered = recovered_token_map(existing)?;
    for (token, metadata) in &recovered {
        if candidate.get(token) != Some(metadata) {
            bail!("candidate map omits or changes an existing recovery entry; update refused");
        }
    }
    Ok(())
}

fn identity_matches(
    meta: &TokenMeta,
    role: &str,
    agent: &str,
    host: Option<&str>,
    hub: Option<&str>,
) -> bool {
    meta.role == role
        && meta.agent == agent
        && meta.host.as_deref() == host
        && meta.hub.as_deref() == hub
}

fn resolved_storage_path(path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => {
                resolved.push(other.as_os_str());
                match fs::canonicalize(&resolved) {
                    Ok(canonical) => resolved = canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("resolve storage path"),
                }
            }
        }
    }
    #[cfg(windows)]
    let resolved = PathBuf::from(resolved.to_string_lossy().to_lowercase());
    Ok(resolved)
}

fn validate_storage_paths(storage: &Storage) -> Result<()> {
    let map = expand_path(&storage.tokens_map_path)?;
    let client = expand_path(&storage.client_token_path)?;
    validate_secret_path(&map)?;
    validate_secret_path(&client)?;
    let pending = pending_client_path(&client)?;
    let map_resolved = resolved_storage_path(&map)?;
    let client_resolved = resolved_storage_path(&client)?;
    let pending_resolved = resolved_storage_path(&pending)?;
    if map_resolved == client_resolved
        || map_resolved == pending_resolved
        || client_resolved == pending_resolved
    {
        bail!("token map and client credential paths must be distinct");
    }
    Ok(())
}

fn pending_client_path(client: &Path) -> Result<PathBuf> {
    validate_secret_path(client)?;
    let leaf = client
        .file_name()
        .context("client path has no filename")?
        .to_string_lossy();
    let pending = client.with_file_name(format!("{leaf}.pending"));
    validate_secret_path(&pending)?;
    Ok(pending)
}

fn backup_map(
    store: &Path,
    directory: &Path,
    map: &BTreeMap<String, Value>,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        println!(
            "DRY: would backup {} under {}",
            store.display(),
            directory.display()
        );
        return Ok(());
    }
    let backup = directory.join(format!(
        "cloud-tokens-{}.json",
        uuid::Uuid::new_v4().simple()
    ));
    write_secret_file(&backup, &serde_json::to_string_pretty(map)?)?;
    println!("Backup previous map -> {}", backup.display());
    Ok(())
}

fn cmd_mint_mode(manifest: &Path, example: &Path, dry_run: bool, rotate: bool) -> Result<()> {
    let m = action_manifest(manifest, example, dry_run)?;
    validate_storage_paths(&m.storage)?;
    let store = expand_path(&m.storage.tokens_map_path)?;
    let _lock = if dry_run {
        None
    } else {
        Some(lock_token_map(&store)?)
    };
    let backup_dir = expand_path(&m.anti_lockout.backup_dir)?;
    let onsite = onsite_hub_token_fp()?;

    let mut map = if store.is_file() {
        read_token_map(&store)?
    } else {
        BTreeMap::new()
    };
    let original = map.clone();
    let mut rotated_client = None;
    for ident in &m.identities {
        if ident.agent.is_empty() || !matches!(ident.role.as_str(), "agent" | "hub" | "operator") {
            bail!("bad role on {}: {}", ident.id, ident.role);
        }
        if ident.role == "hub" && ident.hub.as_deref().unwrap_or("").is_empty() {
            bail!("hub identity {} requires hub field", ident.id);
        }
        let selected = ident.role == m.storage.client_token_role
            && ident.agent == m.storage.client_token_agent
            && ident.host == m.storage.client_token_host;
        let exists = map.values().any(|value| {
            serde_json::from_value::<TokenMeta>(value.clone()).is_ok_and(|meta| {
                identity_matches(
                    &meta,
                    &ident.role,
                    &ident.agent,
                    ident.host.as_deref(),
                    ident.hub.as_deref(),
                )
            })
        });
        if exists && !(rotate && selected) {
            continue;
        }
        let mut tok = new_token_hex(32)?;
        // Extremely unlikely collision with on-site fp; retry a few times.
        for _ in 0..8 {
            if !map.contains_key(&tok)
                && onsite
                    .as_deref()
                    .is_none_or(|ofp| sha256_hex16(&tok) != ofp)
            {
                break;
            }
            tok = new_token_hex(32)?;
        }
        if map.contains_key(&tok) {
            bail!("could not generate a unique token");
        }
        if rotate && selected && rotated_client.replace(tok.clone()).is_some() {
            bail!("multiple selected client identities");
        }
        map.insert(
            tok,
            serde_json::to_value(TokenMeta {
                agent: ident.agent.clone(),
                role: ident.role.clone(),
                host: ident.host.clone(),
                hub: ident.hub.clone(),
            })?,
        );
    }
    assert_no_onsite_reuse(&map, onsite.as_deref())?;

    if rotate && rotated_client.is_none() {
        bail!("rotation identity is absent from the manifest roster");
    }

    if store.is_file() && m.anti_lockout.keep_previous_map_backup {
        backup_map(&store, &backup_dir, &original, dry_run)?;
    }

    let body = serde_json::to_string_pretty(&map)? + "\n";
    if dry_run {
        println!(
            "DRY: would write token map ({} entries, {} bytes)",
            map.len(),
            body.len()
        );
        return Ok(());
    }
    write_secret_file(&store, &body)?;
    if let Some(token) = rotated_client {
        write_secret_file(
            &pending_client_path(&expand_path(&m.storage.client_token_path)?)?,
            &token,
        )?;
    }
    println!(
        "Minted {} cloud tokens into map (values not printed).",
        map.len()
    );
    println!(
        "Next: bw-upsert, wrangler-put, then activate a staged rotation; old credentials remain valid until explicit revocation."
    );
    Ok(())
}

fn cmd_write_client(manifest: &Path, example: &Path, dry_run: bool) -> Result<()> {
    let m = action_manifest(manifest, example, dry_run)?;
    validate_storage_paths(&m.storage)?;
    let store = expand_path(&m.storage.tokens_map_path)?;
    let _lock = if dry_run {
        None
    } else {
        Some(lock_token_map(&store)?)
    };
    let client = expand_path(&m.storage.client_token_path)?;
    let map = read_token_map(&store)?;
    assert_no_onsite_reuse(&map, onsite_hub_token_fp()?.as_deref())?;

    let mut chosen = None;
    let current = match fs::read_to_string(&client) {
        Ok(s) => Some(s.trim().to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read existing client credential"),
    };
    let mut matches = Vec::new();
    for (tok, value) in &map {
        let meta: TokenMeta = serde_json::from_value(value.clone())
            .map_err(|_error| anyhow::anyhow!("invalid token metadata"))?;
        if meta.role != m.storage.client_token_role {
            continue;
        }
        if meta.agent != m.storage.client_token_agent {
            continue;
        }
        if meta.host != m.storage.client_token_host {
            continue;
        }
        matches.push(tok.clone());
        if current.as_deref() == Some(tok) {
            chosen = Some(tok.clone());
        }
    }
    if chosen.is_none() && matches.len() == 1 {
        chosen = matches.pop();
    }
    if chosen.is_none() && matches.len() > 1 {
        bail!("multiple client tokens match; restore the existing client file or use rotate");
    }
    let Some(tok) = chosen else {
        bail!(
            "no token for role={} agent={} host={:?}",
            m.storage.client_token_role,
            m.storage.client_token_agent,
            m.storage.client_token_host
        );
    };
    if dry_run {
        println!("DRY: would write client token {}", client.display());
        return Ok(());
    }
    write_secret_file(&client, &tok)?;
    println!(
        "Wrote client cloud-token for agent={} (not printed): {}",
        m.storage.client_token_agent,
        client.display()
    );
    Ok(())
}

fn cmd_status(manifest: &Path, example: &Path) -> Result<()> {
    let m = ensure_manifest(manifest, example)?;
    let store = expand_path(&m.storage.tokens_map_path)?;
    let client = expand_path(&m.storage.client_token_path)?;
    println!("identities={}", m.identities.len());
    println!(
        "token_map_exists={} path={}",
        store.is_file(),
        store.display()
    );
    println!(
        "client_token_exists={} path={}",
        client.is_file(),
        client.display()
    );
    println!("cloud_base_url={}", m.cloud_base_url);
    println!(
        "worker_name={}",
        m.worker_name.as_deref().unwrap_or("agentbus-cloud")
    );
    println!("onsite_fp_present={}", onsite_hub_token_fp()?.is_some());
    if store.is_file() {
        let map = read_token_map(&store)?;
        let mut roles: BTreeMap<String, usize> = BTreeMap::new();
        for value in map.values() {
            let role = value
                .get("role")
                .and_then(Value::as_str)
                .context("missing token role")?;
            *roles.entry(role.to_owned()).or_default() += 1;
        }
        let parts: Vec<String> = roles.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!("role_counts={}", parts.join(","));
    }
    Ok(())
}

#[cfg(feature = "server-mode")]
fn cmd_smoke(manifest: &Path, example: &Path) -> Result<()> {
    let m = ensure_manifest(manifest, example)?;
    let token = fs::read_to_string(expand_path(&m.storage.client_token_path)?)
        .context("authenticated smoke requires a readable client token file")?;
    let token = token.trim().to_owned();
    if token.len() < 32 || token.chars().any(char::is_whitespace) {
        bail!("invalid client token");
    }
    authenticated_smoke(&m.cloud_base_url, &token)
}

#[cfg(feature = "server-mode")]
fn authenticated_smoke(base: &str, token: &str) -> Result<()> {
    let url = reqwest::Url::parse(base).context("invalid cloud URL")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))))
    {
        bail!(
            "cloud URL requires HTTPS (HTTP allowed only for local fixtures), without credentials/query/fragment"
        );
    }
    let base = base.to_owned();
    let token = token.to_owned();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build smoke runtime")?
            .block_on(smoke_http(&base, &token))
    })
    .join()
    .map_err(|_panic| anyhow::anyhow!("smoke runtime thread failed"))?
}

#[cfg(not(feature = "server-mode"))]
fn cmd_smoke(_manifest: &Path, _example: &Path) -> Result<()> {
    bail!("cloud authenticated smoke requires a build with server-mode enabled")
}

#[cfg(not(feature = "server-mode"))]
fn authenticated_smoke(_base: &str, _token: &str) -> Result<()> {
    bail!("cloud authenticated smoke requires a build with server-mode enabled")
}

fn cmd_activate(
    manifest: &Path,
    example: &Path,
    dry_run: bool,
    mut invoke: impl FnMut(&str, &[&str], Option<&[u8]>, Option<&Path>) -> Result<Vec<u8>>,
    mut smoke: impl FnMut(&str, &str) -> Result<()>,
) -> Result<()> {
    let m = action_manifest(manifest, example, dry_run)?;
    validate_storage_paths(&m.storage)?;
    let store = expand_path(&m.storage.tokens_map_path)?;
    let client = expand_path(&m.storage.client_token_path)?;
    let pending = pending_client_path(&client)?;
    let _lock = if dry_run {
        None
    } else {
        Some(lock_token_map(&store)?)
    };
    let map = read_token_map(&store)?;
    assert_no_onsite_reuse(&map, onsite_hub_token_fp()?.as_deref())?;
    let token = fs::read_to_string(&pending).context("read staged client credential")?;
    let token = token.trim();
    let value = map
        .get(token)
        .context("staged credential is absent from the map")?;
    let meta: TokenMeta = serde_json::from_value(value.clone())
        .map_err(|_error| anyhow::anyhow!("invalid staged credential metadata"))?;
    if meta.role != m.storage.client_token_role
        || meta.agent != m.storage.client_token_agent
        || meta.host != m.storage.client_token_host
    {
        bail!("staged credential does not match the selected client identity");
    }
    if dry_run {
        println!("DRY: would verify recovery, authenticate staged credential, then publish client");
        return Ok(());
    }
    verify_recovery(&m, &map, &mut invoke)?;
    smoke(&m.cloud_base_url, token)?;
    write_secret_file(&client, token)?;
    fs::remove_file(&pending).context("remove activated staging credential")?;
    println!(
        "Activated verified replacement; previous credentials remain valid until explicit revocation."
    );
    Ok(())
}

#[cfg(feature = "server-mode")]
async fn smoke_http(base: &str, token: &str) -> Result<()> {
    let base = base.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build HTTP client")?;
    let response = client
        .get(format!("{base}/health"))
        .send()
        .await
        .context("GET /health")?;
    if !response.status().is_success() {
        bail!("unexpected health status {}", response.status().as_u16());
    }
    let health: Value = response.json().await.context("health json")?;
    if health.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("health did not report ok=true");
    }
    let resp = client
        .get(format!("{base}/presence"))
        .bearer_auth(token)
        .send()
        .await
        .context("GET /presence")?;
    let code = resp.status().as_u16();
    println!("Authed GET /presence HTTP {code} (body redacted)");
    if !resp.status().is_success() {
        bail!("unexpected presence status {code}");
    }
    Ok(())
}

fn cmd_wrangler_hint(manifest: &Path, example: &Path) -> Result<()> {
    let m = ensure_manifest(manifest, example)?;
    let store = expand_path(&m.storage.tokens_map_path)?;
    if !store.is_file() {
        bail!("missing token map {}", store.display());
    }
    println!(
        "Run cloud-tokens bw-upsert, then cloud-tokens wrangler-put; upload requires recovery equality plus independently established deployed-map authority; otherwise preserve the opaque deployed secret."
    );
    println!(
        "Worker={} base={}",
        m.worker_name.as_deref().unwrap_or("agentbus-cloud"),
        m.cloud_base_url
    );
    Ok(())
}

fn run_external(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    cwd: Option<&Path>,
) -> Result<Vec<u8>> {
    run_external_with_budgets(
        program,
        args,
        input,
        cwd,
        std::time::Duration::from_secs(120),
        std::time::Duration::from_secs(2),
    )
}

fn run_external_with_budgets(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    cwd: Option<&Path>,
    deadline: std::time::Duration,
    settlement: std::time::Duration,
) -> Result<Vec<u8>> {
    let started = std::time::Instant::now();
    let program_owned = program.to_owned();
    let args_owned: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let input = input.map(<[u8]>::to_vec);
    let cwd = cwd.map(Path::to_path_buf);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::sync_channel(1);
    // A dedicated owned worker permits both entered and executing caller
    // runtimes. Never join it indefinitely or target another process/thread.
    let _worker = std::thread::Builder::new()
        .name("cloud-token-external".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("build private external-tool runtime")
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _delivered = sender.send(Err(error));
                    let _reported = shutdown_sender.send(());
                    return;
                }
            };
            let remaining = deadline.saturating_sub(started.elapsed());
            let args: Vec<_> = args_owned.iter().map(String::as_str).collect();
            // Reserve coordination inside the same settlement budget. Native
            // settlement must not consume the receiver's entire final window.
            let reserve = settlement.min(std::time::Duration::from_millis(50));
            let native_settlement = settlement.saturating_sub(reserve);
            let result = if remaining.is_zero() {
                Err(anyhow::anyhow!("external deadline expired before launch"))
            } else {
                runtime.block_on(run_external_bounded(
                    &program_owned,
                    &args,
                    input.as_deref(),
                    cwd.as_deref(),
                    remaining,
                    native_settlement,
                ))
            };
            // Publish the native child/pipe result independently of runtime
            // shutdown. Held handles must not replace its known failure.
            let _delivered = sender.send(result);
            let remaining = (deadline + settlement).saturating_sub(started.elapsed());
            // Return from bounded shutdown does not prove every detached
            // blocking worker exited. Success already requires finished pipes.
            runtime.shutdown_timeout(remaining.min(settlement));
            let _reported = shutdown_sender.send(());
        })
        .context("start bounded external-tool worker")?;
    match receiver.recv_timeout((deadline + settlement).saturating_sub(started.elapsed())) {
        Ok(result) => {
            let remaining = (deadline + settlement).saturating_sub(started.elapsed());
            let shutdown_returned = shutdown_receiver.recv_timeout(remaining).is_ok();
            match result {
                Ok(output) if shutdown_returned => Ok(output),
                Ok(_output) => bail!(
                    "{program} native child and pipes completed; runtime_cleanup=UNSETTLED; do not retry; external output is redacted"
                ),
                Err(error) if shutdown_returned => Err(error),
                Err(error) => Err(error.context(
                    "runtime_cleanup=UNSETTLED; native child/pipe failure retained; external output is redacted",
                )),
            }
        }
        Err(_error) => bail!(
            "{program} worker deadline/failure; command/mutation outcome NOT_ESTABLISHED; original worker may remain unsettled; do not retry; external output is redacted"
        ),
    }
}
async fn drain_external_pipes(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    stdin: Option<tokio::process::ChildStdin>,
    input: Option<&[u8]>,
    finished: &std::cell::Cell<bool>,
) -> Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let read_stdout = async {
        let mut bytes = Vec::new();
        stdout
            .context("external stdout unavailable")?
            .read_to_end(&mut bytes)
            .await?;
        Ok::<_, anyhow::Error>(bytes)
    };
    let read_stderr = async {
        let mut bytes = Vec::new();
        stderr
            .context("external stderr unavailable")?
            .read_to_end(&mut bytes)
            .await?;
        Ok::<_, anyhow::Error>(bytes)
    };
    let write_stdin = async {
        if let Some(input) = input {
            let mut stdin = stdin.context("external stdin unavailable")?;
            stdin.write_all(input).await?;
            stdin.shutdown().await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let (stdout, stderr, stdin) = tokio::join!(read_stdout, read_stderr, write_stdin);
    finished.set(true);
    let stdout = stdout?;
    stderr?;
    stdin?;
    Ok(stdout)
}

async fn run_external_bounded(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    cwd: Option<&Path>,
    deadline: std::time::Duration,
    settlement: std::time::Duration,
) -> Result<Vec<u8>> {
    use std::cell::Cell;
    use std::process::Stdio;

    // Count synchronous process launch inside the existing operation budget.
    let expires = tokio::time::Instant::now() + deadline;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let pipes_finished = Cell::new(false);
    let pipes = drain_external_pipes(
        child.stdout.take(),
        child.stderr.take(),
        child.stdin.take(),
        input,
        &pipes_finished,
    );
    tokio::pin!(pipes);
    let result = tokio::time::timeout_at(expires, async {
        let (status, output) = tokio::join!(child.wait(), &mut pipes);
        Ok::<_, anyhow::Error>((status?, output?))
    })
    .await;
    let failure = match result {
        Ok(Ok((status, output))) if status.success() => return Ok(output),
        Ok(Ok((_status, _output))) => {
            bail!("{program} failed; external output is redacted");
        }
        Ok(Err(_error)) => "process or pipe failure",
        Err(_deadline) => "deadline expired",
    };
    // Only the original Child handle is targeted, never a PID lookup/tree.
    // No implicit kill-on-drop or retry is used after this single request.
    let termination = match child.try_wait() {
        Ok(Some(_)) => "already exited",
        Ok(None) | Err(_) => {
            if child.start_kill().is_ok() {
                "requested for original child"
            } else {
                "request failed; outcome unknown"
            }
        }
    };
    let settled = tokio::time::timeout(settlement, async {
        let drain = async {
            if !pipes_finished.get() {
                let _output = (&mut pipes).await;
            }
        };
        let (status, ()) = tokio::join!(child.wait(), drain);
        status
    })
    .await;
    let settled = matches!(settled, Ok(Ok(_))) && pipes_finished.get();
    bail!(
        "{program} {failure}; command/mutation outcome NOT_ESTABLISHED; original termination {termination}; pipes settled={settled}; external output is redacted"
    );
}

fn require_unlocked_bw(
    invoke: &mut impl FnMut(&str, &[&str], Option<&[u8]>, Option<&Path>) -> Result<Vec<u8>>,
) -> Result<()> {
    let raw = invoke("bw", &["status"], None, None)?;
    let status: Value = serde_json::from_slice(&raw).context("parse Bitwarden status")?;
    if status.get("status").and_then(Value::as_str) != Some("unlocked") {
        bail!("unlock Bitwarden before recovery or upload");
    }
    Ok(())
}

fn verify_recovery(
    m: &Manifest,
    map: &BTreeMap<String, Value>,
    invoke: &mut impl FnMut(&str, &[&str], Option<&[u8]>, Option<&Path>) -> Result<Vec<u8>>,
) -> Result<()> {
    require_unlocked_bw(invoke)?;
    let raw = invoke("bw", &["get", "item", &m.bitwarden.item_name], None, None)?;
    let item: Value = serde_json::from_slice(&raw).context("parse Bitwarden recovery item")?;
    let recovered = recovered_token_map(&item)?;
    if &recovered != map {
        bail!("Bitwarden recovery differs from the local map; upload refused");
    }
    Ok(())
}

fn canonical_metadata(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted: BTreeMap<_, _> = object
                .into_iter()
                .map(|(key, value)| (key, canonical_metadata(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(array) => Value::Array(array.into_iter().map(canonical_metadata).collect()),
        other => other,
    }
}

fn token_map_sha256(map: &BTreeMap<String, Value>) -> Result<String> {
    use sha2::{Digest as _, Sha256};
    let canonical = canonical_metadata(serde_json::to_value(map)?);
    Ok(hex_bytes(&Sha256::digest(serde_json::to_vec(&canonical)?)))
}

fn authority_baseline(
    m: &Manifest,
    candidate: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>> {
    let evidence = m.deployed_map_authority.as_ref().context(
        "deployed-map authority UNKNOWN; upload refused; preserve the opaque deployed secret",
    )?;
    if evidence.source_kind != "independently-verified-stored-baseline"
        || evidence.authority_reference.trim().is_empty()
        || [&evidence.baseline_sha256, &evidence.candidate_sha256]
            .iter()
            .any(|hash| {
                hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
    {
        bail!("independently established stored-baseline evidence required; upload refused");
    }
    validate_storage_paths(&m.storage)?;
    let baseline = expand_path(&evidence.baseline_path)?;
    let resolved = resolved_storage_path(&baseline)?;
    let map = expand_path(&m.storage.tokens_map_path)?;
    let client = expand_path(&m.storage.client_token_path)?;
    for path in [&map, &client, &pending_client_path(&client)?] {
        if resolved == resolved_storage_path(path)? {
            bail!("authoritative baseline must be distinct from mutable toolkit files");
        }
    }
    let recovered = read_token_map(&baseline)?;
    if token_map_sha256(&recovered)? != evidence.baseline_sha256
        || token_map_sha256(candidate)? != evidence.candidate_sha256
    {
        bail!("stored-baseline or candidate authority binding differs; upload refused");
    }
    Ok(recovered)
}

fn require_deployment_authority(m: &Manifest, candidate: &BTreeMap<String, Value>) -> Result<()> {
    for (token, metadata) in authority_baseline(m, candidate)? {
        if candidate.get(&token) != Some(&metadata) {
            bail!("candidate omits or changes an authoritative baseline entry; upload refused");
        }
    }
    Ok(())
}

fn token_sha256(token: &str) -> String {
    use sha2::{Digest as _, Sha256};
    hex_bytes(&Sha256::digest(token.as_bytes()))
}

fn read_cutover_receipt(selection: &RetiredToken) -> Result<CutoverReceipt> {
    use sha2::{Digest as _, Sha256};
    let path = expand_path(&selection.cutover_receipt_path)?;
    validate_secret_path(&path)?;
    let bytes = fs::read(path).context("read cutover evidence")?;
    if hex_bytes(&Sha256::digest(&bytes)) != selection.cutover_receipt_sha256 {
        bail!("cutover evidence changed; revocation refused");
    }
    serde_json::from_slice(&bytes).map_err(|_error| anyhow::anyhow!("parse cutover evidence"))
}

fn revocation_candidate(
    m: &Manifest,
    baseline: &BTreeMap<String, Value>,
    active_client: &str,
    mut receipt: impl FnMut(&RetiredToken) -> Result<CutoverReceipt>,
) -> Result<BTreeMap<String, Value>> {
    validate_token_map(baseline)?;
    let revocation = m
        .revocation
        .as_ref()
        .context("missing explicit revocation selection")?;
    if revocation.retired.is_empty() || !baseline.contains_key(active_client) {
        bail!("empty selection or active client is not in the baseline");
    }
    let baseline_sha256 = token_map_sha256(baseline)?;
    let mut candidate = baseline.clone();
    let mut selected = std::collections::BTreeSet::new();
    for selection in &revocation.retired {
        if !selected.insert(selection.token_sha256.clone()) {
            bail!("duplicate retired-token selection");
        }
        let identities: Vec<_> = m
            .identities
            .iter()
            .filter(|identity| identity.id == selection.identity_id)
            .collect();
        if identities.len() != 1 {
            bail!("retired identity must identify exactly one manifest identity");
        }
        let identity = identities[0];
        let (token, value) = baseline
            .iter()
            .find(|(token, _)| token_sha256(token) == selection.token_sha256)
            .context("selected retired token is not in the baseline")?;
        if token == active_client {
            bail!("current active client token cannot be revoked");
        }
        let meta: TokenMeta = serde_json::from_value(value.clone())?;
        if !identity_matches(
            &meta,
            &identity.role,
            &identity.agent,
            identity.host.as_deref(),
            identity.hub.as_deref(),
        ) {
            bail!("retired token does not match its named identity");
        }
        let proof = receipt(selection)?;
        let replacement = baseline
            .iter()
            .find(|(key, _)| token_sha256(key) == proof.replacement_token_sha256)
            .context("cutover replacement is not in the authoritative baseline")?;
        let replacement_meta: TokenMeta = serde_json::from_value(replacement.1.clone())?;
        if proof.identity_id != selection.identity_id
            || proof.retired_token_sha256 != selection.token_sha256
            || proof.baseline_sha256 != baseline_sha256
            || proof.cloud_base_url != m.cloud_base_url
            || !proof.replacement_authenticated
            || !proof.retired_client_disconnected
            || proof.evidence_reference.trim().is_empty()
            || replacement.0 == token
            || !identity_matches(
                &replacement_meta,
                &identity.role,
                &identity.agent,
                identity.host.as_deref(),
                identity.hub.as_deref(),
            )
        {
            bail!("cutover evidence is incomplete or has a different binding");
        }
        candidate.remove(token);
    }
    validate_token_map(&candidate)?;
    for role in ["operator", "hub"] {
        if !candidate
            .values()
            .any(|value| value.get("role").and_then(Value::as_str) == Some(role))
        {
            bail!("revocation would leave no operator or hub token");
        }
    }
    // A replacement selected for retirement elsewhere is not a cutover survivor.
    for selection in &revocation.retired {
        let proof = receipt(selection)?;
        if !candidate
            .keys()
            .any(|token| token_sha256(token) == proof.replacement_token_sha256)
        {
            bail!("cutover replacement must survive the complete selection");
        }
    }
    Ok(candidate)
}

fn require_revocation_authority(
    m: &Manifest,
    candidate: &BTreeMap<String, Value>,
    active_client: &str,
) -> Result<BTreeMap<String, Value>> {
    let baseline = authority_baseline(m, candidate)?;
    require_revocation_shape(m, &baseline, candidate, active_client, read_cutover_receipt)?;
    Ok(baseline)
}

fn require_revocation_shape(
    m: &Manifest,
    baseline: &BTreeMap<String, Value>,
    candidate: &BTreeMap<String, Value>,
    active_client: &str,
    receipt: impl FnMut(&RetiredToken) -> Result<CutoverReceipt>,
) -> Result<()> {
    if &revocation_candidate(m, baseline, active_client, receipt)? != candidate {
        bail!("revocation candidate must be exactly baseline minus the selected retired tokens");
    }
    Ok(())
}

fn require_revocation_lease(m: &Manifest, remaining_seconds: i64) -> Result<()> {
    use sha2::{Digest as _, Sha256};
    let revocation = m
        .revocation
        .as_ref()
        .context("missing revocation selection")?;
    let path = expand_path(&revocation.exclusive_lease_path)?;
    validate_secret_path(&path)?;
    let bytes = fs::read(path).context("read exclusive mutation lease")?;
    if hex_bytes(&Sha256::digest(&bytes)) != revocation.exclusive_lease_sha256 {
        bail!("exclusive mutation lease changed");
    }
    let lease: RevocationLease = serde_json::from_slice(&bytes)
        .map_err(|_error| anyhow::anyhow!("parse exclusive mutation lease"))?;
    let expected_resource = format!(
        "cloudflare-worker:{}:AGENT_BUS_TOKENS",
        m.worker_name.as_deref().unwrap_or("agentbus-cloud")
    );
    let expires = chrono::DateTime::parse_from_rfc3339(&lease.expires_at_utc)
        .map_err(|_error| anyhow::anyhow!("invalid mutation lease expiry"))?;
    if lease.status != "GRANTED"
        || lease.owner.trim().is_empty()
        || lease.evidence_reference.trim().is_empty()
        || lease.resource != expected_resource
        || (expires.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds()
            < remaining_seconds
    {
        bail!("exclusive mutation lease is missing or has insufficient remaining time");
    }
    Ok(())
}

#[cfg(any(feature = "server-mode", test))]
fn validate_live_token_manifest(value: &Value, raw_sha256: &str, count: usize) -> Result<()> {
    if value.get("representation").and_then(Value::as_str) != Some("utf8-secret-binding-v1")
        || value.get("sha256").and_then(Value::as_str) != Some(raw_sha256)
        || value.get("entry_count").and_then(Value::as_u64) != u64::try_from(count).ok()
    {
        bail!("live production token manifest differs; mutation/readback not verified");
    }
    Ok(())
}

#[cfg(any(feature = "server-mode", test))]
const MANIFEST_BODY_LIMIT: usize = 65_536;

#[cfg(any(feature = "server-mode", test))]
fn checked_manifest_content_length(length: Option<u64>) -> Result<()> {
    if length.is_some_and(|length| length > MANIFEST_BODY_LIMIT as u64) {
        bail!("production manifest response exceeds the bounded body limit");
    }
    Ok(())
}

#[cfg(any(feature = "server-mode", test))]
fn checked_manifest_body_size(current: usize, additional: usize) -> Result<usize> {
    current
        .checked_add(additional)
        .filter(|size| *size <= MANIFEST_BODY_LIMIT)
        .context("production manifest response exceeds the bounded body limit")
}

#[cfg(feature = "server-mode")]
async fn read_live_token_manifest(mut response: reqwest::Response) -> Result<Value> {
    checked_manifest_content_length(response.content_length())?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_error| anyhow::anyhow!("read production manifest body"))?
    {
        checked_manifest_body_size(bytes.len(), chunk.len())?;
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_error| anyhow::anyhow!("parse production manifest"))
}

#[cfg(feature = "server-mode")]
fn verify_live_revocation(
    m: &Manifest,
    baseline: &BTreeMap<String, Value>,
    candidate: &BTreeMap<String, Value>,
    phase: RevocationPhase,
) -> Result<()> {
    use sha2::{Digest as _, Sha256};
    let base = reqwest::Url::parse(&m.cloud_base_url)
        .map_err(|_error| anyhow::anyhow!("invalid cloud URL"))?;
    if !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || base.scheme() != "https"
    {
        bail!("live revocation requires a credential-free HTTPS cloud URL");
    }
    let authority = m
        .deployed_map_authority
        .as_ref()
        .context("authority missing")?;
    let bytes = if phase == RevocationPhase::BeforeUpload {
        let bytes = fs::read(expand_path(&authority.baseline_path)?)?;
        let captured: BTreeMap<String, Value> = serde_json::from_slice(&bytes)
            .map_err(|_error| anyhow::anyhow!("parse production proof baseline"))?;
        if &captured != baseline {
            bail!("production proof baseline changed");
        }
        bytes
    } else {
        // Exactly the bytes passed to Wrangler, not the canonical authority hash.
        serde_json::to_vec(candidate)?
    };
    let expected_hash = hex_bytes(&Sha256::digest(&bytes));
    let count = if phase == RevocationPhase::BeforeUpload {
        baseline.len()
    } else {
        candidate.len()
    };
    let operator = candidate
        .iter()
        .find(|(_, value)| value.get("role").and_then(Value::as_str) == Some("operator"))
        .context("no surviving operator for manifest readback")?
        .0
        .clone();
    let mut replacements = Vec::new();
    let mut retired = Vec::new();
    for selection in &m.revocation.as_ref().context("missing revocation")?.retired {
        let proof = read_cutover_receipt(selection)?;
        replacements.push(
            candidate
                .keys()
                .find(|token| token_sha256(token) == proof.replacement_token_sha256)
                .context("replacement must survive")?
                .clone(),
        );
        retired.push(
            baseline
                .keys()
                .find(|token| token_sha256(token) == selection.token_sha256)
                .context("retired baseline entry missing")?
                .clone(),
        );
    }
    let base = base.as_str().trim_end_matches('/').to_owned();
    run_live_revocation_readback(LiveRevocationReadback {
        base,
        operator,
        replacements,
        retired,
        expected_hash,
        count,
        phase,
    })
}

#[cfg(feature = "server-mode")]
struct LiveRevocationReadback {
    base: String,
    operator: String,
    replacements: Vec<String>,
    retired: Vec<String>,
    expected_hash: String,
    count: usize,
    phase: RevocationPhase,
}

#[cfg(feature = "server-mode")]
async fn live_revocation_requests(
    readback: LiveRevocationReadback,
    budget: std::time::Duration,
) -> Result<()> {
    let LiveRevocationReadback {
        base,
        operator,
        replacements,
        retired,
        expected_hash,
        count,
        phase,
    } = readback;
    let client = reqwest::Client::builder()
        .timeout(budget)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_error| anyhow::anyhow!("build revocation HTTP client"))?;
    for token in &replacements {
        let response = client
            .get(format!("{base}/presence"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_error| anyhow::anyhow!("replacement authentication transport failed"))?;
        if !response.status().is_success() {
            bail!("replacement is not currently authenticated");
        }
    }
    if phase == RevocationPhase::AfterUpload {
        for token in &retired {
            let response = client
                .get(format!("{base}/presence"))
                .bearer_auth(token)
                .send()
                .await
                .map_err(|_error| {
                    anyhow::anyhow!("retired credential readback transport failed")
                })?;
            if response.status() != reqwest::StatusCode::UNAUTHORIZED {
                bail!("retired credential rejection is not verified");
            }
        }
    }
    // Last request before secret-put is the fresh, operator-only raw binding.
    let response = client
        .get(format!("{base}/admin/tokens/manifest"))
        .bearer_auth(&operator)
        .send()
        .await
        .map_err(|_error| anyhow::anyhow!("production manifest transport failed"))?;
    if !response.status().is_success() {
        bail!("operator manifest readback denied");
    }
    let value = read_live_token_manifest(response).await?;
    validate_live_token_manifest(&value, &expected_hash, count)
}

#[cfg(feature = "server-mode")]
fn run_live_revocation_readback(readback: LiveRevocationReadback) -> Result<()> {
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(20);
    let settlement = std::time::Duration::from_secs(2);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let (done, cleanup) = std::sync::mpsc::sync_channel(1);
    let _worker = std::thread::Builder::new()
        .name("cloud-token-revocation-readback".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_error) => {
                    let _ = sender.send(Err(anyhow::anyhow!("build readback runtime")));
                    let _ = done.send(());
                    return;
                }
            };
            let result = runtime.block_on(async {
                tokio::time::timeout(
                    budget.saturating_sub(started.elapsed()),
                    live_revocation_requests(readback, budget),
                )
                .await
            });
            let result = if started.elapsed() >= budget {
                Err(anyhow::anyhow!("live revocation readback deadline"))
            } else {
                result.unwrap_or_else(|_deadline| {
                    Err(anyhow::anyhow!("live revocation readback deadline"))
                })
            };
            let _ = sender.send(result);
            runtime.shutdown_timeout((budget + settlement).saturating_sub(started.elapsed()));
            let _ = done.send(());
        })
        .context("start owned revocation readback worker")?;
    let result = receiver
        .recv_timeout((budget + settlement).saturating_sub(started.elapsed()))
        .map_err(|_error| {
            anyhow::anyhow!("revocation readback unsettled; do not retry mutation")
        })?;
    if cleanup
        .recv_timeout((budget + settlement).saturating_sub(started.elapsed()))
        .is_err()
    {
        return Err(result
            .err()
            .unwrap_or_else(|| anyhow::anyhow!("readback runtime cleanup unsettled")));
    }
    if started.elapsed() >= budget + settlement {
        bail!("revocation readback settlement deadline");
    }
    result
}

#[cfg(not(feature = "server-mode"))]
fn verify_live_revocation(
    _m: &Manifest,
    _baseline: &BTreeMap<String, Value>,
    _candidate: &BTreeMap<String, Value>,
    _phase: RevocationPhase,
) -> Result<()> {
    bail!("live revocation requires server-mode for authenticated manifest readback")
}

fn cmd_revoke(
    manifest: &Path,
    root: Option<&Path>,
    dry_run: bool,
    invoke: impl FnMut(&str, &[&str], Option<&[u8]>, Option<&Path>) -> Result<Vec<u8>>,
    live: impl FnMut(
        &Manifest,
        &BTreeMap<String, Value>,
        &BTreeMap<String, Value>,
        RevocationPhase,
    ) -> Result<()>,
) -> Result<()> {
    #[cfg(not(feature = "server-mode"))]
    if !dry_run {
        bail!("live revocation requires server-mode before any recovery mutation");
    }
    cmd_revoke_with_onsite(manifest, root, dry_run, invoke, live, onsite_hub_token_fp)
}

fn preserve_revocation_rollback(
    m: &Manifest,
    baseline: &BTreeMap<String, Value>,
    store: &Path,
    client: &Path,
) -> Result<()> {
    let backup_dir = expand_path(&m.anti_lockout.backup_dir)?;
    validate_secret_path(&backup_dir)?;
    fs::create_dir_all(&backup_dir)?;
    let rollback = backup_dir.join(format!(
        "revocation-baseline-{}.json",
        token_map_sha256(baseline)?
    ));
    let baseline_path = expand_path(
        &m.deployed_map_authority
            .as_ref()
            .context("authority missing")?
            .baseline_path,
    )?;
    let baseline_bytes = fs::read(&baseline_path)?;
    let backup_map: BTreeMap<String, Value> = serde_json::from_slice(&baseline_bytes)
        .map_err(|_error| anyhow::anyhow!("parse rollback baseline"))?;
    if &backup_map != baseline {
        bail!("authoritative baseline changed before rollback preservation");
    }
    for protected in [store, client, baseline_path.as_path()] {
        if resolved_storage_path(&rollback)? == resolved_storage_path(protected)? {
            bail!("rollback must be distinct from live files and authority baseline");
        }
    }
    if rollback.exists() {
        validate_secret_path(&rollback)?;
        if fs::read(&rollback)? != baseline_bytes {
            bail!("rollback baseline collision");
        }
    } else {
        let mut file = create_private_file(&rollback)?;
        file.write_all(&baseline_bytes)?;
        file.sync_all()?;
    }
    Ok(())
}

fn cmd_revoke_with_onsite(
    manifest: &Path,
    root: Option<&Path>,
    dry_run: bool,
    mut invoke: impl FnMut(&str, &[&str], Option<&[u8]>, Option<&Path>) -> Result<Vec<u8>>,
    mut live: impl FnMut(
        &Manifest,
        &BTreeMap<String, Value>,
        &BTreeMap<String, Value>,
        RevocationPhase,
    ) -> Result<()>,
    onsite: impl FnOnce() -> Result<Option<String>>,
) -> Result<()> {
    use base64::Engine as _;
    // No manifest initialization, locks, provider calls or file writes in preview.
    let m = load_manifest(manifest)?;
    validate_storage_paths(&m.storage)?;
    if !m.anti_lockout.keep_previous_map_backup
        || !m.anti_lockout.require_bw_item_before_wrangler_put
    {
        bail!("revocation requires baseline backup and recovery gates");
    }
    let store = expand_path(&m.storage.tokens_map_path)?;
    let client = expand_path(&m.storage.client_token_path)?;
    let active_client = fs::read_to_string(&client).context("read active client binding")?;
    let active_client = active_client.trim();
    let original = read_token_map(&store)?;
    let candidate = revocation_candidate(&m, &original, active_client, read_cutover_receipt)?;
    let baseline = require_revocation_authority(&m, &candidate, active_client)?;
    if original != baseline {
        bail!("local map must equal the authoritative baseline before revocation");
    }
    let worker = m.worker_name.as_deref().unwrap_or("agentbus-cloud");
    if worker.is_empty()
        || !worker
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        bail!("invalid Worker name");
    }
    let cwd = root.unwrap_or(Path::new(".")).join("cloud/agentbus");
    if !cwd.is_dir() {
        bail!("cloud Worker directory is missing");
    }
    if dry_run {
        println!(
            "DRY: exact retired-token removal validated; no providers invoked or files changed"
        );
        return Ok(());
    }
    assert_no_onsite_reuse(&baseline, onsite()?.as_deref())?;
    require_revocation_lease(&m, 150)?;
    let _lock = lock_token_map(&store)?;
    if read_token_map(&store)? != baseline || fs::read_to_string(&client)?.trim() != active_client {
        bail!("local baseline or active client changed before revocation");
    }
    require_unlocked_bw(&mut invoke)?;
    let raw = invoke("bw", &["get", "item", &m.bitwarden.item_name], None, None)?;
    let mut item: Value = serde_json::from_slice(&raw)
        .map_err(|_error| anyhow::anyhow!("parse Bitwarden recovery item"))?;
    if recovered_token_map(&item)? != baseline {
        bail!("recovery baseline differs; revocation refused");
    }
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("existing recovery item has no id")?
        .to_owned();
    preserve_revocation_rollback(&m, &baseline, &store, &client)?;
    item["notes"] = Value::String(serde_json::to_string(&candidate)?);
    let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&item)?);
    require_revocation_lease(&m, 150)?;
    invoke("bw", &["edit", "item", &id], Some(encoded.as_bytes()), None)
        .context("recovery mutation failed; inspect retained rollback before any retry")?;
    verify_recovery(&m, &candidate, &mut invoke)
        .context("candidate recovery not verified; upload not attempted")?;
    let body = serde_json::to_vec(&candidate)?;
    require_revocation_lease(&m, 150)?;
    live(&m, &baseline, &candidate, RevocationPhase::BeforeUpload)?;
    // Cover the existing 120s provider budget, 2s settlement, and 22s readback.
    require_revocation_lease(&m, 150)?;
    if read_token_map(&store)? != baseline || fs::read_to_string(&client)?.trim() != active_client {
        bail!(
            "local baseline or active client changed before upload; recovery may contain candidate"
        );
    }
    invoke(
        "wrangler",
        &["secret", "put", "AGENT_BUS_TOKENS", "--name", worker],
        Some(&body),
        Some(&cwd),
    )
    .context("upload failed or ambiguous; recovery may contain candidate; do not retry blindly")?;
    require_revocation_lease(&m, 25).context(
        "upload returned success but lease no longer covers readback; reconcile before retry",
    )?;
    live(&m, &baseline, &candidate, RevocationPhase::AfterUpload).context(
        "upload returned success but production revocation readback failed; do not retry blindly",
    )?;
    require_revocation_lease(&m, 1).context(
        "production readback returned but mutation lease expired before local publication",
    )?;
    write_secret_file(&store, &serde_json::to_string_pretty(&candidate)?).context(
        "upload returned success but local map publication failed; reconcile before retry",
    )?;
    require_revocation_lease(&m, 1)
        .context("local map was published but mutation lease expired; reconcile before retry")?;
    println!(
        "Exact retired-token removal and live rejection/readback verified; rollback retained (values redacted)"
    );
    Ok(())
}

fn cmd_upload(
    manifest: &Path,
    example: &Path,
    root: Option<&Path>,
    dry_run: bool,
    mut invoke: impl FnMut(&str, &[&str], Option<&[u8]>, Option<&Path>) -> Result<Vec<u8>>,
) -> Result<()> {
    let m = action_manifest(manifest, example, dry_run)?;
    if !m.anti_lockout.require_bw_item_before_wrangler_put {
        bail!("the recovery upload gate cannot be disabled");
    }
    let store = expand_path(&m.storage.tokens_map_path)?;
    let _lock = if dry_run {
        None
    } else {
        Some(lock_token_map(&store)?)
    };
    let map = read_token_map(&store)?;
    assert_no_onsite_reuse(&map, onsite_hub_token_fp()?.as_deref())?;
    require_deployment_authority(&m, &map)?;
    verify_recovery(&m, &map, &mut invoke)?;
    let worker = m.worker_name.as_deref().unwrap_or("agentbus-cloud");
    if worker.is_empty()
        || !worker
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        bail!("invalid Worker name");
    }
    let cwd = root.unwrap_or(Path::new(".")).join("cloud/agentbus");
    if !cwd.is_dir() {
        bail!("cloud Worker directory is missing");
    }
    if dry_run {
        println!(
            "DRY: local-map recovery equality verified; replacement requires independently established deployed-map authority"
        );
        return Ok(());
    }
    let body = serde_json::to_vec(&map)?;
    invoke(
        "wrangler",
        &["secret", "put", "AGENT_BUS_TOKENS", "--name", worker],
        Some(&body),
        Some(&cwd),
    )?;
    println!(
        "Uploaded local map after recovery equality readback (values redacted); deployed-map completeness is not inferred."
    );
    Ok(())
}

fn cmd_bw_upsert(manifest: &Path, example: &Path, dry_run: bool) -> Result<()> {
    cmd_bw_upsert_with_invoke(manifest, example, dry_run, run_external)
}

fn cmd_bw_upsert_with_invoke(
    manifest: &Path,
    example: &Path,
    dry_run: bool,
    mut invoke: impl FnMut(&str, &[&str], Option<&[u8]>, Option<&Path>) -> Result<Vec<u8>>,
) -> Result<()> {
    use base64::Engine as _;
    let m = action_manifest(manifest, example, dry_run)?;
    let store = expand_path(&m.storage.tokens_map_path)?;
    let _lock = if dry_run {
        None
    } else {
        Some(lock_token_map(&store)?)
    };
    let map = read_token_map(&store)?;
    assert_no_onsite_reuse(&map, onsite_hub_token_fp()?.as_deref())?;
    if dry_run {
        println!(
            "DRY: would store the local map in Bitwarden; deployed-map completeness is not inferred"
        );
        return Ok(());
    }
    require_unlocked_bw(&mut invoke)?;
    let raw = invoke(
        "bw",
        &["list", "items", "--search", &m.bitwarden.item_name],
        None,
        None,
    )?;
    let items: Vec<Value> = serde_json::from_slice(&raw)
        .map_err(|_error| anyhow::anyhow!("parse Bitwarden item search"))?;
    let mut matches: Vec<_> = items
        .into_iter()
        .filter(|item| item.get("name").and_then(Value::as_str) == Some(&m.bitwarden.item_name))
        .collect();
    if matches.len() > 1 {
        bail!("multiple exact Bitwarden recovery items; resolve duplicates before updating");
    }
    let existing = matches.pop();
    if let Some(existing) = &existing {
        require_recovery_entries_preserved(existing, &map)?;
    }
    let id = existing
        .as_ref()
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if existing.is_some() && id.as_deref().is_none_or(str::is_empty) {
        bail!("existing Bitwarden recovery item has no id");
    }
    let mut item = match existing {
        Some(item) => item,
        None => serde_json::from_slice(&invoke("bw", &["get", "template", "item"], None, None)?)
            .context("parse Bitwarden secure-note template")?,
    };
    if !item.is_object() {
        bail!("invalid Bitwarden item object");
    }
    item["type"] = serde_json::json!(2);
    item["name"] = serde_json::json!(m.bitwarden.item_name);
    item["secureNote"] = serde_json::json!({"type": 0});
    item["notes"] = serde_json::json!(serde_json::to_string(&map)?);
    let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&item)?);
    if let Some(id) = id {
        invoke("bw", &["edit", "item", &id], Some(encoded.as_bytes()), None)?;
    } else {
        invoke("bw", &["create", "item"], Some(encoded.as_bytes()), None)?;
    }
    verify_recovery(&m, &map, &mut invoke)?;
    println!(
        "Bitwarden local-map recovery equality verified (values redacted); deployed-map authority remains independently required."
    );
    Ok(())
}

/// Run a `cloud-tokens` operator action.
///
/// # Errors
///
/// Returns an error on I/O, validation, or HTTP failures. Never includes raw
/// token material in error messages intentionally.
pub fn run_cloud_tokens(
    action: CloudTokensAction,
    manifest: Option<&Path>,
    example: Option<&Path>,
    repo_root: Option<&Path>,
    dry_run: bool,
) -> Result<()> {
    let manifest_path = match manifest {
        Some(p) => p.to_path_buf(),
        None => default_manifest_path()?,
    };
    let example_path = match example {
        Some(p) => p.to_path_buf(),
        None => default_example_path(repo_root),
    };

    if dry_run
        && matches!(
            action,
            CloudTokensAction::InitManifest
                | CloudTokensAction::Status
                | CloudTokensAction::Smoke
                | CloudTokensAction::WranglerHint
        )
    {
        let _ = action_manifest(&manifest_path, &example_path, true)?;
        println!("DRY: {action:?}; no files written or HTTP requests sent");
        return Ok(());
    }

    match action {
        CloudTokensAction::InitManifest => cmd_init_manifest(&manifest_path, &example_path),
        CloudTokensAction::Mint => cmd_mint(&manifest_path, &example_path, dry_run),
        CloudTokensAction::Rotate => cmd_mint_mode(&manifest_path, &example_path, dry_run, true),
        CloudTokensAction::Activate => cmd_activate(
            &manifest_path,
            &example_path,
            dry_run,
            run_external,
            authenticated_smoke,
        ),
        CloudTokensAction::WriteClient => cmd_write_client(&manifest_path, &example_path, dry_run),
        CloudTokensAction::Status => cmd_status(&manifest_path, &example_path),
        CloudTokensAction::Smoke => cmd_smoke(&manifest_path, &example_path),
        CloudTokensAction::WranglerHint => cmd_wrangler_hint(&manifest_path, &example_path),
        CloudTokensAction::BwUpsert => cmd_bw_upsert(&manifest_path, &example_path, dry_run),
        CloudTokensAction::WranglerPut => cmd_upload(
            &manifest_path,
            &example_path,
            repo_root,
            dry_run,
            run_external,
        ),
        CloudTokensAction::Revoke => cmd_revoke(
            &manifest_path,
            repo_root,
            dry_run,
            run_external,
            verify_live_revocation,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn revocation_fixture(
        dir: &Path,
    ) -> (PathBuf, Manifest, BTreeMap<String, Value>, CutoverReceipt) {
        use sha2::{Digest as _, Sha256};
        let (manifest, example) = sample_manifest(dir);
        let mut value: Value = serde_json::from_slice(&fs::read(&example).unwrap()).unwrap();
        let old = "a".repeat(64);
        let replacement = "b".repeat(64);
        let baseline = BTreeMap::from([
            (
                old.clone(),
                serde_json::json!({"role":"agent", "agent":"warp-oz-p1gen7", "host":"dtm-p1gen7"}),
            ),
            (
                replacement.clone(),
                serde_json::json!({"role":"agent", "agent":"warp-oz-p1gen7", "host":"dtm-p1gen7"}),
            ),
            (
                "c".repeat(64),
                serde_json::json!({"role":"hub", "agent":"asuspro13-sync", "hub":"asuspro13"}),
            ),
            (
                "d".repeat(64),
                serde_json::json!({"role":"operator", "agent":"operator"}),
            ),
            (
                "e".repeat(64),
                serde_json::json!({"role":"agent", "agent":"unknown-to-manifest", "extension":{"nested":[17,true,"retain"]}}),
            ),
        ]);
        let mut candidate = baseline.clone();
        candidate.remove(&old);
        let authority_path = dir.join("independent-baseline.json");
        write_secret_file(
            &authority_path,
            &serde_json::to_string_pretty(&baseline).unwrap(),
        )
        .unwrap();
        write_secret_file(
            &dir.join("tokens.json"),
            &serde_json::to_string(&baseline).unwrap(),
        )
        .unwrap();
        write_secret_file(&dir.join("client.token"), &replacement).unwrap();
        let proof = CutoverReceipt {
            identity_id: "ag".into(),
            retired_token_sha256: token_sha256(&old),
            replacement_token_sha256: token_sha256(&replacement),
            baseline_sha256: token_map_sha256(&baseline).unwrap(),
            cloud_base_url: "https://agentbus.example.test".into(),
            replacement_authenticated: true,
            retired_client_disconnected: true,
            evidence_reference: "isolated fake cutover; not production evidence".into(),
        };
        let receipt_path = dir.join("cutover.json");
        let receipt_bytes = serde_json::to_vec(&proof).unwrap();
        fs::write(&receipt_path, &receipt_bytes).unwrap();
        let lease_path = dir.join("lease.json");
        let lease_bytes = serde_json::to_vec(&serde_json::json!({
            "status":"GRANTED", "owner":"isolated-fake-owner",
            "resource":"cloudflare-worker:agentbus-cloud:AGENT_BUS_TOKENS",
            "expires_at_utc":(chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
            "evidence_reference":"isolated fake lease; not coordination evidence"
        }))
        .unwrap();
        fs::write(&lease_path, &lease_bytes).unwrap();
        value["deployed_map_authority"] = serde_json::json!({
            "source_kind":"independently-verified-stored-baseline",
            "authority_reference":"isolated simulated authority; not production evidence",
            "baseline_path":authority_path,
            "baseline_sha256":token_map_sha256(&baseline).unwrap(),
            "candidate_sha256":token_map_sha256(&candidate).unwrap()
        });
        value["revocation"] = serde_json::json!({
            "retired":[{"identity_id":"ag", "token_sha256":token_sha256(&old),
                "cutover_receipt_path":receipt_path, "cutover_receipt_sha256":hex_bytes(&Sha256::digest(&receipt_bytes))}],
            "exclusive_lease_path":lease_path,
            "exclusive_lease_sha256":hex_bytes(&Sha256::digest(&lease_bytes))
        });
        fs::create_dir_all(dir.join("cloud/agentbus")).unwrap();
        fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        let m = load_manifest(&manifest).unwrap();
        (manifest, m, baseline, proof)
    }

    #[test]
    fn revocation_removes_exact_selection_and_retains_unknown_metadata() {
        let dir = tempdir().unwrap();
        let (_, m, baseline, proof) = revocation_fixture(dir.path());
        let candidate =
            revocation_candidate(&m, &baseline, &"b".repeat(64), |_| Ok(proof.clone())).unwrap();
        let mut expected = baseline.clone();
        expected.remove(&"a".repeat(64));
        assert_eq!(candidate, expected);
        assert_eq!(
            candidate.get(&"e".repeat(64)),
            baseline.get(&"e".repeat(64))
        );
        assert!(require_revocation_authority(&m, &candidate, &"b".repeat(64)).is_ok());
        assert!(
            require_deployment_authority(&m, &candidate).is_err(),
            "additive upload guard must still reject removal"
        );
    }

    #[test]
    fn revocation_rejects_changed_or_omitted_surviving_entries() {
        let dir = tempdir().unwrap();
        let (_, m, baseline, proof) = revocation_fixture(dir.path());
        let candidate =
            revocation_candidate(&m, &baseline, &"b".repeat(64), |_| Ok(proof.clone())).unwrap();
        for scenario in 0..3 {
            let mut changed = candidate.clone();
            match scenario {
                0 => {
                    changed.remove(&"e".repeat(64));
                }
                1 => {
                    changed.get_mut(&"e".repeat(64)).unwrap()["extension"] = Value::Null;
                }
                2 => {
                    changed.insert("f".repeat(64), baseline[&"e".repeat(64)].clone());
                }
                _ => unreachable!(),
            }
            assert!(
                require_revocation_shape(&m, &baseline, &changed, &"b".repeat(64), |_| Ok(
                    proof.clone()
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn revocation_refuses_active_client_and_invalid_cutover_binding() {
        let dir = tempdir().unwrap();
        let (_, m, baseline, proof) = revocation_fixture(dir.path());
        assert!(
            revocation_candidate(&m, &baseline, &"a".repeat(64), |_| Ok(proof.clone())).is_err()
        );
        for scenario in 0..9 {
            let mut changed = proof.clone();
            match scenario {
                0 => changed.replacement_authenticated = false,
                1 => changed.retired_client_disconnected = false,
                2 => changed.baseline_sha256 = "0".repeat(64),
                3 => changed.identity_id = "other".into(),
                4 => changed.retired_token_sha256 = token_sha256(&"e".repeat(64)),
                5 => changed.replacement_token_sha256 = token_sha256(&"a".repeat(64)),
                6 => changed.replacement_token_sha256 = token_sha256(&"d".repeat(64)),
                7 => changed.cloud_base_url = "https://different.example.test".into(),
                8 => changed.evidence_reference.clear(),
                _ => unreachable!(),
            }
            assert!(
                revocation_candidate(&m, &baseline, &"b".repeat(64), |_| Ok(changed.clone()))
                    .is_err()
            );
        }
    }

    #[test]
    fn revocation_refuses_duplicate_or_missing_named_selection() {
        let dir = tempdir().unwrap();
        let (_, m, baseline, proof) = revocation_fixture(dir.path());
        for scenario in 0..4 {
            let mut changed = m.clone();
            match scenario {
                0 => changed.revocation.as_mut().unwrap().retired.clear(),
                1 => {
                    let selection = changed.revocation.as_ref().unwrap().retired[0].clone();
                    changed.revocation.as_mut().unwrap().retired.push(selection);
                }
                2 => changed.revocation.as_mut().unwrap().retired[0].identity_id = "unknown".into(),
                3 => changed.identities.push(changed.identities[2].clone()),
                _ => unreachable!(),
            }
            assert!(
                revocation_candidate(&changed, &baseline, &"b".repeat(64), |_| Ok(proof.clone()))
                    .is_err()
            );
        }
    }

    #[test]
    fn revocation_refuses_removing_last_operator_or_hub() {
        let dir = tempdir().unwrap();
        let (_, m, original, _) = revocation_fixture(dir.path());
        for (identity_id, first) in [("op", "d".repeat(64)), ("hub", "c".repeat(64))] {
            let second = "f".repeat(64);
            let mut baseline = original.clone();
            baseline.insert(second.clone(), baseline[&first].clone());
            let mut changed = m.clone();
            let selection = changed.revocation.as_ref().unwrap().retired[0].clone();
            changed.revocation.as_mut().unwrap().retired = [first.clone(), second.clone()]
                .into_iter()
                .map(|token| RetiredToken {
                    identity_id: identity_id.into(),
                    token_sha256: token_sha256(&token),
                    ..selection.clone()
                })
                .collect();
            let error = revocation_candidate(&changed, &baseline, &"b".repeat(64), |selected| {
                Ok(CutoverReceipt {
                    identity_id: identity_id.into(),
                    retired_token_sha256: selected.token_sha256.clone(),
                    replacement_token_sha256: token_sha256(
                        if selected.token_sha256 == token_sha256(&first) {
                            &second
                        } else {
                            &first
                        },
                    ),
                    baseline_sha256: token_map_sha256(&baseline).unwrap(),
                    cloud_base_url: changed.cloud_base_url.clone(),
                    replacement_authenticated: true,
                    retired_client_disconnected: true,
                    evidence_reference: "isolated cyclic retirement fixture".into(),
                })
            })
            .unwrap_err();
            assert!(error.to_string().contains("leave no operator or hub"));
        }
    }

    #[test]
    fn revocation_dry_run_changes_no_files_and_invokes_no_tools_or_http() {
        let dir = tempdir().unwrap();
        let (manifest, _, _, _) = revocation_fixture(dir.path());
        let before: BTreeMap<_, _> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_file())
            .map(|path| (path.clone(), fs::read(path).unwrap()))
            .collect();
        cmd_revoke(
            &manifest,
            Some(dir.path()),
            true,
            |_, _, _, _| panic!("dry run must never invoke a provider"),
            |_, _, _, _| panic!("dry run must never send HTTP"),
        )
        .unwrap();
        let after: BTreeMap<_, _> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_file())
            .map(|path| (path.clone(), fs::read(path).unwrap()))
            .collect();
        assert_eq!(after, before);
        assert!(!dir.path().join("backups").exists());
        assert!(
            !pending_client_path(&dir.path().join("client.token"))
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn revocation_refuses_unknown_authority_and_changed_cutover_before_providers() {
        let dir = tempdir().unwrap();
        let (manifest, m, _, _) = revocation_fixture(dir.path());
        fs::write(
            &m.revocation.as_ref().unwrap().retired[0].cutover_receipt_path,
            b"changed",
        )
        .unwrap();
        assert!(
            cmd_revoke(
                &manifest,
                Some(dir.path()),
                false,
                |_, _, _, _| panic!("changed receipt must refuse before providers"),
                |_, _, _, _| panic!("changed receipt must refuse before HTTP")
            )
            .is_err()
        );
        let fresh = tempdir().unwrap();
        let (manifest, _, _, _) = revocation_fixture(fresh.path());
        let mut value: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("deployed_map_authority");
        fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            cmd_revoke(
                &manifest,
                Some(fresh.path()),
                false,
                |_, _, _, _| panic!("UNKNOWN authority must refuse before providers"),
                |_, _, _, _| panic!("UNKNOWN authority must refuse before HTTP")
            )
            .is_err()
        );
    }

    #[test]
    fn live_manifest_binding_is_raw_utf8_not_canonical_authority() {
        use sha2::{Digest as _, Sha256};
        let dir = tempdir().unwrap();
        let (_, _, baseline, _) = revocation_fixture(dir.path());
        let raw = serde_json::to_vec_pretty(&baseline).unwrap();
        let raw_hash = hex_bytes(&Sha256::digest(&raw));
        let canonical_hash = token_map_sha256(&baseline).unwrap();
        assert_ne!(raw_hash, canonical_hash);
        let valid = serde_json::json!({"representation":"utf8-secret-binding-v1", "sha256":raw_hash, "entry_count":baseline.len()});
        assert!(validate_live_token_manifest(&valid, &raw_hash, baseline.len()).is_ok());
        for scenario in 0..4 {
            let mut changed = valid.clone();
            match scenario {
                0 => changed["sha256"] = serde_json::json!(canonical_hash),
                1 => changed["entry_count"] = serde_json::json!(baseline.len() + 1),
                2 => changed["representation"] = serde_json::json!("canonical-map-v1"),
                3 => changed["entry_count"] = serde_json::json!(5.0),
                _ => unreachable!(),
            }
            assert!(validate_live_token_manifest(&changed, &raw_hash, baseline.len()).is_err());
        }
    }

    #[test]
    fn live_manifest_body_limit_refuses_declared_or_streamed_overflow() {
        assert!(checked_manifest_content_length(None).is_ok());
        assert!(checked_manifest_content_length(Some(MANIFEST_BODY_LIMIT as u64)).is_ok());
        assert!(checked_manifest_content_length(Some(MANIFEST_BODY_LIMIT as u64 + 1)).is_err());
        assert_eq!(
            checked_manifest_body_size(0, MANIFEST_BODY_LIMIT).unwrap(),
            MANIFEST_BODY_LIMIT
        );
        assert_eq!(
            checked_manifest_body_size(17, MANIFEST_BODY_LIMIT - 17).unwrap(),
            MANIFEST_BODY_LIMIT
        );
        assert!(checked_manifest_body_size(0, MANIFEST_BODY_LIMIT + 1).is_err());
        assert!(checked_manifest_body_size(MANIFEST_BODY_LIMIT, 1).is_err());
        assert!(checked_manifest_body_size(usize::MAX, 1).is_err());
    }

    #[test]
    fn revocation_lease_rejects_changed_or_expired_mutation_evidence() {
        use sha2::{Digest as _, Sha256};
        let dir = tempdir().unwrap();
        let (_, m, _, _) = revocation_fixture(dir.path());
        assert!(require_revocation_lease(&m, 150).is_ok());
        let revocation = m.revocation.as_ref().unwrap();
        let bytes = fs::read(&revocation.exclusive_lease_path).unwrap();
        for scenario in 0..4 {
            let mut changed = m.clone();
            let mut value: Value = serde_json::from_slice(&bytes).unwrap();
            match scenario {
                0 => {
                    value["expires_at_utc"] = serde_json::json!(
                        (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339()
                    );
                }
                1 => value["status"] = serde_json::json!("RELEASED"),
                2 => value["resource"] = serde_json::json!("different-worker"),
                3 => value["owner"] = serde_json::json!(""),
                _ => unreachable!(),
            }
            let altered = serde_json::to_vec(&value).unwrap();
            fs::write(&revocation.exclusive_lease_path, &altered).unwrap();
            assert!(
                require_revocation_lease(&m, 150).is_err(),
                "changed hash must refuse"
            );
            changed.revocation.as_mut().unwrap().exclusive_lease_sha256 =
                hex_bytes(&Sha256::digest(&altered));
            assert!(
                require_revocation_lease(&changed, 150).is_err(),
                "invalid bound lease must refuse"
            );
        }
    }

    fn assert_revocation_events(events: &[&str], before_ok: bool) {
        assert_eq!(
            events,
            if before_ok {
                vec!["recovery-edit", "live-before", "upload", "live-after"]
            } else {
                vec!["recovery-edit", "live-before"]
            }
        );
    }

    #[test]
    fn revocation_fake_providers_preserve_rollback_and_require_fresh_readback() {
        use base64::Engine as _;
        for before_ok in [false, true] {
            let dir = tempdir().unwrap();
            let (manifest, m, baseline, proof) = revocation_fixture(dir.path());
            let candidate =
                revocation_candidate(&m, &baseline, &"b".repeat(64), |_| Ok(proof.clone()))
                    .unwrap();
            let baseline_bytes = fs::read(
                m.deployed_map_authority
                    .as_ref()
                    .unwrap()
                    .baseline_path
                    .clone(),
            )
            .unwrap();
            let mut item = serde_json::json!({"id":"fake-existing-item", "notes":serde_json::to_string(&baseline).unwrap(), "fields":[{"name":"retain", "value":"fake-nonsecret"}]});
            let events = std::cell::RefCell::new(Vec::new());
            let result = cmd_revoke_with_onsite(
                &manifest,
                Some(dir.path()),
                false,
                |program, args, input, cwd| {
                    assert!(
                        !args.iter().any(|arg| baseline.contains_key(*arg)),
                        "tokens must not enter argv"
                    );
                    match (program, args) {
                        ("bw", ["status"]) => Ok(br#"{"status":"unlocked"}"#.to_vec()),
                        ("bw", ["get", "item", _]) => Ok(serde_json::to_vec(&item).unwrap()),
                        ("bw", ["edit", "item", "fake-existing-item"]) => {
                            events.borrow_mut().push("recovery-edit");
                            item = serde_json::from_slice(
                                &base64::engine::general_purpose::STANDARD
                                    .decode(input.unwrap())
                                    .unwrap(),
                            )
                            .unwrap();
                            assert_eq!(
                                item["fields"],
                                serde_json::json!([{"name":"retain", "value":"fake-nonsecret"}])
                            );
                            assert_eq!(recovered_token_map(&item).unwrap(), candidate);
                            Ok(Vec::new())
                        }
                        (
                            "wrangler",
                            [
                                "secret",
                                "put",
                                "AGENT_BUS_TOKENS",
                                "--name",
                                "agentbus-cloud",
                            ],
                        ) => {
                            assert!(events.borrow().contains(&"live-before"));
                            assert!(before_ok, "failed fresh readback must prevent upload");
                            assert_eq!(cwd.unwrap(), dir.path().join("cloud/agentbus"));
                            assert_eq!(input.unwrap(), serde_json::to_vec(&candidate).unwrap());
                            events.borrow_mut().push("upload");
                            Ok(Vec::new())
                        }
                        _ => panic!("unexpected fake provider invocation"),
                    }
                },
                |_, observed_baseline, observed_candidate, phase| {
                    assert_eq!(observed_baseline, &baseline);
                    assert_eq!(observed_candidate, &candidate);
                    match phase {
                        RevocationPhase::BeforeUpload => {
                            events.borrow_mut().push("live-before");
                            if !before_ok {
                                bail!("isolated fake raw-binding mismatch");
                            }
                        }
                        RevocationPhase::AfterUpload => {
                            assert!(events.borrow().contains(&"upload"));
                            events.borrow_mut().push("live-after");
                        }
                    }
                    Ok(())
                },
                || Ok(None),
            );
            let rollback = dir.path().join("backups").join(format!(
                "revocation-baseline-{}.json",
                token_map_sha256(&baseline).unwrap()
            ));
            assert_eq!(fs::read(rollback).unwrap(), baseline_bytes);
            assert_eq!(result.is_ok(), before_ok);
            assert_eq!(
                read_token_map(&dir.path().join("tokens.json")).unwrap(),
                if before_ok { candidate } else { baseline }
            );
            assert_revocation_events(&events.borrow(), before_ok);
        }
    }

    #[test]
    fn revocation_dry_run_never_reads_onsite_configuration() {
        let dir = tempdir().unwrap();
        let (manifest, _, _, _) = revocation_fixture(dir.path());
        cmd_revoke_with_onsite(
            &manifest,
            Some(dir.path()),
            true,
            |_, _, _, _| panic!("dry-run provider"),
            |_, _, _, _| panic!("dry-run HTTP"),
            || panic!("dry-run onsite configuration read"),
        )
        .unwrap();
    }

    fn sample_manifest(dir: &Path) -> (PathBuf, PathBuf) {
        let example = dir.join("example.json");
        let store = dir.join("tokens.json");
        let client = dir.join("client.token");
        let backup = dir.join("backups");
        let body = serde_json::json!({
            "schema_version": 1,
            "cloud_base_url": "https://agentbus.example.test",
            "worker_name": "agentbus-cloud",
            "storage": {
                "tokens_map_path": store.to_string_lossy(),
                "client_token_path": client.to_string_lossy(),
                "client_token_role": "agent",
                "client_token_agent": "warp-oz-p1gen7",
                "client_token_host": "dtm-p1gen7"
            },
            "anti_lockout": {
                "keep_previous_map_backup": true,
                "backup_dir": backup.to_string_lossy()
            },
            "identities": [
                {"id": "hub", "role": "hub", "agent": "asuspro13-sync", "hub": "asuspro13"},
                {"id": "op", "role": "operator", "agent": "operator"},
                {"id": "ag", "role": "agent", "agent": "warp-oz-p1gen7", "host": "dtm-p1gen7"}
            ]
        });
        fs::write(&example, serde_json::to_string_pretty(&body).unwrap()).unwrap();
        let manifest = dir.join("manifest.json");
        (manifest, example)
    }

    fn simulated_upload_authority(manifest: &Path, map: &BTreeMap<String, Value>) {
        // Explicit isolated-test attestation, never called by operator actions.
        let baseline = manifest.with_file_name("authoritative-fixture-baseline.json");
        write_secret_file(&baseline, &serde_json::to_string(map).unwrap()).unwrap();
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(manifest).unwrap()).unwrap();
        value["deployed_map_authority"] = serde_json::json!({
            "source_kind":"independently-verified-stored-baseline",
            "authority_reference":"isolated test fixture, not production evidence",
            "baseline_path":baseline,
            "baseline_sha256":token_map_sha256(map).unwrap(),
            "candidate_sha256":token_map_sha256(map).unwrap()
        });
        fs::write(manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    #[test]
    fn unknown_or_changed_authority_never_invokes_recovery_or_upload_tools() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_mint(&manifest, &example, false).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let store = expand_path(&m.storage.tokens_map_path).unwrap();
        let map = read_token_map(&store).unwrap();
        assert!(
            load_manifest(&manifest)
                .unwrap()
                .deployed_map_authority
                .is_none(),
            "mint must never stamp authority evidence"
        );
        for dry_run in [false, true] {
            assert!(
                cmd_upload(
                    &manifest,
                    &example,
                    Some(dir.path()),
                    dry_run,
                    |_, _, _, _| {
                        panic!("unknown authority must refuse before invoking external tools")
                    }
                )
                .is_err()
            );
        }
        simulated_upload_authority(&manifest, &map);
        let original: Value =
            serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
        let baseline = manifest.with_file_name("authoritative-fixture-baseline.json");
        for scenario in 0..6 {
            let mut value = original.clone();
            match scenario {
                0 => {
                    value["deployed_map_authority"]["candidate_sha256"] =
                        serde_json::json!("0".repeat(64));
                }
                1 => {
                    value["deployed_map_authority"]["baseline_sha256"] =
                        serde_json::json!("0".repeat(64));
                }
                2 => value["deployed_map_authority"]["baseline_path"] = serde_json::json!(store),
                3 => {
                    value["deployed_map_authority"]["baseline_path"] =
                        serde_json::json!(dir.path().join("absent.json"));
                }
                4 => {
                    value["deployed_map_authority"]["source_kind"] =
                        serde_json::json!("newly-minted-map");
                }
                5 => value["deployed_map_authority"]["authority_reference"] = serde_json::json!(""),
                _ => unreachable!(),
            }
            fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(
                cmd_upload(
                    &manifest,
                    &example,
                    Some(dir.path()),
                    false,
                    |_, _, _, _| { panic!("invalid evidence must refuse before tools") }
                )
                .is_err()
            );
        }
        fs::write(&manifest, serde_json::to_vec(&original).unwrap()).unwrap();
        let mut changed = map.clone();
        let metadata = changed.values_mut().next().unwrap();
        metadata["extension"] = serde_json::json!("changed");
        write_secret_file(&store, &serde_json::to_string(&changed).unwrap()).unwrap();
        assert!(
            require_deployment_authority(&load_manifest(&manifest).unwrap(), &changed).is_err()
        );
        let mut rebound = original;
        rebound["deployed_map_authority"]["candidate_sha256"] =
            serde_json::json!(token_map_sha256(&changed).unwrap());
        fs::write(&manifest, serde_json::to_vec(&rebound).unwrap()).unwrap();
        assert!(
            require_deployment_authority(&load_manifest(&manifest).unwrap(), &changed).is_err(),
            "rebinding the candidate hash must not allow baseline metadata loss"
        );
        assert!(baseline.is_file());
    }

    #[test]
    fn empty_operational_maps_fail_before_recovery_or_upload() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_init_manifest(&manifest, &example).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let store = expand_path(&m.storage.tokens_map_path).unwrap();
        write_secret_file(&store, "{}").unwrap();
        let before = fs::read(&store).unwrap();
        assert!(read_token_map(&store).is_err());
        assert!(cmd_mint(&manifest, &example, false).is_err());
        assert!(
            cmd_bw_upsert_with_invoke(&manifest, &example, false, |_, _, _, _| {
                panic!("empty map must fail before invoking Bitwarden")
            })
            .is_err()
        );
        assert!(
            cmd_upload(
                &manifest,
                &example,
                Some(dir.path()),
                false,
                |_, _, _, _| { panic!("empty map must fail before recovery or upload") }
            )
            .is_err()
        );
        assert_eq!(fs::read(store).unwrap(), before);
    }

    #[test]
    fn existing_recovery_baselines_cannot_be_destroyed_by_upsert() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_init_manifest(&manifest, &example).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let token = "a".repeat(64);
        let metadata =
            serde_json::json!({"agent":"fixture", "role":"agent", "extension":{"preserve":17}});
        let candidate = BTreeMap::from([(token.clone(), metadata.clone())]);
        write_secret_file(
            &expand_path(&m.storage.tokens_map_path).unwrap(),
            &serde_json::to_string(&candidate).unwrap(),
        )
        .unwrap();
        let foreign = serde_json::to_string(&serde_json::json!({"b".repeat(64):metadata})).unwrap();
        let changed =
            serde_json::to_string(&serde_json::json!({token:{"agent":"fixture", "role":"agent"}}))
                .unwrap();
        let invalid = serde_json::to_string(
            &serde_json::json!({"b".repeat(64):{"agent":"fixture", "role":"invalid"}}),
        )
        .unwrap();
        for notes in [
            None,
            Some("bad-json".to_owned()),
            Some("{}".to_owned()),
            Some(foreign),
            Some(changed),
            Some(invalid),
        ] {
            let mut existing =
                serde_json::json!({"id":"recovery-fixture", "name":m.bitwarden.item_name});
            if let Some(notes) = notes {
                existing["notes"] = serde_json::json!(notes);
            }
            let mut calls = Vec::new();
            let result = cmd_bw_upsert_with_invoke(
                &manifest,
                &example,
                false,
                |program, args, _input, _cwd| {
                    calls.push((program.to_owned(), args[0].to_owned()));
                    match args[0] {
                        "status" => Ok(br#"{"status":"unlocked"}"#.to_vec()),
                        "list" => Ok(serde_json::to_vec(&vec![existing.clone()]).unwrap()),
                        _ => panic!("invalid or incomplete baseline must not be edited"),
                    }
                },
            );
            assert!(result.is_err());
            assert_eq!(
                calls,
                [
                    ("bw".to_owned(), "status".to_owned()),
                    ("bw".to_owned(), "list".to_owned())
                ]
            );
        }
    }

    #[test]
    fn upsert_retains_every_recovered_entry_and_exact_metadata() {
        use base64::Engine as _;
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_init_manifest(&manifest, &example).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let baseline = BTreeMap::from([(
            "b".repeat(64),
            serde_json::json!({"agent":"external-owner", "role":"agent", "extension":{"preserve":17}}),
        )]);
        let mut candidate = baseline.clone();
        candidate.insert(
            "a".repeat(64),
            serde_json::json!({"agent":"new-client", "role":"agent"}),
        );
        write_secret_file(
            &expand_path(&m.storage.tokens_map_path).unwrap(),
            &serde_json::to_string(&candidate).unwrap(),
        )
        .unwrap();
        let existing = serde_json::json!({"id":"recovery-fixture", "name":m.bitwarden.item_name,
            "notes":serde_json::to_string(&baseline).unwrap(), "fields":[{"name":"preserve", "value":"metadata"}]});
        let mut edited = None;
        cmd_bw_upsert_with_invoke(&manifest, &example, false, |program, args, input, _cwd| {
            assert_eq!(program, "bw");
            match args[0] {
                "status" => Ok(br#"{"status":"unlocked"}"#.to_vec()),
                "list" => Ok(serde_json::to_vec(&vec![existing.clone()]).unwrap()),
                "edit" => {
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(input.unwrap())
                        .unwrap();
                    let item: Value = serde_json::from_slice(&raw).unwrap();
                    assert_eq!(recovered_token_map(&item).unwrap(), candidate);
                    assert_eq!(item["fields"], existing["fields"]);
                    edited = Some(item);
                    Ok(Vec::new())
                }
                "get" => Ok(serde_json::to_vec(edited.as_ref().unwrap()).unwrap()),
                _ => panic!("unexpected fake Bitwarden call"),
            }
        })
        .unwrap();
        assert!(edited.is_some());
    }

    #[test]
    #[cfg(unix)]
    fn external_deadline_returns_without_echoing_output_or_retrying() {
        let start = std::time::Instant::now();
        let error = run_external_with_budgets(
            "sh",
            &["-c", "printf dummy-private-output; while :; do :; done"],
            None,
            None,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        let rendered = format!("{error:#}");
        assert!(rendered.contains("deadline"));
        assert!(rendered.contains("NOT_ESTABLISHED"));
        assert!(!rendered.contains("dummy-private-output"));
    }

    #[test]
    #[cfg(unix)]
    fn external_tools_work_in_entered_and_executing_runtime_contexts() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        {
            let _entered = runtime.enter();
            assert_eq!(
                run_external("sh", &["-c", "printf fixture-result"], None, None).unwrap(),
                b"fixture-result"
            );
        };
        runtime.block_on(async {
            assert_eq!(
                run_external("sh", &["-c", "printf fixture-result"], None, None).unwrap(),
                b"fixture-result"
            );
        });
    }

    #[test]
    #[cfg(unix)]
    fn inherited_pipe_after_original_exit_never_causes_an_indefinite_join() {
        let start = std::time::Instant::now();
        // This local disposable fixture's inherited handle expires naturally;
        // no descendant lookup/kill or live service is involved.
        let error = run_external_with_budgets(
            "sh",
            &["-c", "sleep 0.3 & printf dummy-private-output"],
            None,
            None,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        let rendered = format!("{error:#}");
        assert!(rendered.contains("NOT_ESTABLISHED"));
        assert!(!rendered.contains("dummy-private-output"));
    }

    #[cfg(windows)]
    fn windows_fixture_args(mode: &str, marker: Option<&Path>) -> Vec<String> {
        let mut args = vec![
            "--exact".into(),
            "cloud_tokens::tests::external_process_fixture_child".into(),
            "--nocapture".into(),
            "--quiet".into(),
            "--color".into(),
            "never".into(),
            "--test-threads=1".into(),
            "--skip".into(),
            format!("__cloud_fixture_mode={mode}"),
        ];
        if let Some(marker) = marker {
            args.push("--skip".into());
            args.push(format!("__cloud_fixture_marker={}", marker.display()));
        }
        args
    }

    #[test]
    #[cfg(windows)]
    fn external_process_fixture_child() {
        use std::io::{Read as _, Write as _};
        use std::os::windows::process::CommandExt as _;
        let args: Vec<String> = std::env::args().collect();
        let Some(mode) = args
            .iter()
            .find_map(|arg| arg.strip_prefix("__cloud_fixture_mode="))
        else {
            // Ordinary full/backend selections never invoke an operational child.
            return;
        };
        let marker = args
            .iter()
            .find_map(|arg| arg.strip_prefix("__cloud_fixture_marker="));
        match mode {
            "success" => std::io::stdout().write_all(b"fixture-result").unwrap(),
            "failure" => {
                std::io::stdout()
                    .write_all(b"dummy-private-output")
                    .unwrap();
                std::io::stderr().write_all(b"dummy-private-error").unwrap();
                std::io::stdout().flush().unwrap();
                std::io::stderr().flush().unwrap();
                std::process::exit(17);
            }
            "stdin-read" => {
                std::io::stdout().write_all(&vec![b'x'; 131_072]).unwrap();
                std::io::stdout().flush().unwrap();
                let mut input = Vec::new();
                std::io::stdin().read_to_end(&mut input).unwrap();
                write!(std::io::stdout(), "{}", input.len()).unwrap();
            }
            "stdin-block" => std::thread::sleep(std::time::Duration::from_secs(3)),
            "held-parent" => {
                let marker = Path::new(marker.expect("owned child marker required"));
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(windows_fixture_args("held-child", Some(marker)))
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .creation_flags(0x0800_0000) // CREATE_NO_WINDOW, hidden finite child.
                    .spawn()
                    .unwrap();
                drop(child); // Natural child remains finite; never tree/PID-killed.
                std::io::stdout()
                    .write_all(b"dummy-private-output")
                    .unwrap();
            }
            "held-child" => {
                std::thread::sleep(std::time::Duration::from_secs(5));
                fs::write(
                    marker.expect("owned child marker required"),
                    "naturally-finished",
                )
                .unwrap();
            }
            _ => panic!("unexpected fixture mode"),
        }
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        // Exit before libtest's final report; the strict initial header is
        // validated by the parent rather than allowing arbitrary noise.
        std::process::exit(0);
    }

    #[cfg(windows)]
    fn invoke_windows_fixture(mode: &str, input: Option<&[u8]>) -> Result<Vec<u8>> {
        let args = windows_fixture_args(mode, None);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let exe = std::env::current_exe()?;
        let output = run_external_with_budgets(
            exe.to_str()
                .context("fixture executable path must be UTF-8")?,
            &args,
            input,
            None,
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(250),
        )?;
        // Exact one-test libtest preamble; reject unexpected or missing bytes.
        let body = output
            .strip_prefix(b"\nrunning 1 test\n".as_slice())
            .context("unexpected native fixture libtest preamble")?;
        Ok(body.to_vec())
    }

    #[test]
    #[cfg(windows)]
    fn windows_external_success_in_entered_and_executing_runtime_contexts() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        {
            let _entered = runtime.enter();
            assert_eq!(
                invoke_windows_fixture("success", None).unwrap(),
                b"fixture-result"
            );
        };
        runtime.block_on(async {
            assert_eq!(
                invoke_windows_fixture("success", None).unwrap(),
                b"fixture-result"
            );
        });
    }

    #[test]
    #[cfg(windows)]
    fn windows_external_ordinary_failure_redacts_both_output_streams() {
        let error = invoke_windows_fixture("failure", None).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("failed"),
            "redacted helper error: {rendered}"
        );
        assert!(!rendered.contains("dummy-private-output"));
        assert!(!rendered.contains("dummy-private-error"));
        assert!(
            !rendered.contains("NOT_ESTABLISHED"),
            "ordinary failure with EOF has a known native result"
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_external_stdin_backpressure_is_drained_or_bounded() {
        let input = vec![b'q'; 131_072];
        let output = invoke_windows_fixture("stdin-read", Some(&input)).unwrap();
        assert_eq!(&output[..131_072], vec![b'x'; 131_072].as_slice());
        assert_eq!(&output[131_072..], b"131072");
        let args = windows_fixture_args("stdin-block", None);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let exe = std::env::current_exe().unwrap();
        let large_input = vec![b'q'; 4 * 1024 * 1024];
        let started = std::time::Instant::now();
        let error = run_external_with_budgets(
            exe.to_str().unwrap(),
            &args,
            Some(&large_input),
            None,
            std::time::Duration::from_millis(500),
            std::time::Duration::from_millis(250),
        )
        .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert!(format!("{error:#}").contains("NOT_ESTABLISHED"));
    }

    #[test]
    #[cfg(windows)]
    fn windows_original_exit_with_inherited_pipe_has_finite_settlement() {
        let dir = tempdir().unwrap();
        let marker = dir.path().join("natural-child-completion.txt");
        let args = windows_fixture_args("held-parent", Some(&marker));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let exe = std::env::current_exe().unwrap();
        let started = std::time::Instant::now();
        let error = run_external_with_budgets(
            exe.to_str().unwrap(),
            &args,
            None,
            None,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("already exited"),
            "must exercise original exit, not a busy original process: {rendered}"
        );
        assert!(rendered.contains("pipes settled=false"));
        assert!(!rendered.contains("dummy-private-output"));
        let natural_deadline = started + std::time::Duration::from_secs(9);
        while !marker.is_file() && std::time::Instant::now() < natural_deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(
            fs::read_to_string(marker).unwrap(),
            "naturally-finished",
            "no descendant/tree termination is allowed"
        );
    }

    #[test]
    fn mint_write_client_roundtrip_shape() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_init_manifest(&manifest, &example).unwrap();
        cmd_mint(&manifest, &example, false).unwrap();
        cmd_write_client(&manifest, &example, false).unwrap();
        cmd_status(&manifest, &example).unwrap();

        let m = load_manifest(&manifest).unwrap();
        let store = expand_path(&m.storage.tokens_map_path).unwrap();
        let raw = fs::read_to_string(&store).unwrap();
        let map: BTreeMap<String, TokenMeta> = serde_json::from_str(&raw).unwrap();
        assert_eq!(map.len(), 3);
        assert!(map.keys().all(|k| k.len() >= 32));
        assert!(map.values().any(|v| v.role == "hub"));
        assert!(map.values().any(|v| v.role == "operator"));
        assert!(map.values().any(|v| v.role == "agent"));

        // Object shape, not array
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert!(v.is_object());
        assert!(!v.is_array());
    }

    #[test]
    fn refuses_array_shaped_map_on_write_client() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_init_manifest(&manifest, &example).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let store = expand_path(&m.storage.tokens_map_path).unwrap();
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        fs::write(&store, "[{\"role\":\"agent\"}]\n").unwrap();
        let err = cmd_write_client(&manifest, &example, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parse token map") || msg.contains("JSON"),
            "unexpected {msg}"
        );
    }

    #[test]
    fn onsite_reuse_detection() {
        let mut map = BTreeMap::new();
        let tok = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        map.insert(
            tok.to_owned(),
            TokenMeta {
                agent: "x".into(),
                role: "agent".into(),
                host: None,
                hub: None,
            },
        );
        let fp = sha256_hex16(tok);
        assert!(assert_no_onsite_reuse(&map, Some(&fp)).is_err());
        assert!(assert_no_onsite_reuse(&map, Some("deadbeefdeadbeef")).is_ok());
    }

    #[test]
    fn mint_and_rotate_preserve_the_complete_existing_map() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_init_manifest(&manifest, &example).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let store = expand_path(&m.storage.tokens_map_path).unwrap();
        let legacy_key = "b".repeat(64);
        let legacy = serde_json::json!({"agent":"external-owner", "role":"agent", "host":"dtm-carbon-two", "extension":{"preserve":17}});
        let old = BTreeMap::from([(legacy_key.clone(), legacy.clone())]);
        write_secret_file(&store, &serde_json::to_string(&old).unwrap()).unwrap();
        cmd_mint(&manifest, &example, false).unwrap();
        let minted = read_token_map(&store).unwrap();
        assert_eq!(minted.len(), 4);
        assert_eq!(minted[&legacy_key], legacy);
        cmd_mint(&manifest, &example, false).unwrap();
        assert_eq!(
            read_token_map(&store).unwrap(),
            minted,
            "mint must be additive and idempotent"
        );
        cmd_write_client(&manifest, &example, false).unwrap();
        let client = expand_path(&m.storage.client_token_path).unwrap();
        let old_client = fs::read_to_string(&client).unwrap();
        cmd_mint_mode(&manifest, &example, false, true).unwrap();
        let rotated = read_token_map(&store).unwrap();
        assert_eq!(rotated.len(), 5);
        for (key, value) in &minted {
            assert_eq!(&rotated[key], value);
        }
        assert_eq!(fs::read_to_string(&client).unwrap(), old_client);
        let new_client = fs::read_to_string(pending_client_path(&client).unwrap()).unwrap();
        assert_ne!(new_client, old_client);
        assert!(rotated.contains_key(old_client.trim()));
        assert!(rotated.contains_key(new_client.trim()));
        cmd_write_client(&manifest, &example, false).unwrap();
        assert_eq!(fs::read_to_string(client).unwrap(), old_client);
        for entry in fs::read_dir(expand_path(&m.anti_lockout.backup_dir).unwrap()).unwrap() {
            let path = entry.unwrap().path();
            let stem = path.file_stem().unwrap().to_str().unwrap();
            assert!(uuid::Uuid::parse_str(stem.strip_prefix("cloud-tokens-").unwrap()).is_ok());
            let _: BTreeMap<String, Value> = read_token_map(&path).unwrap();
        }
    }

    #[test]
    fn selected_host_never_uses_an_unbound_token() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_init_manifest(&manifest, &example).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let map = BTreeMap::from([(
            "a".repeat(64),
            serde_json::json!({"agent":"warp-oz-p1gen7", "role":"agent"}),
        )]);
        write_secret_file(
            &expand_path(&m.storage.tokens_map_path).unwrap(),
            &serde_json::to_string(&map).unwrap(),
        )
        .unwrap();
        assert!(cmd_write_client(&manifest, &example, false).is_err());
        assert!(!expand_path(&m.storage.client_token_path).unwrap().exists());
    }

    #[test]
    fn dry_mint_does_not_initialize_or_write_secrets() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_mint(&manifest, &example, true).unwrap();
        assert!(!manifest.exists());
        assert!(!dir.path().join("tokens.json").exists());
        assert!(!dir.path().join("backups").exists());
    }

    #[cfg(unix)]
    #[test]
    fn map_lock_drop_unlocks_while_an_inherited_descriptor_remains_open() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("tokens.json");
        let guard = lock_token_map(&path).unwrap();
        let inherited = guard.0.try_clone().unwrap();
        assert!(lock_token_map(&path).is_err());
        drop(guard);
        let reacquired = lock_token_map(&path).unwrap();
        drop(reacquired);
        drop(inherited);
    }

    #[test]
    fn map_lock_rejects_mutation_then_releases_without_changing_map() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_mint(&manifest, &example, false).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let path = expand_path(&m.storage.tokens_map_path).unwrap();
        let before = fs::read(&path).unwrap();
        let guard = lock_token_map(&path).unwrap();
        assert!(lock_token_map(&path).is_err());
        assert!(cmd_mint_mode(&manifest, &example, false, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(guard);
        let _reacquired = lock_token_map(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn aliased_storage_paths_are_rejected_before_map_changes() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_mint(&manifest, &example, false).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let map = expand_path(&m.storage.tokens_map_path).unwrap();
        let before = fs::read(&map).unwrap();
        for client in [
            map.clone(),
            map.parent().unwrap().join("unused/../tokens.json"),
        ] {
            let mut value: Value =
                serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
            value["storage"]["client_token_path"] = serde_json::json!(client);
            fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(cmd_write_client(&manifest, &example, false).is_err());
            assert!(cmd_mint_mode(&manifest, &example, false, true).is_err());
            assert_eq!(fs::read(&map).unwrap(), before);
        }
        #[cfg(unix)]
        {
            let alias = dir.path().join("alias.json");
            std::os::unix::fs::symlink(&map, &alias).unwrap();
            let mut storage = m.storage;
            storage.client_token_path = alias.to_string_lossy().into_owned();
            assert!(validate_storage_paths(&storage).is_err());
            let sub = dir.path().join("sub");
            fs::create_dir(&sub).unwrap();
            let logical = dir.path().join("logical");
            fs::create_dir(&logical).unwrap();
            let link = logical.join("linked-directory");
            std::os::unix::fs::symlink(&sub, &link).unwrap();
            storage.client_token_path = link.join("../tokens.json").to_string_lossy().into_owned();
            assert!(validate_storage_paths(&storage).is_err());
            let mut value: Value =
                serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
            value["storage"]["client_token_path"] = serde_json::json!(storage.client_token_path);
            fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(cmd_write_client(&manifest, &example, false).is_err());
            assert_eq!(fs::read(&map).unwrap(), before);
            let client = dir.path().join("client.token");
            let pending = pending_client_path(&client).unwrap();
            fs::write(&pending, "old-fixture-credential").unwrap();
            std::os::unix::fs::symlink(&pending, &client).unwrap();
            value["storage"]["client_token_path"] = serde_json::json!(client);
            fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(cmd_mint_mode(&manifest, &example, false, true).is_err());
            assert_eq!(fs::read(&map).unwrap(), before);
            assert_eq!(
                fs::read_to_string(client).unwrap(),
                "old-fixture-credential"
            );
        }
    }

    #[test]
    fn malformed_secret_metadata_and_recovery_errors_are_redacted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("map.json");
        let secret = "secret-bearing-fixture-never-render-this-value";
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"a".repeat(64):secret})).unwrap(),
        )
        .unwrap();
        assert!(!format!("{:#}", read_token_map(&path).unwrap_err()).contains(secret));
        let (manifest, example) = sample_manifest(dir.path());
        let m = action_manifest(&manifest, &example, true).unwrap();
        let store = expand_path(&m.storage.tokens_map_path).unwrap();
        fs::write(&store, fs::read(&path).unwrap()).unwrap();
        assert!(!format!("{:#}", cmd_status(&manifest, &example).unwrap_err()).contains(secret));
        let error = verify_recovery(&m, &BTreeMap::new(), &mut |_program, args, _input, _cwd| {
            if args == ["status"] {
                return Ok(br#"{"status":"unlocked"}"#.to_vec());
            }
            Ok(serde_json::to_vec(
                &serde_json::json!({"notes":serde_json::to_string(secret).unwrap()}),
            )
            .unwrap())
        })
        .unwrap_err();
        assert!(!format!("{error:#}").contains(secret));
    }

    #[test]
    #[cfg(unix)]
    fn subprocess_private_stdin_and_failure_output_are_real_and_redacted() {
        let secret = b"dummy-private-stdin-payload";
        let large_input = vec![b'a'; 131_072];
        let output = run_external(
            "timeout",
            &["5", "sh", "-c", "head -c 131072 /dev/zero; cat"],
            Some(&large_input),
            None,
        )
        .unwrap();
        assert_eq!(output.len(), 131_072 + large_input.len());
        assert_eq!(&output[131_072..], large_input);
        let error = run_external(
            "sh",
            &["-c", "cat >&2; printf echoed-secret; exit 17"],
            Some(secret),
            None,
        )
        .unwrap_err();
        let rendered = format!("{error:#}");
        assert!(!rendered.contains(std::str::from_utf8(secret).unwrap()));
        assert!(!rendered.contains("echoed-secret"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn activation_keeps_current_client_on_failed_recovery_or_authentication() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_mint(&manifest, &example, false).unwrap();
        cmd_write_client(&manifest, &example, false).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let client = expand_path(&m.storage.client_token_path).unwrap();
        let old = fs::read(&client).unwrap();
        cmd_mint_mode(&manifest, &example, false, true).unwrap();
        let map = read_token_map(&expand_path(&m.storage.tokens_map_path).unwrap()).unwrap();
        let pending = pending_client_path(&client).unwrap();
        let replacement = fs::read_to_string(&pending).unwrap();
        assert_eq!(fs::read(&client).unwrap(), old);
        let mut invoked = false;
        assert!(
            cmd_activate(
                &manifest,
                &example,
                false,
                |_p, _a, _i, _c| bail!("no recovery"),
                |_b, _t| {
                    invoked = true;
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!invoked);
        let recovery =
            |_: &str, args: &[&str], _: Option<&[u8]>, _: Option<&Path>| -> Result<Vec<u8>> {
                if args == ["status"] {
                    Ok(br#"{"status":"unlocked"}"#.to_vec())
                } else {
                    Ok(serde_json::to_vec(
                        &serde_json::json!({"notes":serde_json::to_string(&map).unwrap()}),
                    )
                    .unwrap())
                }
            };
        assert!(
            cmd_activate(&manifest, &example, false, recovery, |_b, _t| bail!(
                "HTTP 401"
            ))
            .is_err()
        );
        assert_eq!(fs::read(&client).unwrap(), old);
        assert!(pending.exists());
        cmd_activate(&manifest, &example, false, recovery, |_base, token| {
            assert_eq!(token, replacement.trim());
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&client).unwrap(), replacement);
        assert!(!pending.exists());
    }

    #[test]
    fn private_creation_is_exclusive_and_failed_publish_cleans_staging() {
        let dir = tempdir().unwrap();
        let private = dir.path().join("exclusive.token");
        let file = create_private_file(&private).unwrap();
        assert!(
            create_private_file(&private).is_err(),
            "an existing path must never be truncated"
        );
        drop(file);
        assert_eq!(fs::metadata(&private).unwrap().len(), 0);
        let target = dir.path().join("occupied");
        fs::create_dir(&target).unwrap();
        assert!(write_secret_file(&target, "dummy-secret").is_err());
        assert!(target.is_dir());
        assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cloud-secret-")
        }));
        assert!(write_secret_file(&dir.path().join("$null"), "dummy-secret").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn private_unix_permissions_precede_secret_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("secret.token");
        let file = create_private_file(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(file.metadata().unwrap().len(), 0);
        drop(file);
        write_secret_file(&path, "dummy-secret").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    #[cfg(windows)]
    fn private_windows_reserved_paths_fail_before_directory_or_file_creation() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("never-created");
        for leaf in [
            "$null",
            "$null.json",
            "$null. ",
            "NUL.txt",
            "PRN. ",
            "COM9.log",
            "AUX:stream",
            "ordinary:stream",
        ] {
            let path = parent.join(leaf);
            assert!(write_secret_file(&path, "dummy-private-bytes").is_err());
            assert!(create_private_file(&path).is_err());
            assert!(!parent.exists());
        }
    }

    #[test]
    #[cfg(windows)]
    fn private_windows_dacl_precedes_secret_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("secret.token");
        let file = create_private_file(&path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        windows_acl_fixture::assert_private(&file);
        drop(file);
        write_secret_file(&path, "dummy-secret").unwrap();
        windows_acl_fixture::assert_private(&fs::File::open(&path).unwrap());
    }

    #[cfg(windows)]
    mod windows_acl_fixture {
        use std::ffi::c_void;
        use std::os::windows::io::AsRawHandle as _;

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
            fn GetSecurityDescriptorOwner(
                descriptor: *const c_void,
                owner: *mut *mut c_void,
                defaulted: *mut i32,
            ) -> i32;
            fn GetSecurityDescriptorDacl(
                descriptor: *const c_void,
                present: *mut i32,
                dacl: *mut *mut c_void,
                defaulted: *mut i32,
            ) -> i32;
            fn IsWellKnownSid(sid: *const c_void, kind: u32) -> i32;
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
            fn GetAclInformation(
                acl: *const c_void,
                output: *mut c_void,
                length: u32,
                class: u32,
            ) -> i32;
            fn GetAce(acl: *const c_void, index: u32, output: *mut *mut c_void) -> i32;
            fn IsValidAcl(acl: *const c_void) -> i32;
            fn IsValidSid(sid: *const c_void) -> i32;
            fn GetLengthSid(sid: *const c_void) -> u32;
            fn EqualSid(first: *const c_void, second: *const c_void) -> i32;
            fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
            fn GetTokenInformation(
                token: *mut c_void,
                class: u32,
                data: *mut c_void,
                size: u32,
                needed: *mut u32,
            ) -> i32;
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetModuleHandleW(name: *const u16) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
            fn GetCurrentProcess() -> *mut c_void;
            fn CloseHandle(handle: *mut c_void) -> i32;
            fn LocalFree(memory: *mut c_void) -> *mut c_void;
        }
        struct Token(*mut c_void);
        impl Drop for Token {
            fn drop(&mut self) {
                // SAFETY: Token owns only the successful OpenProcessToken result.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        struct Descriptor(*mut c_void);
        impl Drop for Descriptor {
            fn drop(&mut self) {
                // SAFETY: Descriptor owns the LocalAlloc-backed GetSecurityInfo result.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }

        fn process_user() -> Vec<usize> {
            let mut raw = std::ptr::null_mut();
            assert_ne!(
                // SAFETY: Current-process pseudo-handle is valid; raw is writable output.
                unsafe { OpenProcessToken(GetCurrentProcess(), 8, &raw mut raw) },
                0
            );
            let token = Token(raw);
            let mut needed = 0;
            // SAFETY: Null buffer/zero length queries TOKEN_USER's required buffer size.
            unsafe { GetTokenInformation(token.0, 1, std::ptr::null_mut(), 0, &raw mut needed) };
            assert!(
                (std::mem::size_of::<usize>()..=1_048_576)
                    .contains(&usize::try_from(needed).unwrap())
            );
            let mut data = vec![
                0_usize;
                usize::try_from(needed)
                    .unwrap()
                    .div_ceil(std::mem::size_of::<usize>())
            ];
            assert_ne!(
                // SAFETY: Aligned data allocation has at least needed writable bytes.
                unsafe {
                    GetTokenInformation(
                        token.0,
                        1,
                        data.as_mut_ptr().cast(),
                        needed,
                        &raw mut needed,
                    )
                },
                0
            );
            data
        }

        fn acl_count(dacl: *const c_void) -> u32 {
            assert!(!dacl.is_null());
            // SAFETY: DACL pointer belongs to a valid still-owned descriptor.
            assert_ne!(unsafe { IsValidAcl(dacl) }, 0);
            let mut info = AclSizeInformation {
                count: 0,
                used: 0,
                free: 0,
            };
            assert_ne!(
                // SAFETY: Valid ACL and aligned, correctly sized output.
                unsafe {
                    GetAclInformation(
                        dacl,
                        (&raw mut info).cast(),
                        u32::try_from(std::mem::size_of::<AclSizeInformation>()).unwrap(),
                        2,
                    )
                },
                0
            );
            info.count
        }

        fn allowed_ace_sid(dacl: *const c_void, index: u32) -> *const c_void {
            let mut raw = std::ptr::null_mut();
            // SAFETY: Caller checked valid ACL and bounds index against its count.
            assert_ne!(unsafe { GetAce(dacl, index, &raw mut raw) }, 0);
            assert!(!raw.is_null());
            // SAFETY: Valid ACL ACE has DWORD alignment and at least ACE_HEADER bytes.
            let header = unsafe { &*raw.cast::<AceHeader>() };
            assert_eq!(header.kind, 0, "ACCESS_ALLOWED_ACE required");
            assert_eq!(header.flags, 0, "no inherited or inheritable ACE flags");
            let sid_offset = std::mem::offset_of!(AllowedAce, sid_start);
            assert!(usize::from(header.size) >= sid_offset + 8);
            // SAFETY: Allowed ACE type, alignment and minimum size were checked.
            let allowed = unsafe { &*raw.cast::<AllowedAce>() };
            assert_eq!(allowed.mask, 0x001f_01ff, "FILE_ALL_ACCESS required");
            let sid = (&raw const allowed.sid_start).cast::<c_void>();
            // SAFETY: Checked ACE size includes the eight-byte SID header.
            let subauthorities = unsafe { *sid.cast::<u8>().add(1) };
            assert!(subauthorities <= 15);
            assert!(sid_offset + 8 + usize::from(subauthorities) * 4 <= usize::from(header.size));
            // SAFETY: The entire SID implied by its header fits the valid ACE.
            assert_ne!(unsafe { IsValidSid(sid) }, 0);
            // SAFETY: SID was validated and backing descriptor remains alive.
            assert!(
                usize::try_from(
                    // SAFETY: SID validated and its backing descriptor remains alive.
                    unsafe { GetLengthSid(sid) },
                )
                .unwrap()
                    + sid_offset
                    <= usize::from(header.size)
            );
            sid
        }

        fn assert_single_user_ace(dacl: *const c_void, user: *const c_void) {
            assert_eq!(acl_count(dacl), 1, "exactly one owner ACE required");
            let sid = allowed_ace_sid(dacl, 0);
            assert_ne!(
                // SAFETY: Both SIDs are valid in still-live allocations.
                unsafe { EqualSid(sid, user) },
                0,
                "ACE must match current process user"
            );
        }

        fn is_wine() -> bool {
            let name: Vec<u16> = "ntdll.dll".encode_utf16().chain(Some(0)).collect();
            // SAFETY: NUL-terminated owned string queries an already-loaded module only.
            let module = unsafe { GetModuleHandleW(name.as_ptr()) };
            assert!(!module.is_null(), "ntdll must already be loaded");
            // SAFETY: Live module and NUL-terminated ASCII export name; no export is invoked.
            !unsafe { GetProcAddress(module, c"wine_get_version".as_ptr().cast()) }.is_null()
        }

        fn assert_creation_descriptor(user: *const c_void) {
            super::super::windows_private_file::inspect_private_descriptor(|descriptor| {
                let mut owner = std::ptr::null_mut();
                let mut defaulted = 0;
                let mut present = 0;
                let mut dacl = std::ptr::null_mut();
                let mut control = 0;
                let mut revision = 0;
                assert_ne!(
                    // SAFETY: Callback borrows the actual production descriptor before LocalFree.
                    unsafe {
                        GetSecurityDescriptorOwner(descriptor, &raw mut owner, &raw mut defaulted)
                    },
                    0
                );
                assert!(!owner.is_null());
                // SAFETY: Returned owner is inside the live descriptor; user already validated.
                assert_ne!(unsafe { IsValidSid(owner) }, 0);
                // SAFETY: Both validated SIDs remain alive for comparison.
                assert_ne!(unsafe { EqualSid(owner, user) }, 0);
                assert_ne!(
                    // SAFETY: Valid descriptor and writable output fields.
                    unsafe {
                        GetSecurityDescriptorDacl(
                            descriptor,
                            &raw mut present,
                            &raw mut dacl,
                            &raw mut defaulted,
                        )
                    },
                    0
                );
                assert_ne!(present, 0);
                assert_ne!(
                    // SAFETY: Valid live descriptor and correctly typed writable outputs.
                    unsafe {
                        GetSecurityDescriptorControl(
                            descriptor,
                            &raw mut control,
                            &raw mut revision,
                        )
                    },
                    0
                );
                assert_eq!(revision, 1);
                assert_eq!(
                    control & 0x1004,
                    0x1004,
                    "production descriptor must be protected on every host"
                );
                assert_single_user_ace(dacl, user);
            })
            .unwrap();
        }

        fn assert_wine_projection(dacl: *const c_void, user: *const c_void) {
            // Wine 9 server/file.c mode_to_sd synthesizes LocalSystem plus the
            // Unix owner, and cannot preserve SE_DACL_PROTECTED. This verifies
            // that exact emulation tier, never native filesystem protection.
            assert_eq!(
                acl_count(dacl),
                2,
                "exact Wine owner/system projection required"
            );
            let mut users = 0;
            let mut systems = 0;
            for index in 0..2 {
                let sid = allowed_ace_sid(dacl, index);
                // SAFETY: Valid SID and still-live user allocation.
                if unsafe { EqualSid(sid, user) } != 0 {
                    users += 1;
                }
                // SAFETY: Valid SID; WinLocalSystemSid is WELL_KNOWN_SID_TYPE 22.
                else if unsafe { IsWellKnownSid(sid, 22) } != 0 {
                    systems += 1;
                } else {
                    panic!("unexpected principal in Wine projected ACL");
                }
            }
            assert_eq!((users, systems), (1, 1));
            eprintln!("WINE_EMULATED_ACL_ONLY: native protected filesystem ACL NOT_ESTABLISHED");
        }

        pub(super) fn assert_private(file: &std::fs::File) {
            let user_data = process_user();
            // SAFETY: Successful TOKEN_USER buffer read initialized its first SID pointer.
            let user = unsafe { *user_data.as_ptr().cast::<*const c_void>() };
            // SAFETY: TOKEN_USER owns the SID within the still-live aligned user_data allocation.
            assert_ne!(unsafe { IsValidSid(user) }, 0);
            let mut owner = std::ptr::null_mut();
            let mut dacl = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: Owned file handle is live; output pointers are writable; other outputs optional.
            let code = unsafe {
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
            assert_eq!(code, 0, "GetSecurityInfo must succeed");
            assert!(!descriptor.is_null());
            let descriptor = Descriptor(descriptor);
            assert!(!owner.is_null());
            // SAFETY: Owner SID was returned inside the still-owned security descriptor.
            assert_ne!(unsafe { IsValidSid(owner) }, 0);
            assert_ne!(
                // SAFETY: Both SIDs validated; descriptor and TOKEN_USER buffers are live.
                unsafe { EqualSid(owner, user) },
                0,
                "current-user owner required"
            );
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                // SAFETY: Owned descriptor valid; control and revision are writable outputs.
                unsafe {
                    GetSecurityDescriptorControl(descriptor.0, &raw mut control, &raw mut revision)
                },
                0
            );
            assert_eq!(revision, 1);
            assert_creation_descriptor(user);
            if is_wine() {
                assert_eq!(
                    control & 0x1004,
                    4,
                    "documented Wine projected descriptor expected"
                );
                assert_wine_projection(dacl, user);
            } else {
                assert_eq!(
                    control & 0x1004,
                    0x1004,
                    "native present, protected DACL required"
                );
                assert_single_user_ace(dacl, user);
            }
        }
    }

    #[test]
    #[cfg(not(feature = "server-mode"))]
    fn minimal_revoke_refuses_before_any_file_or_provider_operation() {
        assert!(
            cmd_revoke(
                Path::new("absent-fixture.json"),
                None,
                false,
                |_, _, _, _| panic!("minimal build provider invoked"),
                |_, _, _, _| panic!("minimal build HTTP invoked")
            )
            .unwrap_err()
            .to_string()
            .contains("requires server-mode")
        );
    }

    #[test]
    fn failed_recovery_never_invokes_wrangler() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_mint(&manifest, &example, false).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let map = read_token_map(&expand_path(&m.storage.tokens_map_path).unwrap()).unwrap();
        simulated_upload_authority(&manifest, &map);
        for mismatch in [false, true] {
            let mut calls = Vec::new();
            let result = cmd_upload(
                &manifest,
                &example,
                Some(dir.path()),
                false,
                |program, args, _input, _cwd| {
                    calls.push(program.to_owned());
                    if args == ["status"] {
                        return Ok(br#"{"status":"unlocked"}"#.to_vec());
                    }
                    if mismatch {
                        Ok(br#"{"notes":"{}"}"#.to_vec())
                    } else {
                        bail!("recovery unavailable")
                    }
                },
            );
            assert!(result.is_err());
            assert_eq!(calls, ["bw", "bw"]);
        }
    }

    #[test]
    fn verified_upload_passes_the_whole_map_only_over_stdin() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        cmd_mint(&manifest, &example, false).unwrap();
        let m = load_manifest(&manifest).unwrap();
        let map = read_token_map(&expand_path(&m.storage.tokens_map_path).unwrap()).unwrap();
        fs::create_dir_all(dir.path().join("cloud/agentbus")).unwrap();
        simulated_upload_authority(&manifest, &map);
        let mut uploads = 0;
        cmd_upload(
            &manifest,
            &example,
            Some(dir.path()),
            false,
            |program, args, input, cwd| {
                if program == "bw" && args == ["status"] {
                    return Ok(br#"{"status":"unlocked"}"#.to_vec());
                }
                if program == "bw" {
                    return Ok(serde_json::to_vec(
                        &serde_json::json!({"notes":serde_json::to_string(&map).unwrap()}),
                    )
                    .unwrap());
                }
                assert_eq!(program, "wrangler");
                assert_eq!(
                    args,
                    [
                        "secret",
                        "put",
                        "AGENT_BUS_TOKENS",
                        "--name",
                        "agentbus-cloud"
                    ]
                );
                assert_eq!(
                    serde_json::from_slice::<BTreeMap<String, Value>>(input.unwrap()).unwrap(),
                    map
                );
                assert_eq!(cwd.unwrap(), dir.path().join("cloud/agentbus"));
                uploads += 1;
                Ok(Vec::new())
            },
        )
        .unwrap();
        assert_eq!(uploads, 1);
    }

    #[test]
    #[cfg(feature = "server-mode")]
    fn authenticated_smoke_cannot_skip_a_missing_client() {
        let dir = tempdir().unwrap();
        let (manifest, example) = sample_manifest(dir.path());
        assert!(
            cmd_smoke(&manifest, &example)
                .unwrap_err()
                .to_string()
                .contains("client token file")
        );
    }

    #[test]
    #[cfg(feature = "server-mode")]
    fn health_errors_stop_before_authenticated_requests() {
        use std::io::Read as _;
        for status in [302, 304, 500] {
            let listener = std::net::TcpListener::bind("localhost:0").unwrap();
            let base = format!("http://localhost:{}", listener.local_addr().unwrap().port());
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    assert_eq!(socket.read(&mut byte).unwrap(), 1);
                    request.push(byte[0]);
                    assert!(request.len() < 8192);
                }
                assert!(
                    !String::from_utf8_lossy(&request)
                        .to_ascii_lowercase()
                        .contains("authorization")
                );
                let body = r#"{"ok":true}"#;
                write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                drop(socket);
                listener.set_nonblocking(true).unwrap();
                listener
            });
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let error = runtime
                .block_on(smoke_http(&base, &"a".repeat(64)))
                .unwrap_err();
            assert!(error.to_string().contains(&status.to_string()));
            let listener = server.join().unwrap();
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
        }
    }

    #[test]
    #[cfg(feature = "server-mode")]
    fn authenticated_smoke_rejects_all_unsuccessful_statuses() {
        use std::io::Read as _;
        for status in [401, 403, 500, 501] {
            let listener = std::net::TcpListener::bind("localhost:0").unwrap();
            let base = format!("http://localhost:{}", listener.local_addr().unwrap().port());
            let server = std::thread::spawn(move || {
                for n in 0..2 {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        assert_eq!(socket.read(&mut byte).unwrap(), 1);
                        request.push(byte[0]);
                        assert!(request.len() < 8192);
                    }
                    let request = String::from_utf8_lossy(&request);
                    let (code, body) = if n == 0 {
                        (200, r#"{"ok":true}"#)
                    } else {
                        assert!(
                            request
                                .to_ascii_lowercase()
                                .contains(&format!("authorization: bearer {}", "a".repeat(64)))
                        );
                        (status, "{}")
                    };
                    write!(socket,"HTTP/1.1 {code} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                }
            });
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let error = runtime
                .block_on(smoke_http(&base, &"a".repeat(64)))
                .unwrap_err();
            assert!(error.to_string().contains(&status.to_string()));
            assert!(!error.to_string().contains(&"a".repeat(64)));
            server.join().unwrap();
        }
    }
}
