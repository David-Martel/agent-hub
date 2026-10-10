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

    pub(super) fn create(path: &Path) -> Result<fs::File> {
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

fn lock_token_map(path: &Path) -> Result<fs::File> {
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
    Ok(file)
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

fn require_deployment_authority(m: &Manifest, candidate: &BTreeMap<String, Value>) -> Result<()> {
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
    for (token, metadata) in recovered {
        if candidate.get(&token) != Some(&metadata) {
            bail!("candidate omits or changes an authoritative baseline entry; upload refused");
        }
    }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
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
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        drop(file);
        let script = r"$acl=Get-Acl -LiteralPath $env:CLOUD_TOKEN_ACL_FIXTURE; $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User; $rules=@($acl.GetAccessRules($true,$true,[Security.Principal.SecurityIdentifier])); if(-not $acl.AreAccessRulesProtected -or $rules.Count -ne 1 -or $rules[0].IdentityReference.Value -ne $sid.Value -or $rules[0].AccessControlType -ne 'Allow' -or $rules[0].IsInherited){exit 1}";
        for content in [None, Some("dummy-secret")] {
            if let Some(content) = content {
                write_secret_file(&path, content).unwrap();
            }
            assert!(
                std::process::Command::new("pwsh")
                    .args(["-NoLogo", "-NoProfile", "-Command", script])
                    .env("CLOUD_TOKEN_ACL_FIXTURE", &path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
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
