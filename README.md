# ADB Logcat Collector

A small desktop GUI tool for collecting `adb logcat` logs from multiple Android devices in parallel.

## Release target

- platform: **Windows**
- distribution: **portable zip + single exe**
- current milestone: **v0.2.0**

## Core capabilities

- show currently connected ADB devices in the main window
- start logcat collection by double-clicking a device row
- stop collection manually per device
- collect from multiple devices in parallel
- configure the `adb` executable path and the device-log output directory
- refresh device list and historical log size
- clear historical device logs while protecting active sessions
- write a separate **application runtime log** for diagnostics
- embedded English and Simplified Chinese UI with system-language default on first launch

## Portable behavior

The Windows release prefers `config.json` next to the exe, and falls back to AppData when the exe directory is not writable.

This keeps the normal zip download usable as a portable tool while still working in restricted directories.

## Logs

The app writes two different kinds of logs:

1. **Device logs** — the `adb logcat` output collected from Android devices
2. **Application log** — startup/runtime diagnostics for the collector itself

The application log is stored separately from device logcat output.

## First run

On first launch, the app asks the user to confirm:

- the `adb` executable path
- the directory used to store collected device logs

The settings dialog supports:

- choosing the UI language (English / Simplified Chinese)
- opening the config directory directly
- opening the application runtime log directly
- showing whether the app is currently running in portable mode or AppData mode

## Configuration example

See `config.example.json` for the current config shape.

## Windows build

```bash
cargo xwin build --target x86_64-pc-windows-msvc --release
```

## Windows release packaging

```bash
./scripts/package_windows_release.sh
```

This produces:

- `dist/adb-logcat-collector.exe`
- `dist/adb-logcat-collector-v0.2.0-win64.zip`

## Troubleshooting

- If normal startup shows no console, that is expected for the Windows GUI build.
- For troubleshooting builds, enable the console feature or run with `--console`.
- If device collection fails, check the configured `adb` path and the application runtime log first.
