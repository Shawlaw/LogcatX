//! Wireless ADB discovery and bounded, single-host protocol probing.
//! Pairing credentials stay on stdin and are never stored in connection history.
use std::{
    collections::HashSet,
    fmt,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::adb_executor::{AdbError, AdbExecutor, ExecOptions};

// Process management lives in AdbExecutor (PRD §21); wireless reuses the
// shared cooperative cancellation type and re-exports it for existing users.
pub use crate::adb_executor::CancelToken;

#[derive(Debug)]
pub struct Failure {
    pub key: &'static str,
    pub detail: String,
}

impl Failure {
    pub(crate) fn new(key: &'static str, detail: impl ToString) -> Self {
        Self {
            key,
            detail: detail.to_string(),
        }
    }

    pub(crate) fn from_adb_error(error: AdbError) -> Self {
        match error {
            AdbError::Cancelled { .. } => Self::new("connect.error.cancelled", ""),
            AdbError::Timeout { .. } => Self::new("connect.error.timeout", ""),
            other => Self::new("connect.error.command", other.to_string()),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.key {
            "connect.error.timeout" => "ADB command timed out",
            "connect.error.cancelled" => "Task cancelled",
            _ => "ADB operation failed",
        };
        if self.detail.is_empty() {
            write!(f, "{message}")
        } else {
            write!(f, "{message}: {}", self.detail)
        }
    }
}

/// Character-for-character replacements preserve TextEdit's character cursor.
/// Trimming is deferred until validation/submission, never during IME preedit.
pub fn normalize_input(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '。' => '.',
            '\u{3000}' => ' ',
            '\u{ff01}'..='\u{ff5e}' => char::from_u32(c as u32 - 0xfee0).unwrap(),
            _ => c,
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub explicit_port: bool,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

pub fn parse_endpoint(input: &str, require_port: bool) -> Result<Endpoint, &'static str> {
    let normalized = normalize_input(input);
    let value = normalized.trim();
    if value.is_empty() {
        return Err("connect.error.empty");
    }
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']').ok_or("connect.error.address")?;
        if !matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
            return Err("connect.error.address");
        }
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':').ok_or("connect.error.address")?)
        };
        (host, port)
    } else if value.parse::<std::net::Ipv6Addr>().is_ok() {
        (value, None)
    } else if let Some((host, port)) = value.split_once(':') {
        (host, Some(port))
    } else {
        (value, None)
    };
    let host = host.trim();
    if host.is_empty() || host.len() > 253 {
        return Err("connect.error.address");
    }
    let host = if let Ok(ip) = host.parse::<IpAddr>() {
        if ip.is_unspecified()
            || ip.is_multicast()
            || ip == IpAddr::V4(std::net::Ipv4Addr::BROADCAST)
        {
            return Err("connect.error.address");
        }
        ip.to_string()
    } else {
        // Reject malformed numeric IPs instead of passing them to DNS.
        if host.chars().all(|c| c.is_ascii_digit() || c == '.')
            || !host.trim_end_matches('.').split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
        {
            return Err("connect.error.address");
        }
        host.to_ascii_lowercase()
    };
    let explicit_port = port.is_some();
    let port = match port {
        Some(port) => {
            let port = port.trim();
            if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
                return Err("connect.error.port");
            }
            port.parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .ok_or("connect.error.port")?
        }
        None if require_port => return Err("connect.error.port_required"),
        None => 5555,
    };
    Ok(Endpoint {
        host,
        port,
        explicit_port,
    })
}

pub fn parse_scan_ip(input: &str) -> Result<IpAddr, &'static str> {
    let input = normalize_input(input);
    let value = input.trim().trim_start_matches('[').trim_end_matches(']');
    let ip = value.parse::<IpAddr>().map_err(|_| "connect.error.ip")?;
    if ip.is_unspecified() || ip.is_multicast() || ip == IpAddr::V4(std::net::Ipv4Addr::BROADCAST) {
        return Err("connect.error.ip");
    }
    Ok(ip)
}

pub fn parse_pairing_code(input: &str) -> Result<String, &'static str> {
    let code = normalize_input(input).trim().to_owned();
    if code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()) {
        Ok(code)
    } else {
        Err("connect.error.code")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceKind {
    Legacy,
    Connect,
    Pairing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub kind: ServiceKind,
    pub address: SocketAddr,
}

pub fn parse_mdns(output: &str) -> Vec<Service> {
    let mut result = Vec::new();
    for line in output.lines() {
        let cols: Vec<_> = line.split_whitespace().collect();
        if cols.len() != 3 {
            continue;
        }
        let kind = match cols[1].trim_end_matches('.') {
            "_adb._tcp" => ServiceKind::Legacy,
            "_adb-tls-connect._tcp" => ServiceKind::Connect,
            "_adb-tls-pairing._tcp" => ServiceKind::Pairing,
            _ => continue,
        };
        let Ok(address) = cols[2].parse::<SocketAddr>() else {
            continue;
        };
        if address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
            continue;
        }
        if !result
            .iter()
            .any(|s: &Service| s.kind == kind && s.address == address)
        {
            result.push(Service {
                name: cols[0].to_owned(),
                kind,
                address,
            });
        }
    }
    result.sort_by_key(|s| (s.address, s.name.clone()));
    result
}

#[cfg(test)]
pub(crate) mod tests;

pub fn discover(adb_path: &str, cancel: &CancelToken) -> Result<Vec<Service>, Failure> {
    let output = AdbExecutor::new(adb_path)
        .execute_with_options(
            &["mdns", "services"],
            ExecOptions {
                timeout: Some(Duration::from_secs(5)),
                cancel: Some(cancel.clone()),
                ..Default::default()
            },
        )
        .map_err(Failure::from_adb_error)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.success() || !stdout.contains("List of discovered mdns services") {
        return Err(Failure::new(
            "connect.error.discovery",
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(parse_mdns(&stdout))
}

pub fn pair(adb_path: &str, target: &str, code: &str, cancel: &CancelToken) -> Result<(), Failure> {
    let endpoint = parse_endpoint(target, true).map_err(|key| Failure::new(key, ""))?;
    let code = parse_pairing_code(code).map_err(|key| Failure::new(key, ""))?;
    let output = AdbExecutor::new(adb_path)
        .execute_with_options(
            &["pair", &endpoint.to_string()],
            ExecOptions {
                timeout: Some(Duration::from_secs(20)),
                cancel: Some(cancel.clone()),
                stdin_line: Some(code),
                ..Default::default()
            },
        )
        .map_err(Failure::from_adb_error)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.success()
        && stdout
            .to_ascii_lowercase()
            .contains("successfully paired to")
    {
        Ok(())
    } else {
        // Do not propagate command output: some ADB builds echo the pairing code.
        Err(Failure::new("connect.error.pair", ""))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdbKind {
    Legacy,
    Wireless,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub address: SocketAddr,
    pub kind: AdbKind,
}

#[derive(Clone, Debug, Default)]
pub struct ScanProgress {
    pub tested: usize,
    pub total: usize,
    pub timed_out: usize,
    pub done: bool,
    pub cancelled: bool,
    pub deadline_reached: bool,
}

#[derive(Clone, Debug)]
pub enum ScanUpdate {
    Found(Candidate),
    Progress(ScanProgress),
}

pub fn scan_ports(ip: IpAddr, recent: &[String], full: bool) -> Vec<u16> {
    let mut seen = HashSet::new();
    let mut ports = Vec::new();
    let mut add = |port| {
        if seen.insert(port) {
            ports.push(port);
        }
    };
    for target in recent {
        if let Ok(endpoint) = parse_endpoint(target, false)
            && endpoint.host.parse::<IpAddr>().ok() == Some(ip)
        {
            add(endpoint.port);
        }
    }
    add(5555);
    for port in 30000..=65535 {
        add(port);
    }
    if full {
        for port in 1..30000 {
            add(port);
        }
    }
    ports
}

pub struct ScanOptions {
    pub ports: Vec<u16>,
    pub concurrency: usize,
    pub connect_timeout: Duration,
    pub probe_timeout: Duration,
    pub deadline: Duration,
}

impl ScanOptions {
    pub fn for_host(ip: IpAddr, recent: &[String], full: bool) -> Self {
        Self {
            ports: scan_ports(ip, recent, full),
            concurrency: 256,
            connect_timeout: Duration::from_millis(if full { 450 } else { 220 }),
            probe_timeout: Duration::from_millis(650),
            deadline: Duration::from_secs(if full { 180 } else { 45 }),
        }
    }
}

pub fn scan(
    ip: IpAddr,
    options: ScanOptions,
    cancel: &CancelToken,
    mut notify: impl FnMut(ScanUpdate),
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            notify(ScanUpdate::Progress(ScanProgress {
                total: options.ports.len(),
                done: true,
                deadline_reached: true,
                ..Default::default()
            }));
            return;
        }
    };
    runtime.block_on(async {
        let mut progress = ScanProgress {
            total: options.ports.len(),
            ..Default::default()
        };
        let mut ports = options.ports.into_iter();
        let mut tasks = tokio::task::JoinSet::new();
        let start = Instant::now();
        let mut last_update = Instant::now();
        notify(ScanUpdate::Progress(progress.clone()));
        loop {
            if cancel.cancelled() || start.elapsed() >= options.deadline {
                break;
            }
            while tasks.len() < options.concurrency.clamp(1, 512) {
                let Some(port) = ports.next() else {
                    break;
                };
                let address = SocketAddr::new(ip, port);
                tasks.spawn(async move {
                    (
                        address,
                        probe(address, options.connect_timeout, options.probe_timeout).await,
                    )
                });
            }
            if tasks.is_empty() {
                break;
            }
            if let Ok(Some(Ok((address, result)))) =
                tokio::time::timeout(Duration::from_millis(40), tasks.join_next()).await
            {
                progress.tested += 1;
                match result {
                    ProbeResult::Adb(kind) => {
                        notify(ScanUpdate::Found(Candidate { address, kind }))
                    }
                    ProbeResult::Timeout => progress.timed_out += 1,
                    ProbeResult::Other => {}
                }
            }
            if last_update.elapsed() >= Duration::from_millis(100) {
                notify(ScanUpdate::Progress(progress.clone()));
                last_update = Instant::now();
            }
        }
        tasks.abort_all();
        progress.done = true;
        progress.cancelled = cancel.cancelled();
        progress.deadline_reached = !progress.cancelled && progress.tested < progress.total;
        notify(ScanUpdate::Progress(progress));
    });
}

enum ProbeResult {
    Adb(AdbKind),
    Other,
    Timeout,
}

async fn probe(
    address: SocketAddr,
    connect_timeout: Duration,
    probe_timeout: Duration,
) -> ProbeResult {
    let mut stream = match tokio::time::timeout(
        connect_timeout,
        tokio::net::TcpStream::connect(address),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => return ProbeResult::Other,
        Err(_) => return ProbeResult::Timeout,
    };
    let exchange = async {
        const CNXN: u32 = 0x4e584e43;
        let banner = b"host::\0";
        let words = [
            CNXN,
            0x01000000,
            4096,
            banner.len() as u32,
            banner.iter().map(|b| *b as u32).sum(),
            CNXN ^ u32::MAX,
        ];
        let mut packet: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        packet.extend_from_slice(banner);
        stream.write_all(&packet).await.ok()?;
        let mut header = [0u8; 24];
        stream.read_exact(&mut header).await.ok()?;
        let word = |i| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
        let command = word(0);
        let size = word(12) as usize;
        if word(20) != command ^ u32::MAX || size > 4096 {
            return None;
        }
        let kind = match command {
            0x534c5453 if size == 0 && word(4) == 0x01000000 => AdbKind::Wireless,
            0x48545541 if size == 20 && word(4) == 1 => AdbKind::Legacy,
            CNXN if size > 0 => AdbKind::Legacy,
            _ => return None,
        };
        let mut payload = vec![0; size];
        stream.read_exact(&mut payload).await.ok()?;
        let checksum: u32 = payload.iter().map(|b| *b as u32).sum();
        if word(16) != checksum {
            return None;
        }
        if command == CNXN
            && ![b"device::".as_slice(), b"recovery::", b"bootloader::"]
                .iter()
                .any(|prefix| payload.starts_with(prefix))
        {
            return None;
        }
        Some(kind)
    };
    match tokio::time::timeout(probe_timeout, exchange).await {
        Ok(Some(kind)) => ProbeResult::Adb(kind),
        Ok(None) => ProbeResult::Other,
        Err(_) => ProbeResult::Timeout,
    }
}
