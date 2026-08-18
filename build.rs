fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let version_parts: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();
    let v_major = version_parts.first().copied().unwrap_or(0);
    let v_minor = version_parts.get(1).copied().unwrap_or(0);
    let v_patch = version_parts.get(2).copied().unwrap_or(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let total_days = secs / 86400;
    let build_hour = ((secs % 86400) / 3600) as u32;
    let (build_year, build_month, build_day) = days_to_ymd(total_days);

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let icon_path = format!("{manifest_dir}/icons/icon.ico");
    let rc_content = format!(
        r#"1 ICON "{icon_path}"

1 VERSIONINFO
FILEVERSION {build_year},{build_month},{build_day},{build_hour}
PRODUCTVERSION {v_major},{v_minor},{v_patch}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "080404b0"
        BEGIN
            VALUE "CompanyName", "LogcatX"
            VALUE "FileDescription", "LogcatX - multi-device adb logcat GUI"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "logcatx"
            VALUE "OriginalFilename", "LogcatX.exe"
            VALUE "ProductName", "LogcatX"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0804, 1200
    END
END
"#
    );

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let rc_path = format!("{out_dir}/resource.rc");
    std::fs::write(&rc_path, &rc_content).expect("failed to write generated rc file");

    let rc = std::env::var("RC")
        .ok()
        .or_else(|| which("llvm-rc"))
        .or_else(|| which("llvm-rc-20"))
        .and_then(|found| normalize_which_path(&found));

    let Some(rc) = rc else {
        println!("cargo:warning=resource compiler not found, skipping Windows icon embedding");
        return;
    };

    let res_path = format!("{out_dir}/resource.res");
    let status = match std::process::Command::new(&rc)
        .arg("-no-preprocess")
        .arg(&rc_path)
        .arg("/FO")
        .arg(&res_path)
        .status()
    {
        Ok(status) => status,
        Err(error) => {
            println!(
                "cargo:warning=failed to run resource compiler {rc}: {error}; exe will not include icon metadata"
            );
            return;
        }
    };

    if status.success() {
        println!("cargo:rustc-link-arg={res_path}");
    } else {
        println!("cargo:warning=resource compilation failed, exe will not include icon metadata");
    }
}

/// Git Bash's `which.exe` (first on PATH on GitHub Windows runners) reports
/// MSYS-rooted paths like `/c/Program Files/LLVM/bin/llvm-rc` — without the
/// `.exe` suffix — which the Windows loader cannot spawn and which `is_file`
/// does not resolve. Translate those to `C:/...`, retrying with the suffix,
/// and reject the lookup otherwise.
fn normalize_which_path(found: &str) -> Option<String> {
    let found = found.trim();
    if !found.starts_with('/') {
        return Some(found.to_owned());
    }
    let bytes = found.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b'/' && bytes[0].is_ascii_alphabetic() {
        let plain = format!("{}:{}", bytes[0] as char, &found[2..]);
        if std::path::Path::new(&plain).is_file() {
            return Some(plain);
        }
        let with_exe = format!("{plain}.exe");
        if std::path::Path::new(&with_exe).is_file() {
            return Some(with_exe);
        }
    }
    None
}

fn days_to_ymd(mut days: i64) -> (u32, u32, u32) {
    let mut y = 1970i64;
    loop {
        let dy = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let md: &[u32] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0u32;
    for (i, &d) in md.iter().enumerate() {
        if days < d as i64 {
            m = i as u32 + 1;
            break;
        }
        days -= d as i64;
    }
    if m == 0 {
        m = 12;
    }
    (y as u32, m, days as u32 + 1)
}

fn which(name: &str) -> Option<String> {
    // `where.exe` reports native Windows paths, while `which` (Git Bash on CI
    // runners) answers with MSYS-rooted paths and possibly several matches.
    if cfg!(windows) {
        if let Some(found) = locate_with("where", name) {
            return Some(found);
        }
    }
    locate_with("which", name)
}

fn locate_with(tool: &str, name: &str) -> Option<String> {
    let output = std::process::Command::new(tool)
        .arg(name)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}
