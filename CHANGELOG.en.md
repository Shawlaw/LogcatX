# Changelog

All notable changes to this project will be documented in this file.

- 中文版更新日志：[`CHANGELOG.md`](./CHANGELOG.md)

## [0.9.0] - 2026-09-22

### Added
- Device files page: browse device directories with breadcrumbs, path jumping, name/size/time sorting, multi-select, quick paths (/sdcard, Download, DCIM, Pictures, Documents, /data/local/tmp) and persistent directory favorites
- Unified transfer queue for upload/download with percentage, transferred bytes and live speed; per-task cancel, cancel-all, failed retry and clear-finished; identical behavior over USB and wireless
- New folder, rename, move and delete (one confirmation for batch deletes showing the item count, with an extra warning when directories are included)
- Drop files onto the Files page to upload into the current directory; dropped APKs offer install-or-upload choice
- run-as browsing of debuggable app data (/data/data), clearly labeled as app data (run-as) instead of regular filesystem access

### Enhanced
- Unified ADB execution core: every adb call now has timeouts, cancellation, bounded output capture and child reaping, so a hanging command can no longer freeze the UI; wireless pair/connect share the same process management
- Static device metadata (manufacturer/model/OS/serial) is cached across polls, ending the per-poll getprop storm; stale discovery replies can no longer overwrite newer state
- Update experience: signature-verified update candidates persist across restarts (download-ready without another network check); the update dialog shows release notes inline (fetch failure never blocks download/install, with retry and view-release fallback); automatic checks back off 3-6h on network failure and 24h on invalid manifests; the applied-update ACK is sent only after a healthy start (first rendered frame), keeping rollback material when bootstrap fails
- Diagnostics carry the build commit: settings and the update dialog show v0.9.0 (commit), and `LogcatX.exe --version` works from the command line; the main UI stays version-only
- Plain file drops no longer block: they enqueue immediately and multiple batches can be in flight

### Fixed
- Log cleanup never follows symlinks/junctions and can never delete outside the configured log root; logcat sessions started mid-cleanup are protected by a live registry check
- Configuration and update state are written atomically, so an interrupted write no longer corrupts files; 0.8 configs migrate seamlessly and missing new fields never block startup
- Windows numeric FILEVERSION now mirrors the semantic version (0.9.0.0) instead of the build date
- Removed the incorrect `run-as <pkg> pm clear` fallback; clear-data failures now surface their real reason

## [0.8.0] - 2026-09-17

### Added
- Wireless debugging discovery, six-digit pairing and automatic connection after pairing; wireless history rediscovers the current dynamic port
- Quick and full TCP port scans for one device IP, with ADB handshake detection, progress, cancellation and manual connection fallback
- Dropping APKs now asks whether to install directly or send to the download directory, with a per-device "always install directly" option that skips further confirmations; the device more menu can restore the confirmation at any time

### Enhanced
- Single-file drops show the full install/push path and destination in the running messages; batch drops show a summary in the UI with per-file details in the application log
- Drag-and-drop install recognizes re-downloaded files such as `.apk.1` / `.apk.2` (browser numeric suffixes) as APKs; they are staged under an `.apk` suffix before installing to satisfy adb's filename check
- Connection addresses and pairing codes accept Chinese punctuation and full-width characters; legacy TCP/IP connections default to port 5555, with normalized-address previews and inline validation
- Stronger device-row selection background and blue border make the current target easier to identify
- Checkboxes draw an accent-colored check mark so the checked state is clearly visible; the manual wireless-debugging option now spells out its behavior

## [0.7.0] - 2026-09-04

### Added
- The device more menu can capture the current device screen and copy it to the Windows clipboard as an image
- A scrcpy submenu in the device more menu can mirror a device or create an independent virtual display; the display can follow the primary screen or use a portrait `720×1600` / `320 DPI` configuration, with a package-name field and filter for launch targets
- Historical log storage management can report usage, preview cleanup by device or time range, and protect actively written logs
- Application updates support automatic or custom HTTP / SOCKS5 proxies and settings-page proxy-configuration testing

### Changed
- The device-menu “Open” action is now labelled “Open latest log file” to make its intent explicit

### Fixed
- Fixed release packages that could be missing the application icon and version information on Windows

## [0.6.1] - 2026-08-27

### Fixed
- Fixed single-line text fields losing focus (and possibly dropping the committed text) when pressing Enter to confirm an IME composition, such as committing raw pinyin with Sogou; affected the device alias, logcat arguments, connect target and settings fields

## [0.6.0] - 2026-08-18

### Added
- In-app update checks: click the sidebar version badge to check for a new version and open the release notes
- Automatic update checks (on by default, can be disabled in settings): a silent check the first time the window opens after 08:00 local time each day; new versions light up the version badge and show a notice
- One-click "download and restart" updates: the package is signature-verified, installed automatically, and the app restarts into the new version; a failed startup rolls back to the previous version
- The update notice can be dismissed with "Later" until a newer version is published

### Changed
- The release is now a single portable zip (app plus docs); the bare exe is no longer published separately. 0.5.x and earlier builds need one final manual download of this zip; later versions update in-app
- Dropping APKs/files onto the window now names the default target device in the hover hint, making misdirected drops less likely with multiple devices

### Fixed
- Fixed garbled log directory and file names when a device alias contains Unicode characters such as Chinese
- All adb logcat child processes are now guaranteed to terminate when the app exits (including crashes), leaving no stray adb processes behind

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
