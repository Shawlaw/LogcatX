use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::Mutex,
};

struct FileAndConsoleLogger {
    file: Mutex<File>,
    log_path: std::path::PathBuf,
    max_size_bytes: u64,
    console_mode: bool,
}

impl log::Log for FileAndConsoleLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let msg = format!("[{} {}] {}", now_timestamp(), record.level(), record.args());

        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{msg}");
            let _ = file.flush();

            if let Ok(meta) = file.metadata() {
                if meta.len() > self.max_size_bytes {
                    let old_path = self.log_path.with_extension("log.old");
                    let _ = std::fs::rename(&self.log_path, &old_path);
                    if let Ok(new_file) = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.log_path)
                    {
                        *file = new_file;
                    }
                }
            }
        }

        if self.console_mode {
            if record.level() <= log::Level::Warn {
                eprintln!("{msg}");
            } else {
                println!("{msg}");
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

pub fn init(log_path: &Path, console_mode: bool, max_log_size_mb: u32) -> Result<(), String> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "Failed to create logger directory {}: {err}",
                parent.display()
            )
        })?;
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|err| format!("Failed to create app log {}: {err}", log_path.display()))?;

    let logger = Box::new(FileAndConsoleLogger {
        file: Mutex::new(file),
        log_path: log_path.to_path_buf(),
        max_size_bytes: std::cmp::max(max_log_size_mb, 1) as u64 * 1024 * 1024,
        console_mode,
    });

    log::set_boxed_logger(logger).map_err(|err| format!("Failed to install logger: {err}"))?;
    log::set_max_level(log::LevelFilter::Debug);
    Ok(())
}

pub fn set_panic_hook(log_path: &Path) {
    let log_path = log_path.to_path_buf();
    std::panic::set_hook(Box::new(move |info| {
        let timestamp = now_timestamp();
        let msg = format!("[{timestamp} ERROR] PANIC: {info}");

        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = writeln!(file, "{msg}");
            let _ = writeln!(
                file,
                "[{timestamp} ERROR] Backtrace:\n{:?}",
                std::backtrace::Backtrace::capture()
            );
        }

        eprintln!("{msg}");
    }));
}

fn now_timestamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}
