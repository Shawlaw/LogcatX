//! LogcatX core library.
//!
//! The binary target (`src/main.rs`) is a thin eframe bootstrap; everything
//! else — adb integration, app UI, config, updater — lives here so
//! integration tests (`tests/`) can exercise core logic against the
//! `fake_adb` double without a real device (PRD §43).

pub mod adb;
pub mod adb_executor;
pub mod app;
pub mod build_info;
pub mod config;
pub mod fs_utils;
pub mod i18n;
pub mod ime;
pub mod managed_child;
pub mod models;
pub mod remote_fs;
pub mod scrcpy;
pub mod task;
pub mod transfer;
pub mod updater;
pub mod wireless;

// Opt-in native renderer screenshots for E2E on locked / remote Windows
// desktops; excluded from ordinary preview / release artifacts.
#[cfg(feature = "e2e")]
pub mod e2e;
