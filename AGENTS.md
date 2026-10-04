# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Collaboration rules

- **Do not start coding without explicit instructions.** If the user's request is exploratory or ambiguous, ask for clarification instead of jumping into implementation.
- **Confirm before irreversible operations.** Any action that cannot be easily undone (force push, delete branch, overwrite uncommitted changes, etc.) must be confirmed with the user before execution.

## Build & development commands

```bash
cargo build                              # debug build
cargo check                              # quick compilation check
cargo test                               # run all unit + integration tests
cargo test --lib adb                     # run tests in a specific module
cargo test parse_logcat_args             # run a single test by name
cargo clippy -- -D warnings              # quick lint (the authoritative gate is scripts/ci_gate.sh)
./scripts/ci_gate.sh                     # THE push gate — run before every push (see below)
cargo run                                # run (no console on Windows)
cargo run --features console             # run with console window
cargo run -- --console                   # same, via CLI flag
LOGCATX_DEMO_APP_UPDATE=1 cargo run      # demo the in-app update flow locally (debug builds; see docs/update-signing.md)
```

## Pre-push checklist (mirrors CI)

The push gate has exactly one source of truth — `scripts/ci_gate.sh` (fmt → clippy with CI's flags → tests). `.github/workflows/ci.yml` invokes this same script, so local and CI can never drift apart; if the gate ever changes, edit the script and both sides follow. Run it **before every push**:

```bash
./scripts/ci_gate.sh
```

(0.9.1 taught this the hard way: the fmt gate was undocumented anywhere an agent would look, so local commits went red only after reaching GitHub.)

Real-device smoke (needs a device in `device` state; all cases are `#[ignore]`-gated and skipped otherwise):

```bash
LOGCATX_REAL_ADB="$(where adb)" cargo test --test real_device -- --ignored --test-threads=1
```

The `live_update_channel_*` case additionally needs the update public key at compile time:

```bash
LOGCATX_REAL_ADB="$(where adb)" LOGCATX_UPDATE_PUBLIC_KEY=<key> cargo test --test real_device live_update -- --ignored
```

Release build (cross-compile on Linux/macOS targeting Windows):
```bash
cargo xwin build --target x86_64-pc-windows-msvc --release
```

Package Windows release:
```bash
./scripts/package_windows_release.sh      # builds natively on Windows, cross-compiles elsewhere; produces dist/LogcatX.exe and the portable zip
```

## Architecture

LogcatX is a Windows-first Rust/egui desktop app for collecting `adb logcat` from multiple Android devices in parallel.

### Source modules (`src/`)

The crate is a lib plus a thin bin: `lib.rs` exposes everything (core business logic *and* the egui UI) so `tests/` can drive core logic against the fake adb double without a GUI; `main.rs` is only the eframe bootstrap.

Core (UI-free, reusable by a future non-egui frontend):

- **`adb_executor.rs`** — `AdbExecutor`: every adb invocation goes through it. execute/execute_with_timeout/cancel, `spawn_streaming` (logcat), bounded output capture, kill + reap of children.
- **`adb.rs`** — device discovery: `list_devices` with `DeviceMetadataCache` (getprop metadata cached across polls, pruned on disconnect) and discovery generations (stale replies can't overwrite newer state); logcat spawn; shell/foreground-app commands; APK install.
- **`task.rs`** — `TaskManager`: per-device, per-kind task lifecycle (replaces global `*_in_progress` bools; one device's operation never blocks another's).
- **`transfer.rs`** — `TransferManager`: transfer queue with per-device concurrency, progress parsing, cancel (= kill + reap), retry, stall supervision. UI clones a handle and polls `snapshot()`.
- **`remote_fs.rs`** — Files backend: `RemotePath` validation, `shell_quote` escaping, the machine-parsed listing protocol (POSIX `[ -d ]/[ -L ]/[ -f ]` kind markers + `stat -c '%s %Y'`; tolerates CRLF/pty output), mkdir/rename/move/delete, and run-as browsing (`run-as <pkg> sh -c '<script>'` — never a bare script argument; adb joins args with spaces and devices exec the first word).
- **`wireless.rs`** (+ `wireless/tests.rs`) — wireless-debugging pair/connect, sharing the same `AdbExecutor` process management.
- **`managed_child.rs`** — child-process handle with reliable kill + reap.
- **`models.rs`** — core data types: `DeviceInfo`, `DeviceEntry` (transport merge), `AppEvent` mpsc enum, dialog state.
- **`config.rs`** — `AppConfig` (serde) with atomic persistence; portable-vs-AppData via `desktop-config`; 0.8→0.9 migration.
- **`fs_utils.rs`**, **`i18n.rs`**, **`updater.rs`**, **`build_info.rs`** — log-dir management/cleanup safety; `desktop-i18n` wrapper (`locales/*.json`); signed in-app updates (see below); compile-time version/commit identity.
- **`scrcpy.rs`** — scrcpy version detection and screen mirroring integration.

UI layer (the only modules allowed to import egui/eframe):

- **`app.rs`** + **`app/`** (`connection.rs` wireless UI, `files.rs` Files page, `text_menu.rs` right-click text menus) — the `eframe::App` implementation: all pages, dialogs, menus, and background task orchestration via mpsc.
- **`ime.rs`** — Windows IME workaround for egui 0.31 single-line text edits.
- **`e2e.rs`** — `--features e2e` native renderer screenshots for GUI E2E (never in release artifacts).

Binaries:

- **`bin/logcatx-updater.rs`** — update helper shipped beside `LogcatX.exe`; performs the post-exit file replacement and restart for updates.
- **`bin/fake_adb.rs`** — scriptable adb double (`FAKE_ADB_SCRIPT` scenario files) driving the integration tests.

### Key patterns

- **Core/UI boundary**: business modules (everything under "Core" above) must not import egui/eframe or `crate::app`; they emit plain Rust data + mpsc events and the egui layer adapts them. This is a standing constraint: 0.10.0 will replace the egui UI with Tauri while reusing this core — do not add UI types to core modules, and do not build speculative GUI abstractions inside 0.9.0 either.
- **Portable mode**: If `config.json` exists beside the exe and the directory is writable, the app runs in portable mode. Otherwise it falls back to `%APPDATA%/LogcatX`. Handled by `desktop-config::PortableAppPaths`.
- **Background ADB**: Logcat collection runs in spawned child processes. The UI polls for output via mpsc channels. Device list refresh also happens asynchronously.
- **Device identity**: transports merge on `identity_key`, which is the device's `ro.serialno` (fallback `ro.boot.serialno`), NOT manufacturer+model. USB is preferred when both USB and wireless are present; unresolved/rotating wireless endpoints fold by host. All shell scripts must assume POSIX sh with possible CRLF pollution.
- **i18n**: All user-visible strings go through the `I18n` struct. Translation files are in `locales/`. CJK fonts are loaded on startup.
- **Application updates**: checks verify a detached Ed25519 signature over the Raw GitHub manifest (`updates/stable.json` on `master`), run at most once per local day after 08:00 on window focus, and only a fresh signature-verified candidate may be downloaded. The layout allow-list must stay in sync across `desktop-update.toml`, `RELEASE_REPLACE_FILES` in `src/updater.rs`, and the packaging script.
- **Log scope**: the app's logcat feature is capture-to-file + collection status + history management + app diagnostics. Live log rendering / virtualized scrolling / in-app filtering are NOT requirements of 0.9.x or 0.10.x; do not add them speculatively.

### Dependencies

Shared infrastructure comes from the [DeskFoundry](https://github.com/Shawlaw/DeskFoundry) monorepo (`desktop-config`, `desktop-fs`, `desktop-i18n`, `desktop-logger`, `desktop-updater`), pinned by git tag in `Cargo.toml`.

### Windows resources

`build.rs` generates a `.rc` file at compile time embedding the icon and version info from `icons/icon.ico`. The resource compiler lookup is a four-level fallback — `RC` env var, then `llvm-rc` on PATH, then `llvm-rc-20`, then the newest Windows SDK `rc.exe` under `C:\Program Files (x86)\Windows Kits\10\bin\<ver>\<arch>\` — so **llvm-rc is NOT required**: any machine with an MSVC toolchain (which always ships the Windows SDK) embeds resources automatically. The `where`/`which` probes tolerate MSYS-style paths and missing `.exe` suffixes (Git Bash on CI). If every level fails the exe still builds, ships without icon/version metadata, and the release workflow fails tag builds whose `LogcatX.exe` lacks embedded resources.

**Never claim resources are missing from a binary without checking the binary.** Verify with:

```powershell
[System.Diagnostics.FileVersionInfo]::GetVersionInfo('<path>\LogcatX.exe') | Select FileVersion, ProductName
```

(The icon and VERSIONINFO are compiled from the same .rc in one step — version info present ⇒ icon present.)

## Tests

- Inline `#[cfg(test)]` unit tests in each source file (heaviest: `adb.rs` parsing, `config.rs`, `updater.rs`, device-merge logic in `app.rs`).
- `tests/` integration suites against the fake adb double: `fake_adb_harness.rs`, `adb_executor.rs`, `remote_fs.rs`, `transfer.rs`, and `update_helper.rs` (drives the real `logcatx-updater` binary through apply/ack/rollback).
- `tests/real_device.rs`: `#[ignore]`-gated smoke against real hardware — discovery/identity, listing protocol, file-op roundtrips, logcat capture, multi-device isolation (parallel pushes must not mix), run-as browsing, dual-transport (USB+wireless) identity, and a live signed-manifest check. Requires `LOGCATX_REAL_ADB`; multi-device cases soft-skip below two devices.
- Run individual tests with `cargo test <test_name>`; the real-device suite as shown in the commands section above.

## Release process

Pushing a tag matching `v*` triggers `.github/workflows/release.yml`: it validates the tag against `Cargo.toml` version, runs tests, builds the flat portable zip on `windows-latest` (embedding the update public key from the `LOGCATX_UPDATE_PUBLIC_KEY` repo variable), extracts the GitHub Release notes from the matching `## [version]` section of `CHANGELOG.md`, publishes the zip as the single release asset, and — when signing is configured — signs and commits `updates/stable.json(.sig)` to `master` via the DeskFoundry `publish-portable-update` action. Signing key setup lives in `docs/update-signing.md`; bump the version in `Cargo.toml` and both changelogs before tagging. Commits stay local until the user decides to push.

### Pre-tag checklist (mirrors release.yml)

`release.yml` is the source of truth for the release flow, and its hard validations are packaged as `scripts/pre_release_check.sh` — which the workflow itself also runs, so local and CI check the same thing. Before tagging:

1. **Gate green**: `./scripts/ci_gate.sh` passes on the commit to be tagged (release.yml runs the tests again regardless).
2. **Release preconditions**: `./scripts/pre_release_check.sh vX.Y.Z` — tag ↔ `Cargo.toml` version match, and both changelogs carry the `## [X.Y.Z] - <date>` section the notes extraction reads. Sync the README milestone lines and `plans/todo.md` in the same bump commit.
3. **Packaging dry run** (when `build.rs`, resources or packaging changed): `./scripts/package_windows_release.sh` on Windows, then confirm `dist/LogcatX.exe` reports `FileVersion == X.Y.Z.0` (the workflow also smoke-tests the zipped artifact's `--version`).
4. **Tag annotated, then watch**: `git tag -a vX.Y.Z -m "Release vX.Y.Z" && git push origin vX.Y.Z`, then `gh run watch $(gh run list --workflow=release.yml --limit 1 --json databaseId -q '.[0].databaseId') --exit-status` — confirm the run instead of assuming, and `git pull --ff-only` afterwards to pick up the bot's `updates/` publish commits.
