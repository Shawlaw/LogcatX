# Changelog

All notable changes to this project will be documented in this file.

## [0.2.0] - Unreleased

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
