use super::*;
use std::{
    net::TcpListener,
    path::PathBuf,
    process::Command,
    sync::{Mutex, OnceLock},
};

pub(crate) struct Fixture {
    pub dir: tempfile::TempDir,
    pub adb: String,
}
impl Fixture {
    pub fn new(config: &str) -> Self {
        static BINARY: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
        let (_, binary) = BINARY.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let binary = dir.path().join(if cfg!(windows) {
                "fake-adb.exe"
            } else {
                "fake-adb"
            });
            let output = Command::new("rustc")
                .args(["--edition=2024", "-O"])
                .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_adb.rs"))
                .arg("-o")
                .arg(&binary)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            (dir, binary)
        });
        let dir = tempfile::tempdir().unwrap();
        let adb = dir.path().join(binary.file_name().unwrap());
        std::fs::copy(binary, &adb).unwrap();
        std::fs::write(dir.path().join("fixture.txt"), config).unwrap();
        Self {
            dir,
            adb: adb.to_string_lossy().into_owned(),
        }
    }
}

#[test]
fn address_normalization_validation_and_explicit_port() {
    for (input, output) in [
        (" １９２。１６８．０．８：５５５５　", "192.168.0.8:5555"),
        ("192.168.0.8", "192.168.0.8:5555"),
        ("localhost：１２３４５", "localhost:12345"),
        ("［２００１：ｄｂ８：：１］：４２", "[2001:db8::1]:42"),
        ("2001:db8::1", "[2001:db8::1]:5555"),
    ] {
        assert_eq!(parse_endpoint(input, false).unwrap().to_string(), output);
    }
    for input in [
        "",
        "192.168.0.999",
        "192.168.0",
        "192.168.0.8:",
        "host:0",
        "host:65536",
        "host:-1",
        "host:+12",
        "a b:42",
        "https://host:42",
        "host:42:5",
        "0.0.0.0",
        "[::]:42",
        "host：１２。３",
    ] {
        assert!(parse_endpoint(input, false).is_err(), "accepted {input}");
    }
    assert_eq!(
        parse_endpoint("192.168.0.8", true).unwrap_err(),
        "connect.error.port_required"
    );
    assert_eq!(parse_pairing_code(" ０１２３４５ ").unwrap(), "012345");
    for code in ["12345", "1234567", "123 45", "123：45"] {
        assert!(parse_pairing_code(code).is_err());
    }
    assert_eq!(
        parse_scan_ip("１２７。０。０。１").unwrap().to_string(),
        "127.0.0.1"
    );
    for ip in [
        "host",
        "127.0.0.1:42",
        "192.168.0.0/24",
        "255.255.255.255",
        "224.0.0.1",
    ] {
        assert!(parse_scan_ip(ip).is_err());
    }
}

#[test]
fn discovery_keeps_pairing_separate_and_rejects_malformed_services() {
    let services = parse_mdns(
        "List of discovered mdns services\nadb-a _adb-tls-pairing._tcp. 127.0.0.1:30001\nadb-b _adb-tls-connect._tcp 127.0.0.1:30002\nadb-duplicate _adb-tls-connect._tcp 127.0.0.1:30002\nlegacy _adb._tcp [::1]:5555\nweb _http._tcp 127.0.0.1:80\nbad _adb._tcp 127.0.0.1:0\nbad _adb._tcp nonsense",
    );
    assert_eq!(services.len(), 3);
    assert_eq!(
        services
            .iter()
            .filter(|s| s.kind == ServiceKind::Pairing)
            .count(),
        1
    );
    assert_eq!(
        services
            .iter()
            .filter(|s| s.kind == ServiceKind::Connect)
            .count(),
        1
    );
}

#[test]
fn full_scan_covers_all_ports_once_and_prioritizes_history() {
    let ip = "127.0.0.1".parse().unwrap();
    let recent = vec![
        "127.0.0.1:12000".into(),
        "127.0.0.1:5555".into(),
        "127.0.0.2:42".into(),
    ];
    let ports = scan_ports(ip, &recent, true);
    assert_eq!(ports.len(), 65535);
    assert_eq!(&ports[..3], &[12000, 5555, 30000]);
    assert_eq!(ports.iter().collect::<HashSet<_>>().len(), 65535);
    assert!(!ports.contains(&0));
    let quick = scan_ports(ip, &recent, false);
    assert!(!quick.contains(&42));
    assert!(quick.contains(&12000) && quick.contains(&65535));
}

fn listener_reply(
    command: u32,
    arg0: u32,
    payload: &[u8],
    corrupt: bool,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let payload = payload.to_vec();
    let thread = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = [0u8; 31];
        socket.read_exact(&mut request).unwrap();
        assert_eq!(&request[..4], b"CNXN");
        assert_eq!(&request[24..], b"host::\0");
        let checksum = payload.iter().map(|b| *b as u32).sum::<u32>() + u32::from(corrupt);
        let header = [
            command,
            arg0,
            0,
            payload.len() as u32,
            checksum,
            command ^ u32::MAX,
        ];
        for word in header {
            socket.write_all(&word.to_le_bytes()).unwrap();
        }
        socket.write_all(&payload).unwrap();
    });
    (port, thread)
}

#[test]
fn tcp_scan_identifies_adb_handshakes_and_rejects_other_services() {
    let (wireless, a) = listener_reply(0x534c5453, 0x01000000, &[], false);
    let (legacy, b) = listener_reply(0x48545541, 1, &[5; 20], false);
    let (bad, c) = listener_reply(0x48545541, 1, &[5; 20], true);
    let (http, d) = listener_reply(0x50545448, 1, &[], false);
    let updates = Mutex::new(Vec::new());
    scan(
        "127.0.0.1".parse().unwrap(),
        ScanOptions {
            ports: vec![wireless, legacy, bad, http],
            concurrency: 4,
            connect_timeout: Duration::from_secs(2),
            probe_timeout: Duration::from_secs(2),
            deadline: Duration::from_secs(5),
        },
        &CancelToken::default(),
        |event| updates.lock().unwrap().push(event),
    );
    for worker in [a, b, c, d] {
        worker.join().unwrap();
    }
    let updates = updates.into_inner().unwrap();
    let found: Vec<_> = updates
        .iter()
        .filter_map(|e| {
            if let ScanUpdate::Found(c) = e {
                Some((c.address.port(), c.kind))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(found.len(), 2);
    assert!(found.contains(&(wireless, AdbKind::Wireless)));
    assert!(found.contains(&(legacy, AdbKind::Legacy)));
    assert!(matches!(updates.last().unwrap(), ScanUpdate::Progress(p) if p.done && p.tested == 4));
}

#[test]
fn scan_cancellation_releases_pending_sockets_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let cancel = CancelToken::default();
    let worker_cancel = cancel.clone();
    let worker = thread::spawn(move || {
        let start = Instant::now();
        let mut last = None;
        scan(
            address.ip(),
            ScanOptions {
                ports: vec![address.port()],
                concurrency: 1,
                connect_timeout: Duration::from_secs(5),
                probe_timeout: Duration::from_secs(5),
                deadline: Duration::from_secs(10),
            },
            &worker_cancel,
            |e| {
                if let ScanUpdate::Progress(p) = e {
                    last = Some(p);
                }
            },
        );
        (start.elapsed(), last.unwrap())
    });
    let (mut socket, _) = listener.accept().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = [0u8; 31];
    socket.read_exact(&mut request).unwrap();
    cancel.cancel();
    let (elapsed, progress) = worker.join().unwrap();
    assert!(elapsed < Duration::from_secs(2));
    assert!(progress.done && progress.cancelled);
    let mut byte = [0];
    assert!(matches!(socket.read(&mut byte), Ok(0) | Err(_)));
}

#[test]
fn process_e2e_pair_rediscovers_changed_port_and_connects() {
    let fixture = Fixture::new("before_pair=127.0.0.1:39000\nconnect=127.0.0.1:39001");
    let token = CancelToken::default();
    let before = discover(&fixture.adb, &token).unwrap();
    assert!(before.iter().any(|s| s.address.port() == 39000));
    pair(
        &fixture.adb,
        "１２７。０。０。１：３７００１",
        "０１２３４５",
        &token,
    )
    .unwrap();
    let after = discover(&fixture.adb, &token).unwrap();
    let endpoint = after
        .iter()
        .find(|s| s.kind == ServiceKind::Connect)
        .unwrap()
        .address
        .to_string();
    assert_eq!(endpoint, "127.0.0.1:39001");
    crate::adb::connect_device(&fixture.adb, &endpoint).unwrap();
    let devices = crate::adb::list_devices(&fixture.adb).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].serial, endpoint);
    let log = std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap();
    assert!(log.contains("pair stdin_valid=true args_has_code=false"));
    assert!(!log.contains("012345"));
    assert!(!log.contains("connect 127.0.0.1:39000"));
}

#[test]
fn process_deadline_and_pairing_failure_are_bounded() {
    let fixture = Fixture::new("mode=hang");
    let start = Instant::now();
    let error = run_adb(
        &fixture.adb,
        &["mdns", "services"],
        None,
        Duration::from_millis(150),
        &CancelToken::default(),
    )
    .unwrap_err();
    assert_eq!(error.key, "connect.error.timeout");
    assert!(start.elapsed() < Duration::from_secs(3));
    let fixture = Fixture::new("");
    let error = pair(
        &fixture.adb,
        "127.0.0.1:37001",
        "999999",
        &CancelToken::default(),
    )
    .unwrap_err();
    assert_eq!(error.key, "connect.error.pair");
    assert!(error.detail.is_empty());
}
