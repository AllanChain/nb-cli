use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Directory holding the connection store (default `~/.config/nb`).
const STORE_DIR_NAME: &str = "nb";
const STORE_FILE_NAME: &str = "connections.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterConnection {
    pub server_url: String,
    pub token: String,
    pub connected_at: DateTime<Utc>,
    pub working_dir: Option<String>,
    pub last_validated: Option<DateTime<Utc>>,
    /// Environment manager used when connecting (direct, uv, pixi)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_manager: Option<String>,
    /// Project root path for uv/pixi environments
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    /// Whether Y.js/collaboration backend is available on the server.
    /// None means unknown: commands probe the server live, and execution
    /// tries Y.js with a fallback to the kernel-WS path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ydoc_available: Option<bool>,
}

/// The resolved connection for the cwd; `None` means no recorded project root
/// matches. Runtime view only — the serialized form is the [`ConnectionStore`].
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub connection: Option<JupyterConnection>,
}

impl Config {
    /// Load the connection for the cwd: the entry whose recorded project root
    /// is the longest ancestor-or-equal of the cwd (`Path::starts_with`
    /// semantics). Never reads files inside repositories.
    pub fn load() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::new());
        Self::resolve_config(&cwd, &load_store())
    }

    fn resolve_config(cwd: &Path, store: &ConnectionStore) -> Self {
        Config {
            connection: resolve_project_root(store, cwd).map(|(_, conn)| conn.clone()),
        }
    }

    /// Save the connection keyed by the cwd's project root (updating an
    /// existing root when inside one, else recording the cwd). A `None`
    /// connection removes the entry — what `nb disconnect` uses.
    pub fn save(&self) -> Result<PathBuf> {
        let cwd = std::env::current_dir().context("Could not determine current directory")?;
        let mut store = load_store();

        let key = match resolve_project_root(&store, &cwd) {
            Some((root, _)) => root,
            None => normalize_root(&cwd),
        };
        match &self.connection {
            Some(conn) => {
                store.projects.insert(key, conn.clone());
            }
            None => {
                store.projects.remove(&key);
            }
        }

        save_store(&store)
    }

    /// Resolve connection with CLI args taking priority
    pub fn resolve_connection(
        &self,
        cli_server: Option<String>,
        cli_token: Option<String>,
    ) -> Result<Option<(String, String)>> {
        // CLI args have highest priority
        if let (Some(server), Some(token)) = (cli_server, cli_token) {
            return Ok(Some((server, token)));
        }

        // Check saved connection
        if let Some(conn) = &self.connection {
            return Ok(Some((conn.server_url.clone(), conn.token.clone())));
        }

        Ok(None)
    }
}

// ==================== CONNECTION STORE ====================

/// Connections keyed by the project root where the user last ran `nb connect`.
/// Lives in the user's home directory, never in the project tree.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ConnectionStore {
    #[serde(default)]
    version: String,
    #[serde(default)]
    projects: BTreeMap<String, JupyterConnection>,
}

/// Directory holding the connection store: `$NB_CONFIG_HOME` when set (tests
/// use this), else the platform config directory plus `nb`.
fn store_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NB_CONFIG_HOME") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let base = dirs::config_dir().context("Could not determine config directory")?;
    Ok(base.join(STORE_DIR_NAME))
}

fn store_path() -> Result<PathBuf> {
    Ok(store_dir()?.join(STORE_FILE_NAME))
}

/// Load the store; a missing, untrusted, or malformed store is reported on
/// stderr and treated as empty so it can never brick the CLI.
fn load_store() -> ConnectionStore {
    let path = match store_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Warning: could not determine connection store path: {e}");
            return ConnectionStore::default();
        }
    };
    load_store_from(&path)
}

/// Load a store from an explicit path.
fn load_store_from(path: &Path) -> ConnectionStore {
    let content = match check_config_path(path).and_then(|_| read_config_contents(path)) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ConnectionStore::default(),
        Err(e) => {
            eprintln!(
                "Warning: ignoring untrusted connection store {}: {}",
                path.display(),
                e
            );
            return ConnectionStore::default();
        }
    };
    match serde_json::from_str(&content) {
        Ok(store) => store,
        Err(e) => {
            eprintln!(
                "Warning: ignoring malformed connection store {}: {}",
                path.display(),
                e
            );
            ConnectionStore::default()
        }
    }
}

/// Persist the store to the default location and return its path.
fn save_store(store: &ConnectionStore) -> Result<PathBuf> {
    let path = store_path()?;
    save_store_to(&path, store)?;
    Ok(path)
}

/// Persist a store via a 0600 temp file + atomic rename; refuses to overwrite
/// a store the trust checks reject, so a planted store can't receive a fresh
/// token.
fn save_store_to(path: &Path, store: &ConnectionStore) -> Result<()> {
    check_save_target(path)?;

    let dir = path.parent().context("Connection store path has no parent")?;
    fs::create_dir_all(dir).context("Failed to create nb config directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best effort: keep the store directory private (it holds tokens).
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    let json =
        serde_json::to_string_pretty(store).context("Failed to serialize connection store")?;
    let tmp = dir.join(format!("{STORE_FILE_NAME}.tmp"));
    fs::write(&tmp, &json).with_context(|| format!("Failed to write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    // Windows cannot rename over an existing file; remove first (tiny window,
    // acceptable for a config store).
    #[cfg(windows)]
    {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to move store into place at {}", path.display()))?;
    Ok(())
}

/// Canonicalize a project root for use as a store key (resolves symlinks so a
/// path recorded through a link matches a later `getcwd`).
fn normalize_root(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The recorded project root that is the longest ancestor-or-equal of `cwd`
/// (component-wise, so `/a/b` never matches `/a/bc`); deepest match wins.
fn resolve_project_root<'a>(
    store: &'a ConnectionStore,
    cwd: &Path,
) -> Option<(String, &'a JupyterConnection)> {
    let cwd_norm = normalize_root(cwd);
    let cwd_path = Path::new(&cwd_norm);
    store
        .projects
        .iter()
        .filter(|(root, _)| cwd_path.starts_with(Path::new(root)))
        .max_by_key(|(root, _)| Path::new(root).components().count())
        .map(|(root, conn)| (root.clone(), conn))
}

// ==================== TRUST CHECKS ====================

/// Path-level trust check via `lstat` (rejects symlinks before opening);
/// re-checked on the opened fd inside [`read_config_contents`].
fn check_config_path(path: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    check_config_meta(&meta)
        .map_err(|reason| std::io::Error::new(std::io::ErrorKind::PermissionDenied, reason))
}

/// Trust check for a store `nb` consults: regular file (lstat also rejects
/// symlinks), owned by the current user, writable by no one else — the same
/// stance ssh takes for keys. (Ownership alone is not enough: files the user
/// materializes — git checkouts, archives — become user-owned, which is why
/// the store lives in the home dir and repo files are never *discovered*.)
#[cfg(unix)]
fn check_config_meta(meta: &fs::Metadata) -> std::result::Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    // For a symlink, lstat reports the link itself, so this rejects symlinks
    // along with other non-regular files.
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    let uid = current_uid();
    if meta.uid() != uid {
        return Err(format!("owned by uid {}, not {uid}", meta.uid()));
    }
    if meta.mode() & 0o022 != 0 {
        return Err("writable by group or others (fix: chmod go-w the file)".to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: getuid is always safe to call.
    unsafe { libc::getuid() }
}

/// Non-Unix fallback: no uid/permission checks available, but still reject
/// non-regular files (including symlinks).
#[cfg(not(unix))]
fn check_config_meta(meta: &fs::Metadata) -> std::result::Result<(), String> {
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    Ok(())
}

/// Refuse to save over a file the trust checks would refuse to read back
/// (a planted world-writable store must not receive a freshly typed token).
#[cfg(unix)]
fn check_save_target(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        check_config_meta(&meta).map_err(|reason| {
            anyhow::anyhow!("refusing to write connection store to {}: {}", path.display(), reason)
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_save_target(_path: &Path) -> Result<()> {
    Ok(())
}

/// Read a store, re-checking the trust policy via fstat on the opened fd —
/// the only TOCTOU measure needed, since a swapped-in file must pass again.
#[cfg(unix)]
fn read_config_contents(path: &Path) -> std::io::Result<String> {
    use std::io::Read;

    let mut file = fs::File::open(path)?;
    let meta = file.metadata()?;
    if let Err(reason) = check_config_meta(&meta) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            reason,
        ));
    }
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

#[cfg(not(unix))]
fn read_config_contents(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

    fn make_connection(server_url: &str, token: &str) -> JupyterConnection {
        JupyterConnection {
            server_url: server_url.to_string(),
            token: token.to_string(),
            connected_at: Utc::now(),
            working_dir: None,
            last_validated: None,
            env_manager: None,
            project_root: None,
            ydoc_available: None,
        }
    }

    fn store_with(projects: &[(&str, &str)]) -> ConnectionStore {
        let mut store = ConnectionStore::default();
        store.version = "2".to_string();
        for (root, token) in projects {
            store
                .projects
                .insert((*root).to_string(), make_connection("http://srv", token));
        }
        store
    }

    // ==================== RESOLVE CONNECTION TESTS ====================

    #[test]
    fn test_resolve_connection_priority() {
        let saved = Config {
            connection: Some(make_connection("http://saved:8888", "saved_token")),
        };
        // CLI args beat saved; partial CLI args fall back to saved.
        assert_eq!(
            saved
                .resolve_connection(Some("http://cli:9999".into()), Some("cli_token".into()))
                .unwrap(),
            Some(("http://cli:9999".to_string(), "cli_token".to_string()))
        );
        assert_eq!(
            saved.resolve_connection(Some("http://cli:9999".into()), None).unwrap(),
            Some(("http://saved:8888".to_string(), "saved_token".to_string()))
        );
        assert_eq!(
            saved.resolve_connection(None, None).unwrap(),
            Some(("http://saved:8888".to_string(), "saved_token".to_string()))
        );
        // No saved connection and no args → None.
        assert_eq!(Config::default().resolve_connection(None, None).unwrap(), None);
    }

    #[test]
    fn test_serde_roundtrip_with_connection() {
        let original = JupyterConnection {
            server_url: "http://127.0.0.1:8888".to_string(),
            token: "abc123".to_string(),
            connected_at: Utc::now(),
            working_dir: Some("/home/user".to_string()),
            last_validated: None,
            env_manager: Some("uv".to_string()),
            project_root: Some("/projects/myproject".to_string()),
            ydoc_available: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let roundtripped: JupyterConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.server_url, "http://127.0.0.1:8888");
        assert_eq!(roundtripped.token, "abc123");
        assert_eq!(roundtripped.working_dir, Some("/home/user".to_string()));
        assert_eq!(roundtripped.env_manager, Some("uv".to_string()));
        assert_eq!(roundtripped.project_root, Some("/projects/myproject".to_string()));
    }

    #[test]
    fn test_serde_env_manager_omitted_when_none() {
        // skip_serializing_if means None fields must be absent, not null —
        // required for forward compat with stores written before these fields existed.
        let conn = make_connection("http://127.0.0.1:8888", "tok");
        let json = serde_json::to_string(&conn).unwrap();
        assert!(
            !json.contains("env_manager"),
            "env_manager must be absent when None\nJSON: {}",
            json
        );
        assert!(
            !json.contains("project_root"),
            "project_root must be absent when None\nJSON: {}",
            json
        );
    }

    // ==================== PROJECT-ROOT RESOLUTION TESTS ====================

    #[test]
    fn test_resolve_project_root_matches_cwd_and_ancestors() {
        let store = store_with(&[("/home/user/proj", "tok")]);
        for cwd in ["/home/user/proj", "/home/user/proj/notebooks/2024"] {
            let (root, conn) = resolve_project_root(&store, Path::new(cwd)).expect("match");
            assert_eq!(root, "/home/user/proj");
            assert_eq!(conn.token, "tok");
        }
    }

    #[test]
    fn test_resolve_project_root_deepest_wins() {
        let store = store_with(&[("/home/user/proj", "root"), ("/home/user/proj/sub", "sub")]);
        let (root, conn) =
            resolve_project_root(&store, Path::new("/home/user/proj/sub/deep")).expect("match");
        assert_eq!(root, "/home/user/proj/sub");
        assert_eq!(conn.token, "sub");
    }

    #[test]
    fn test_resolve_project_root_no_false_prefix_match() {
        // /home/user/proj must not match /home/user/proj2, siblings, or an
        // empty store.
        let store = store_with(&[("/home/user/proj", "tok")]);
        for cwd in ["/home/user/proj2", "/home/other/proj"] {
            assert!(resolve_project_root(&store, Path::new(cwd)).is_none());
        }
        assert!(resolve_project_root(&ConnectionStore::default(), Path::new("/anywhere")).is_none());
    }

    // ==================== STORE I/O TESTS ====================

    #[test]
    fn test_store_roundtrip_via_explicit_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("connections.json");
        let mut store = ConnectionStore::default();
        store
            .projects
            .insert("/p".to_string(), make_connection("http://h:8888", "t"));
        save_store_to(&path, &store).unwrap();

        let loaded = load_store_from(&path);
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects["/p"].server_url, "http://h:8888");
        assert_eq!(loaded.projects["/p"].token, "t");
    }

    #[test]
    fn test_load_store_missing_file_is_empty() {
        let tmp = TempDir::new().unwrap();
        let store = load_store_from(&tmp.path().join("connections.json"));
        assert!(store.projects.is_empty());
    }

    #[test]
    fn test_load_store_malformed_is_empty_without_panicking() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("connections.json");
        fs::write(&path, "{{{ not json").unwrap();
        let store = load_store_from(&path);
        assert!(store.projects.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_load_store_untrusted_is_empty() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("connections.json");
        fs::write(&path, r#"{"version":"2","projects":{}}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        let store = load_store_from(&path);
        assert!(store.projects.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_save_store_refuses_untrusted_existing_target() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("connections.json");
        // Attacker-planted world-writable store: nb must refuse to write the
        // token into it instead of doing so silently.
        fs::write(&path, "attacker").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        let store = store_with(&[("/p", "tok")]);
        assert!(save_store_to(&path, &store).is_err());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("attacker"), "untrusted store must not be overwritten");
    }

    #[cfg(unix)]
    #[test]
    fn test_load_store_rejects_symlinked_store() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real.json");
        fs::write(&real, r#"{"version":"2","projects":{}}"#).unwrap();
        let link = tmp.path().join("connections.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let store = load_store_from(&link);
        assert!(store.projects.is_empty());
    }

    // ==================== RESOLVE CONFIG TESTS ====================

    #[test]
    fn test_resolve_config_uses_store_connection() {
        let tmp = TempDir::new().unwrap();
        let store = store_with(&[(&normalize_root(tmp.path()), "tok")]);
        let config = Config::resolve_config(tmp.path(), &store);
        assert_eq!(config.connection.unwrap().token, "tok");
    }

    #[test]
    fn test_resolve_config_default_when_nothing_matches() {
        let tmp = TempDir::new().unwrap();
        let config = Config::resolve_config(tmp.path(), &ConnectionStore::default());
        assert!(config.connection.is_none());
    }
}
