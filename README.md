# ADB Logcat Collector

A small desktop GUI tool for collecting `adb logcat` logs from multiple Android devices in parallel.

## Current status

This project is being hardened for a Windows-first public release.

## Core capabilities

- show currently connected ADB devices
- start logcat collection by double-clicking a device row
- stop collection manually
- collect from multiple devices in parallel
- configure `adb` path and log output directory
- refresh device list and historical log size
- clear historical device logs

## Windows build

```bash
cargo xwin build --target x86_64-pc-windows-msvc --release
```

## Configuration

The portable Windows release is designed to prefer `config.json` next to the exe, and fall back to AppData when needed.

See `config.example.json` for the current config shape.
