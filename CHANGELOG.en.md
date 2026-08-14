# Changelog

All notable changes to this project will be documented in this file.

- 中文版更新日志：[`CHANGELOG.md`](./CHANGELOG.md)

## [0.6.0] - 2026-08-15

### Added
- In-app update checks: the sidebar version badge opens the "Application update" dialog for manual checks and release notes
- Automatic update checks: a silent background check the first time the window opens after 08:00 local time each day; new versions light up the version badge and show a notice, with an opt-out in settings
- Download and restart updates: packages are Ed25519-signature and SHA-256 verified, then replaced by the new `LogcatX.Updater.exe` helper after the app exits, with rollback copies kept until the new build starts successfully
- "Later" silences the notice for the offered version until a newer one is published

### Changed
- The release is now a single portable zip (`LogcatX_<version>_windows_x64_portable_<commit>.zip` containing the app, update helper, and docs); the bare exe is no longer uploaded separately
- GitHub Release notes are now extracted from the matching `CHANGELOG.md` section instead of the annotated tag message
- CI releases now build natively on windows-latest and run tests on tag builds
- 0.5.x and earlier builds ship without the update helper: download this zip manually one last time; later versions can update in-app
- Signing key generation and repository setup live in `docs/update-signing.md`; the public key is injected at build time via `LOGCATX_UPDATE_PUBLIC_KEY`, and builds without it report "Application updates are not configured in this build" and never contact update servers

## [0.5.3] - 2026-06-06

### Added
- "Copy latest log file path" action in the device more menu to copy the current or most recent log file path to clipboard
- Per-device logcat command arguments (e.g. `-v threadtime -s Tag:V *:E`) persisted in config and applied automatically on reconnection
- Logcat arguments edit dialog with save and clear actions

## [0.5.2] - 2026-05-12

### Added
- a minimum main-window size constraint so the UI cannot be resized down to an unusable state

### Fixed
- clearing foreground-app data now retries via `run-as <package> pm clear <package>` when direct `pm clear` fails with the expected permission error
- version bumped to `0.5.2`

## [0.5.1] - 2026-05-09

### Added
- a new default device-name rule: alias > manufacturer + model > serial
- current foreground-app detection directly from the device list
- foreground-app quick actions for force-stop, clear data, and uninstall
- confirmation dialogs for destructive foreground-app actions
- automatic grouping of USB and Wi-Fi ADB transports that belong to the same physical device

### Changed
- removed the dedicated "Selected device" detail panel to further simplify the Devices page
- moved device-alias editing into the device-row `More` menu
- updated the device-list hint to emphasize row selection as the default drop target plus `More` for device actions
- when the same device is available over both USB and Wi-Fi, the device list now prefers the USB transport
- version bumped to `0.5.1`

## [0.5.0] - 2026-05-06

### Added
- a redesigned main window with a left sidebar and a main content area
- a fixed-height (150px) bottom log panel on the Devices page, independent of the main content scroll area
- direct device-serial copy actions in both the list and the detail panel
- direct device-shell launch actions in both the list and the detail panel
- direct disconnect actions for network devices from the device list
- a one-click ADB Server restart action in the UI
- drag-and-drop APK installation onto a target device
- drag-and-drop file transfer to `/sdcard/Download`

### Changed
- dropped files now prefer the currently selected device; if none is selected, the app asks for the target device first
- batch drops can process multiple APKs and regular files in one pass
- removed the top toolbar; moved the GitHub button to the bottom of the sidebar
- removed "Settings" and "Clear History" buttons from the action row (functions overlap with the sidebar)
- shrunk overview stat cards for better information density
- changed device-list horizontal scrollbar to always-hidden; horizontal scrolling is now handled by the mouse wheel
- empty device-list card now shrinks to fit content instead of reserving a fixed height
- version bumped to `0.5.0`

## [0.4.0] - 2026-04-29

### Added
- GitHub project homepage button in the main window
- device alias persistence, pinned devices, and recent network connection history in `config.json`
- direct `adb connect` flow for `IP:port` targets with a recent-connections dialog
- friendlier device-state labels in the UI
- Android version display in the device list
- a Google official Platform-Tools download link when ADB is not detected on first launch

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
