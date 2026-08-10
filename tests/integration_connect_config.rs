//! Tests for `nb status`, `nb connect`, and `nb disconnect` against the
//! project-keyed connection store.
//!
//! Connections live in `$NB_CONFIG_HOME/connections.json` (default
//! `~/.config/nb/connections.json`), keyed by the project root where the user
//! ran `nb connect`; commands in subdirectories resolve the nearest recorded
//! root. These tests do NOT require a live Jupyter server — they exercise the
//! config read/write code path only. Every test points `NB_CONFIG_HOME` at its
//! own TempDir so tests are isolated from the developer's real store and from
//! each other (each test spawns its own `nb` process, so the env var cannot
//! race).

mod test_helpers;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use test_helpers::CommandResult;

// ==================== TEST HELPERS ====================

struct ConfigTestEnv {
    /// Working tree where `nb` is invoked (the "project").
    temp_dir: TempDir,
    /// Stand-in for `~/.config/nb` (the connection store location).
    store_dir: TempDir,
    binary_path: PathBuf,
}

impl ConfigTestEnv {
    fn new() -> Self {
        ConfigTestEnv {
            temp_dir: TempDir::new().expect("Failed to create temp dir"),
            store_dir: TempDir::new().expect("Failed to create store dir"),
            binary_path: env!("CARGO_BIN_EXE_nb").into(),
        }
    }

    fn store_path(&self) -> PathBuf {
        self.store_dir.path().join("connections.json")
    }

    /// Seed the connection store directly, mapping project roots to
    /// (server_url, token, env_manager).
    fn seed_store(&self, projects: &[(&str, &str, &str, Option<&str>)]) {
        let mut map = serde_json::Map::new();
        for (root, server_url, token, env_manager) in projects {
            map.insert(
                (*root).to_string(),
                json!({
                    "server_url": server_url,
                    "token": token,
                    "connected_at": "2024-01-01T00:00:00Z",
                    "working_dir": null,
                    "last_validated": null,
                    "env_manager": env_manager
                }),
            );
        }
        let store = json!({ "version": "2", "projects": map });
        fs::write(self.store_path(), serde_json::to_string_pretty(&store).unwrap())
            .expect("Failed to seed store");
    }

    /// Write arbitrary bytes to `$NB_CONFIG_HOME/connections.json` (for
    /// testing malformed input).
    fn write_raw_store(&self, content: &str) {
        fs::write(self.store_path(), content).expect("Failed to write raw store");
    }

    /// Read and parse the store; returns None if it does not exist.
    fn read_store(&self) -> Option<Value> {
        if !self.store_path().exists() {
            return None;
        }
        let content = fs::read_to_string(&self.store_path()).expect("Failed to read store");
        Some(serde_json::from_str(&content).expect("Failed to parse store"))
    }

    fn run(&self, args: &[&str]) -> CommandResult {
        self.run_in(self.temp_dir.path(), args)
    }

    /// Run nb with the given working directory and NB_CONFIG_HOME pointing at
    /// this test's store dir.
    fn run_in(&self, dir: &Path, args: &[&str]) -> CommandResult {
        let output = Command::new(&self.binary_path)
            .args(args)
            .env("NB_CONFIG_HOME", self.store_dir.path())
            .current_dir(dir)
            .output()
            .expect("Failed to execute nb command");
        CommandResult {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
        }
    }
}

// ==================== STATUS TESTS ====================

#[test]
fn test_status_when_not_connected() {
    let env = ConfigTestEnv::new();
    let result = env.run(&["status"]).assert_success();
    assert!(
        result
            .stdout
            .contains("Not connected to any Jupyter server"),
        "Expected 'Not connected' message\nStdout: {}",
        result.stdout
    );
}

#[test]
fn test_status_human_readable_when_connected() {
    let env = ConfigTestEnv::new();
    let root = env.temp_dir.path().canonicalize().unwrap();
    env.seed_store(&[(root.to_str().unwrap(), "http://127.0.0.1:8888", "testtoken", None)]);
    let result = env.run(&["status"]).assert_success();
    assert!(
        result.stdout.contains("http://127.0.0.1:8888"),
        "Expected server URL in status output\nStdout: {}",
        result.stdout
    );
    assert!(
        result.stdout.contains("✓ Connected"),
        "Expected connected indicator\nStdout: {}",
        result.stdout
    );
}

#[test]
fn test_status_json_when_not_connected() {
    let env = ConfigTestEnv::new();
    let result = env.run(&["status", "--json"]).assert_success();
    assert_eq!(
        result.stdout.trim(),
        "null",
        "Expected 'null' JSON when not connected\nStdout: {}",
        result.stdout
    );
}

#[test]
fn test_status_json_when_connected() {
    let env = ConfigTestEnv::new();
    let root = env.temp_dir.path().canonicalize().unwrap();
    env.seed_store(&[(root.to_str().unwrap(), "http://127.0.0.1:9999", "mytoken", None)]);
    let result = env.run(&["status", "--json"]).assert_success();
    let json: Value =
        serde_json::from_str(&result.stdout).expect("status --json did not produce valid JSON");
    assert_eq!(
        json["server_url"].as_str(),
        Some("http://127.0.0.1:9999"),
        "JSON missing server_url"
    );
    assert!(
        json["connected_at"].is_string(),
        "JSON missing connected_at"
    );
}

#[test]
fn test_status_python_no_env_manager() {
    let env = ConfigTestEnv::new();
    let root = env.temp_dir.path().canonicalize().unwrap();
    env.seed_store(&[(root.to_str().unwrap(), "http://127.0.0.1:8888", "tok", None)]);
    let result = env.run(&["status", "--python"]).assert_success();
    assert!(
        result.stdout.trim().is_empty(),
        "Expected empty output for --python with no env_manager\nStdout: {:?}",
        result.stdout
    );
}

#[test]
fn test_status_python_env_managers() {
    for (env_manager, expected) in [("uv", "uv run"), ("pixi", "pixi run")] {
        let env = ConfigTestEnv::new();
        let root = env.temp_dir.path().canonicalize().unwrap();
        env.seed_store(&[(
            root.to_str().unwrap(),
            "http://127.0.0.1:8888",
            "tok",
            Some(env_manager),
        )]);
        let result = env.run(&["status", "--python"]).assert_success();
        assert_eq!(
            result.stdout.trim(),
            expected,
            "env_manager {env_manager}\nStdout: {:?}",
            result.stdout
        );
    }
}

// ==================== DISCONNECT TESTS ====================

#[test]
fn test_disconnect_when_connected() {
    let env = ConfigTestEnv::new();
    let root = env.temp_dir.path().canonicalize().unwrap();
    env.seed_store(&[(root.to_str().unwrap(), "http://127.0.0.1:8888", "tok", None)]);

    let result = env.run(&["disconnect"]).assert_success();
    assert!(
        result.stdout.contains("✓ Disconnected"),
        "Expected disconnected confirmation\nStdout: {}",
        result.stdout
    );

    // The store entry must be gone entirely, and status must reflect it.
    let store = env.read_store().expect("store file should still exist");
    assert!(
        store["projects"].as_object().map(|m| m.is_empty()).unwrap_or(false),
        "connection must be removed after disconnect\nStore: {}",
        store
    );
    let status = env.run(&["status"]).assert_success();
    assert!(
        status.stdout.contains("Not connected to any Jupyter server"),
        "Expected 'Not connected' after disconnect\nStdout: {}",
        status.stdout
    );
}

#[test]
fn test_disconnect_when_not_connected() {
    let env = ConfigTestEnv::new();
    let result = env.run(&["disconnect"]).assert_success();
    assert!(
        result
            .stdout
            .contains("Not connected to any Jupyter server"),
        "Expected not-connected message\nStdout: {}",
        result.stdout
    );
}

// ==================== STORE ERROR TESTS ====================

#[test]
fn test_status_with_malformed_store_json() {
    let env = ConfigTestEnv::new();
    env.write_raw_store("{{{ not valid json");

    // Must not panic; the store is treated as empty with a warning.
    let result = env.run(&["status"]).assert_success();
    assert!(
        result.stdout.contains("Not connected"),
        "malformed store must be treated as empty\nStdout: {}\nStderr: {}",
        result.stdout, result.stderr
    );
    assert!(
        result.stderr.contains("malformed connection store"),
        "malformed store must print a warning to stderr\nStderr: {}",
        result.stderr
    );
}

#[test]
fn test_status_with_incomplete_connection_object() {
    let env = ConfigTestEnv::new();
    // Missing required `token` field — serde fails to deserialize
    // JupyterConnection, so the whole store entry is dropped.
    let root = env.temp_dir.path().canonicalize().unwrap();
    env.write_raw_store(&format!(
        r#"{{"version":"2","projects":{{"{}":{{"server_url":"http://127.0.0.1:8888"}}}}}}"#,
        root.display()
    ));

    let result = env.run(&["status"]).assert_success();
    assert!(
        result.stdout.contains("Not connected"),
        "incomplete connection must be dropped\nStdout: {}\nStderr: {}",
        result.stdout, result.stderr
    );
}

// ==================== PROJECT-ROOT RESOLUTION TESTS ====================
//
// `nb connect` records the connection under the project root; commands run
// from any subdirectory resolve the nearest recorded root, and connect from a
// subdirectory updates the project entry instead of forking a new one.

#[test]
fn test_status_from_subdirectory_finds_project_connection() {
    let env = ConfigTestEnv::new();
    let root = env.temp_dir.path().canonicalize().unwrap();
    env.seed_store(&[(root.to_str().unwrap(), "http://127.0.0.1:8888", "tok", None)]);
    let sub = env.temp_dir.path().join("notebooks").join("2024");
    fs::create_dir_all(&sub).expect("create subdir");

    let result = env.run_in(&sub, &["status"]).assert_success();
    assert!(
        result.stdout.contains("http://127.0.0.1:8888"),
        "status from subdir must resolve the project connection\nStdout: {}",
        result.stdout
    );
}

#[test]
fn test_status_deepest_recorded_root_wins() {
    let env = ConfigTestEnv::new();
    let root = env.temp_dir.path().canonicalize().unwrap();
    let sub = env.temp_dir.path().join("notebooks");
    fs::create_dir_all(&sub).expect("create subdir");
    env.seed_store(&[
        (root.to_str().unwrap(), "http://root:8888", "root", None),
        (
            sub.canonicalize().unwrap().to_str().unwrap(),
            "http://sub:8888",
            "sub",
            None,
        ),
    ]);
    let deep = sub.join("2024").join("jan");
    fs::create_dir_all(&deep).expect("create deep subdir");

    let result = env.run_in(&deep, &["status"]).assert_success();
    assert!(
        result.stdout.contains("http://sub:8888"),
        "the deepest recorded root must win\nStdout: {}",
        result.stdout
    );
}

#[test]
fn test_disconnect_from_subdirectory_clears_project_connection() {
    let env = ConfigTestEnv::new();
    let root = env.temp_dir.path().canonicalize().unwrap();
    env.seed_store(&[(root.to_str().unwrap(), "http://127.0.0.1:8888", "tok", None)]);
    let sub = env.temp_dir.path().join("notebooks");
    fs::create_dir_all(&sub).expect("create subdir");

    env.run_in(&sub, &["disconnect"]).assert_success();

    let store = env.read_store().expect("store must exist");
    assert!(
        store["projects"].as_object().map(|m| m.is_empty()).unwrap_or(false),
        "disconnect from subdir must clear the project connection\nStore: {}",
        store
    );
}

#[test]
fn test_connect_from_subdirectory_updates_project_connection() {
    let env = ConfigTestEnv::new();
    let root = env.temp_dir.path().canonicalize().unwrap();
    env.seed_store(&[(root.to_str().unwrap(), "http://127.0.0.1:8888", "old", None)]);
    let sub = env.temp_dir.path().join("notebooks");
    fs::create_dir_all(&sub).expect("create subdir");

    env.run_in(
        &sub,
        &[
            "connect",
            "--server",
            "http://127.0.0.1:9999",
            "--token",
            "new",
            "--skip-validation",
        ],
    )
    .assert_success();

    let store = env.read_store().expect("store must exist");
    let projects = store["projects"].as_object().expect("projects map");
    assert_eq!(
        projects.len(),
        1,
        "connect from subdir must update the project entry, not fork a new one\nStore: {}",
        store
    );
    assert_eq!(
        projects[root.to_str().unwrap()]["server_url"].as_str(),
        Some("http://127.0.0.1:9999"),
        "project connection must be updated\nStore: {}",
        store
    );
    assert!(
        !sub.join(".jupyter").exists(),
        "connect must never create a config inside the project tree"
    );
}

#[test]
fn test_connect_with_empty_store_creates_entry_keyed_by_cwd() {
    let env = ConfigTestEnv::new();
    // No store entry anywhere; connect records the cwd as the project root.
    env.run(&[
        "connect",
        "--server",
        "http://127.0.0.1:9999",
        "--token",
        "tok",
        "--skip-validation",
    ])
    .assert_success();

    let store = env.read_store().expect("store must exist");
    let root = env.temp_dir.path().canonicalize().unwrap();
    assert_eq!(
        store["projects"][root.to_str().unwrap()]["server_url"].as_str(),
        Some("http://127.0.0.1:9999"),
        "connect must record the cwd as the project root\nStore: {}",
        store
    );
    assert!(
        !env.temp_dir.path().join(".jupyter").exists(),
        "connect must not write into the project tree"
    );
}

// ==================== SECURITY TESTS ====================
//
// The store lives in the user's home directory; files inside the project tree
// are never read, so nothing shipped inside a repository can steer `nb` at a
// server. The remaining risk is the store file itself: it holds tokens, so a
// planted world-writable store must not receive a freshly typed one.

#[test]
fn test_connect_refuses_to_overwrite_untrusted_store() {
    use std::os::unix::fs::PermissionsExt;
    let env = ConfigTestEnv::new();
    // Attacker-planted world-writable store: `nb connect` must refuse to
    // write the freshly typed token into it instead of doing so silently.
    fs::write(env.store_path(), "attacker").expect("write store");
    fs::set_permissions(env.store_path(), fs::Permissions::from_mode(0o666)).expect("chmod");

    let result = env.run(&[
        "connect",
        "--server",
        "http://127.0.0.1:9999",
        "--token",
        "new",
        "--skip-validation",
    ]);
    assert!(
        !result.success,
        "connect must fail instead of writing into an untrusted store\nStdout: {}\nStderr: {}",
        result.stdout, result.stderr
    );
    assert!(
        result.stderr.contains("refusing to write connection store"),
        "Stderr: {}",
        result.stderr
    );
    // The attacker's file must be untouched.
    let content = fs::read_to_string(env.store_path()).expect("read store");
    assert_eq!(content, "attacker", "untrusted store must not be overwritten");
}
