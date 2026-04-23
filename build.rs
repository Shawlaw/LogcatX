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
        .or_else(|| which("llvm-rc-20"));

    let Some(rc) = rc else {
        println!("cargo:warning=resource compiler not found, skipping Windows icon embedding");
        return;
    };

    let res_path = format!("{out_dir}/resource.res");
    let status = std::process::Command::new(&rc)
        .arg("-no-preprocess")
        .arg(&rc_path)
        .arg("/FO")
        .arg(&res_path)
        .status()
        .expect("failed to run resource compiler");

    if status.success() {
        println!("cargo:rustc-link-arg={res_path}");
    } else {
        println!("cargo:warning=resource compilation failed, exe will not include icon metadata");
    }
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
    std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !path.is_empty() { Some(path) } else { None }
            } else {
                None
            }
        })
}
