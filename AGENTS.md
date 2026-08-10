# Agent Guidelines

## Working with Notebooks (.ipynb files)

When the user asks to read, edit, execute, or work with .ipynb files, use the notebook-cli skill, which provides the `nb` command-line tool. Do not use the built-in Read/Write tools for `.ipynb` files.

## Connect-mode integration tests: backend selection

`tests/integration_connect_mode.rs` exercises connect-mode against whatever
collaboration backend is installed in the active test venv. `jupyter-collaboration`
and `jupyter-server-documents` (JSD) are competing collaborative-editing server
extensions and **must never be installed into the same venv** — each has its own:

- `tests/.test-venv` — JSD + local-mode tests (default). Pinned:
  `jupyter_server==2.20.0`, `jupyter-server-documents==0.2.5`.
- `tests/.test-venv-collab` — jupyter-collaboration. Pinned:
  `jupyter_server==2.20.0`, `jupyter-collaboration==4.4.1`.

Set up a venv with `./tests/setup_test_env.sh [jsd|jupyter-collaboration]`
(defaults to `jsd`). Select which backend a test run targets with
`NB_TEST_BACKEND=<jsd|jupyter-collaboration>` (read by `test_helpers::test_backend()`);
this also picks the matching venv directory automatically. Run with:

```
NB_TEST_BACKEND=jupyter-collaboration cargo test --test integration_connect_mode -- --test-threads=1
```

The shared Jupyter server is spawned once per test process (`OnceLock`) with its
`current_dir` set to a tempdir root, so backend-specific artifacts like
jupyter-collaboration's `.jupyter_ystore.db` land there instead of the crate
root. On teardown, an `atexit` hook calls `jupyter server stop <port> -y` to
cleanly shut down the server. Each notebook-executing test also explicitly
deletes its Jupyter session/kernel via `DELETE /api/sessions/{id}` when its
`NotebookSession` guard drops (production code intentionally never deletes
sessions, so tests must do this themselves).

**Known state (2026-07-05):** against `jupyter-collaboration`, the 4
execute/restart tests (what PR #99 / issue #92 fixed — FileID fallback, `sessionId`
on the Y.js room WS handshake, v1 kernel-WS subprotocol, client-side output
writing) pass 10/10 runs with zero flakiness, and gate the `test-connect-collab`
CI job. `test_clear_outputs_in_connect_mode` and
`test_clear_outputs_specific_cell_in_connect_mode` are marked `#[ignore]`
against jupyter-collaboration (issue #100): `nb output clear` correctly edits
the Y.js room, but `jupyter_server_ydoc` only flushes the room to disk on a
debounced ~1s timer (`document_save_delay`), so `nb read` immediately
afterward races that debounce and can observe stale content — confirmed by
direct measurement (still stale at +0.7s, cleared by +1.7s). This is a
**different** root cause from #90 (JSD's clear never persists, permanently,
because externalized output files get unconditionally re-materialized into
the notebook on every save) — don't conflate the two if either gets fixed.

## Connection store (`~/.config/nb/connections.json`) keys connections by project root

`nb connect` records the connection in the user's connection store
(`~/.config/nb/connections.json`, dir overridable with `NB_CONFIG_HOME`),
keyed by the canonical project root where it ran. `Config::load` resolves the
connection for the cwd by taking the recorded root that is the longest
component-wise ancestor-or-equal of the cwd (`Path::starts_with` semantics,
so `/a/b` never matches `/a/bc`); `Config::save` writes back under that same
root (or the cwd when no entry matches), so connecting from a subdirectory
updates the project connection. This is why running `nb` in a notebook subdir
still uses the project connection. A `None` connection removes the entry
(`nb disconnect`).

Security: the store lives in the user's home, never in the project tree — no
token is written into (and no connection is read from) files that ship with a
repository. Any file the user materializes (a git checkout, an extracted
archive) becomes user-owned with mode 0644, so a connection file shipped
inside a cloned repo would pass every ownership/permission check and silently
redirect `nb` at an attacker's server — and `nb connect` would write the real
token into the repo. The store avoids the whole class by never consulting
project files. The trust checks remain for the store `nb` does consult: it
must be a regular file (lstat, so symlinks are rejected), owned by the current
uid, and not group/world-writable; `load` re-checks via fstat on the opened fd
(that fstat is the only TOCTOU measure, don't add directory-level checks
back), and `save` refuses to overwrite an existing file that fails the checks.
`libc` is a Unix-only dependency for `getuid`.

## Remote mode and server base_url

`nb connect` stores the server URL exactly as given, including any base_url
path prefix (e.g. `https://host/jupyter`), and every endpoint — REST API,
Contents API, kernel WS, Y.js room WS, FileID index — lives under that prefix.
Two URL-building pitfalls in `src/execution/server/ydoc.rs` were fixed for
this (2026-08):

- Never rebuild a URL from `scheme://host:port`, and never replace the path
  with `Url::set_path()`: both silently drop the base_url prefix. Append route
  segments onto the existing path with `path_segments_mut()` instead (same
  pattern as `contents_url` in `client.rs`).
- jupyter-collaboration registers its routes as Tornado regexes
  `/api/collaboration/session/(.*)` and `/api/collaboration/room/(.*)`, so the
  trailing slash after `session`/`room` is part of the match. The connect-time
  probe calls `get_file_id` with an empty path; dropping the slash makes it
  404 and the backend is silently misdetected as absent ("Mode: direct").
  Verify route changes against the regexes in `jupyter_server_ydoc/app.py`.
