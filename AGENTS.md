# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Collaboration rules

- **Do not start coding without explicit instructions.** If the user's request is exploratory or ambiguous, ask for clarification instead of jumping into implementation.
- **Confirm before irreversible operations.** Any action that cannot be easily undone (force push, delete branch, overwrite uncommitted changes, etc.) must be confirmed with the user before execution.

## Build & development commands

```bash
cargo build                              # debug build
cargo check                              # quick compilation check
cargo test                               # run all unit tests
cargo test --lib adb                     # run tests in a specific module
cargo test parse_logcat_args             # run a single test by name
cargo clippy -- -D warnings              # lint
cargo run                                # run (no console on Windows)
cargo run --features console             # run with console window
cargo run -- --console                   # same, via CLI flag
```

Release build (cross-compile on Linux/macOS targeting Windows):
```bash
cargo xwin build --target x86_64-pc-windows-msvc --release
```

Package Windows release:
```bash
./scripts/package_windows_release.sh      # produces dist/LogcatX.exe and a zip
```

## Architecture

LogcatX is a Windows-first Rust/egui desktop app for collecting `adb logcat` from multiple Android devices in parallel.

### Source modules (`src/`)

- **`main.rs`** — Entry point. Resolves config paths, initializes logging and i18n, launches the eframe window. The `console` feature flag and `--console` CLI arg control whether a terminal window appears.
- **`app.rs`** (~3500 lines) — The main `eframe::App` implementation. All UI rendering (devices page, settings page, dialogs, menus), application state, and background task orchestration via `std::sync::mpsc`. The `LogcatXApp` struct holds all runtime state.
- **`adb.rs`** — All ADB subprocess interactions: device listing, metadata queries, logcat spawning, shell/foreground-app commands, file push, APK install. Each ADB call spawns a `Command`. Logcat processes are managed as `Child` handles stored per-device.
- **`config.rs`** — `AppConfig` (serde struct) with persistence. `resolve_app_paths()` determines portable vs AppData mode via `desktop-config`. Config is loaded/saved as JSON.
- **`models.rs`** — Core data types: `DeviceInfo`, `ForegroundApp`, `LogcatSession`, `DeviceState`, UI dialog state enums.
- **`fs_utils.rs`** — Log directory management, file sizing, path display helpers, log cleanup.
- **`i18n.rs`** — Thin wrapper around `desktop-i18n`. Loads translations from `locales/en.json` and `locales/zh-CN.json`.

### Key patterns

- **Portable mode**: If `config.json` exists beside the exe and the directory is writable, the app runs in portable mode. Otherwise it falls back to `%APPDATA%/LogcatX`. Handled by `desktop-config::PortableAppPaths`.
- **Background ADB**: Logcat collection runs in spawned child processes. The UI polls for output via mpsc channels. Device list refresh also happens asynchronously.
- **Device identity**: USB and Wi-Fi connections to the same physical device are merged using `identity_key` (derived from manufacturer+model). USB is preferred when both are present.
- **i18n**: All user-visible strings go through the `I18n` struct. Translation files are in `locales/`. CJK fonts are loaded on startup.

### Dependencies

Shared infrastructure comes from the [DeskFoundry](https://github.com/Shawlaw/DeskFoundry) monorepo (`desktop-config`, `desktop-fs`, `desktop-i18n`, `desktop-logger`), pinned by git tag in `Cargo.toml`.

### Windows resources

`build.rs` generates a `.rc` file at compile time embedding the icon and version info from `icons/icon.ico`. It requires `llvm-rc` on the PATH for cross-compilation builds.

## Tests

Tests are inline `#[cfg(test)]` modules in each source file. The heaviest test coverage is in `adb.rs` (parsing tests) and `config.rs` (serialization/default tests). Run individual tests with `cargo test <test_name>`.

## Release process

Pushing a tag matching `v*` triggers `.github/workflows/release.yml`, which validates the tag against `Cargo.toml` version, builds Windows artifacts, and creates a GitHub Release. Bump the version in `Cargo.toml` before tagging.
