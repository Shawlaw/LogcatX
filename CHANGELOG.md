# Changelog

All notable changes to this project will be documented in this file.

## [0.4.0] - 2026-04-29

### Added
- GitHub project homepage button in the main window
- device alias persistence, pinned devices, and recent network connection history in `config.json`
- direct `adb connect` flow for `IP:port` targets with a recent-connections dialog
- friendlier device-state labels in the UI

### Changed
- device logs are now grouped by alias-based directories when an alias is set
- log file names now use the alias prefix when an alias is available
- changing a device alias now renames the corresponding historical log directory when possible
- device ordering now respects pinned devices before the normal name sort
- the device list now auto-refreshes when ADB device snapshots change
- device rows now support click-to-select and click-again to clear the selection
- version bumped to `0.4.0`

## [0.3.1] - 2026-04-24

### Changed
- switched shared desktop infrastructure to the public `DeskFoundry` monorepo GitHub dependencies
- `desktop-logger`, `desktop-config`, `desktop-i18n`, and `desktop-fs` are now consumed as reusable SDK crates instead of app-local copies
- version bumped to `0.3.1`

## [0.3.0] - 2026-04-23

### Added
- MIT license for open-source distribution
- embedded English and Simplified Chinese UI resources with persisted language selection
- Chinese README plus English alias document for public repository use

### Changed
- project renamed from `adb-logcat-collector` to `LogcatX`
- version bumped to `0.3.0` for the first public open-source release
- default Windows release artifacts now use the `LogcatX` product name
- main window now emphasizes device status and quick actions instead of exposing raw filesystem paths
- user-facing Windows paths are normalized for display instead of showing `\\?\` verbatim prefixes

### Packaging
- public release output is standardized around `LogcatX.exe` and `LogcatX-v0.3.0-win64.zip`

## [0.2.0] - Internal milestone

### Added
- Windows-first portable release structure
- portable config resolution (exe directory first, AppData fallback)
- dedicated application runtime log with panic capture
- Windows branding resources via `build.rs`, icon assets, and embedded EXE metadata
- version display, config/app-log shortcuts, and richer device session information in the UI
- release planning docs under `plans/`
- release packaging helper script and `cargo xwin` build guidance
- embedded English and Simplified Chinese UI resources with persisted language selection

### Changed
- default project version bumped from `0.1.0` to `0.2.0`
- Windows builds now default to GUI subsystem mode instead of showing a console window
- first-run and settings flows now better explain config paths, app logs, and portable mode
- main window now emphasizes device status and quick actions instead of exposing raw filesystem paths
- user-facing Windows paths are normalized for display instead of showing `\\?\` verbatim prefixes

### Packaging
- release output is standardized around a Windows portable zip containing the exe, README, CHANGELOG, and config example
