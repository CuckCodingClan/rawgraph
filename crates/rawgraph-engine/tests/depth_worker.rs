//! Depth worker pool tests with a fake worker. This executable (no libtest harness) is its own
//! worker: started with `--fake <mode> <log> <marker>` it speaks the RGD2 / RGD1 protocol.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use rawgraph_engine::depth::{estimate, shutdown_depth_workers, DepthConfig, DepthError};
use rawgraph_engine::CancelToken;

type TestFn = fn(&std::path::Path);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--fake") {
        fake(&args[2], &args[3], &args[4]);
        return;
    }
    let dir = std::env::temp_dir().join(format!("rg-depth-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let tests: [(&str, TestFn); 20] = [
        ("reuse_one_process_for_many_requests", reuse),
        ("cancel_kills_worker_within_500ms", cancel_kills),
        ("fresh_worker_after_cancel", fresh_after_cancel),
        ("crash_restarts_once", crash_restart),
        ("crash_twice_reports_error", crash_always),
        ("hang_times_out", timeout),
        ("old_one_shot_fallback", one_shot),
        ("worker_error_keeps_process", worker_error),
        ("strict_old_worker_that_exits_on_hello", strict_old),
        ("protocol_version_mismatch", bad_version),
        ("shutdown_interrupts_running_request", shutdown_interrupts),
        ("config_change_stops_old_worker", config_change),
        ("cancel_while_waiting_for_the_pool_lock", cancel_in_queue),
        ("cancel_during_handshake", cancel_handshake),
        ("heavy_stderr_output", noisy),
        ("split_frames", split_frames),
        ("garbage_frame", garbage),
        ("one_shot_cancel", one_shot_cancel),
        ("one_shot_timeout", one_shot_timeout),
        ("grandchild_is_killed_with_the_worker", grandchild),
    ];
    // Optional name filter, like libtest: the first argument that is not a flag.
    let filter = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned();
    let mut failed = 0;
    for (name, test) in tests {
        if filter.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
            continue;
        }
        shutdown_depth_workers();
        let sub = dir.join(name);
        std::fs::create_dir_all(&sub).expect("test dir");
        let result = std::panic::catch_unwind(|| test(&sub));
        shutdown_depth_workers();
        match result {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if failed > 0 {
        eprintln!("{failed} depth worker test(s) failed");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------- fake worker

fn read_frame(input: &mut impl Read) -> Option<Vec<u8>> {
    let mut len = [0_u8; 4];
    input.read_exact(&mut len).ok()?;
    let mut payload = vec![0_u8; u32::from_le_bytes(len) as usize];
    input.read_exact(&mut payload).ok()?;
    Some(payload)
}

fn write_frame(out: &mut impl Write, payload: &[u8]) {
    out.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    out.write_all(payload).unwrap();
    out.flush().unwrap();
}

fn depth_samples(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..h {
        for x in 0..w {
            out.extend_from_slice(&(x as f32 / w as f32).to_le_bytes());
        }
    }
    out
}

fn fake(mode: &str, log: &str, marker: &str) {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
    writeln!(f, "{}", std::process::id()).unwrap();
    drop(f);
    let first_time = !std::path::Path::new(marker).exists();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    if mode == "sleeper" {
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    if mode == "grandchild" {
        // A child that inherits our pipes and outlives us unless somebody kills the whole tree.
        let exe = std::env::current_exe().unwrap();
        let child_log = format!("{log}.child");
        #[allow(clippy::zombie_processes)]
        let _ = std::process::Command::new(exe).args(["--fake", "sleeper", &child_log, marker]).spawn().unwrap();
    }
    if matches!(mode, "oneshot" | "oneshot_hang" | "strict") {
        let mut raw = Vec::new();
        if mode == "strict" {
            // An old worker that rejects anything but RGD1 and exits.
            let mut head = [0_u8; 4];
            stdin.read_exact(&mut head).unwrap();
            if &head != b"RGD1" {
                eprintln!("invalid RGD1 request");
                std::process::exit(2);
            }
            raw.extend_from_slice(&head);
        }
        stdin.read_to_end(&mut raw).unwrap();
        if mode == "oneshot_hang" {
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        let (w, h) = (u32::from_le_bytes(raw[4..8].try_into().unwrap()), u32::from_le_bytes(raw[8..12].try_into().unwrap()));
        stdout.write_all(b"RGD1").unwrap();
        stdout.write_all(&w.to_le_bytes()).unwrap();
        stdout.write_all(&h.to_le_bytes()).unwrap();
        stdout.write_all(&depth_samples(w, h)).unwrap();
        return;
    }
    while let Some(frame) = read_frame(&mut stdin) {
        match frame[0] {
            0x01 => write_frame(&mut stdout, &[&[0x81][..], b"RGD2", &[if mode == "badversion" { 9 } else { 1 }]].concat()),
            0x02 => {
                let (w, h) = (u32::from_le_bytes(frame[1..5].try_into().unwrap()), u32::from_le_bytes(frame[5..9].try_into().unwrap()));
                match mode {
                    "hang" | "grandchild" => loop {
                        std::thread::sleep(Duration::from_secs(60));
                    },
                    "hang_first" if first_time => {
                        std::fs::write(marker, b"x").unwrap();
                        loop {
                            std::thread::sleep(Duration::from_secs(60));
                        }
                    }
                    "crash_first" if first_time => {
                        std::fs::write(marker, b"x").unwrap();
                        eprintln!("fake crash");
                        std::process::exit(3);
                    }
                    "crash_always" => {
                        eprintln!("fake crash");
                        std::process::exit(3);
                    }
                    "garbage" => write_frame(&mut stdout, &[0x55, 1, 2, 3]),
                    "noisy" => {
                        let mut err = std::io::stderr().lock();
                        for _ in 0..16 {
                            err.write_all(&[b'x'; 65536]).unwrap();
                        }
                        drop(err);
                        write_frame(&mut stdout, &[&[0x82][..], &w.to_le_bytes(), &h.to_le_bytes(), &depth_samples(w, h)].concat());
                    }
                    "split" => {
                        let payload = [&[0x82][..], &w.to_le_bytes(), &h.to_le_bytes(), &depth_samples(w, h)].concat();
                        let all = [&(payload.len() as u32).to_le_bytes()[..], &payload].concat();
                        for (i, b) in all.iter().enumerate() {
                            stdout.write_all(&[*b]).unwrap();
                            if i < 40 {
                                stdout.flush().unwrap();
                                std::thread::sleep(Duration::from_millis(2));
                            }
                        }
                        stdout.flush().unwrap();
                    }
                    "error" => write_frame(&mut stdout, &[&[0xFF][..], b"bad image"].concat()),
                    _ => write_frame(&mut stdout, &[&[0x82][..], &w.to_le_bytes(), &h.to_le_bytes(), &depth_samples(w, h)].concat()),
                }
            }
            _ => return,
        }
    }
}

// ---------------------------------------------------------------- helpers

fn config(dir: &std::path::Path, mode: &str) -> DepthConfig {
    let mut c = DepthConfig::new(std::env::current_exe().unwrap());
    let log = dir.join("log.txt");
    let marker = dir.join("marker");
    c.args = vec!["--fake".into(), mode.into(), log.into(), marker.into()];
    c.timeout = Duration::from_secs(20);
    c.handshake_timeout = Duration::from_secs(5);
    c
}

fn pids(dir: &std::path::Path) -> Vec<u32> {
    std::fs::read_to_string(dir.join("log.txt")).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect()
}

fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
    }
}

fn image() -> (u32, u32, Vec<f32>) {
    (4, 2, vec![0.5; 4 * 2 * 4])
}

fn check_ok(result: Result<Vec<f32>, DepthError>) {
    let data = result.expect("depth result");
    assert_eq!(data.len(), 8);
    assert_eq!(&data[..4], &[0.0, 0.25, 0.5, 0.75]);
}

// ---------------------------------------------------------------- tests

fn reuse(dir: &std::path::Path) {
    let c = config(dir, "ok");
    let (w, h, px) = image();
    for _ in 0..5 {
        check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    }
    assert_eq!(pids(dir).len(), 1, "one process for five requests");
}

fn cancel_kills(dir: &std::path::Path) {
    let c = config(dir, "hang");
    let (w, h, px) = image();
    let cancel = CancelToken::new();
    let remote = cancel.clone();
    let fired = std::sync::Arc::new(std::sync::Mutex::new(None));
    let fired_writer = fired.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        *fired_writer.lock().unwrap() = Some(Instant::now());
        remote.cancel();
    });
    let result = estimate(&c, w, h, &px, &cancel);
    let returned = Instant::now();
    t.join().unwrap();
    assert_eq!(result, Err(DepthError::Cancelled));
    let since_cancel = returned.duration_since(fired.lock().unwrap().expect("cancel fired"));
    assert!(since_cancel < Duration::from_millis(500), "cancel took {since_cancel:?}");
    let pid = *pids(dir).last().expect("worker started");
    assert!(!alive(pid), "worker {pid} still running");
}

fn fresh_after_cancel(dir: &std::path::Path) {
    let c = config(dir, "hang_first");
    let (w, h, px) = image();
    let cancel = CancelToken::new();
    let remote = cancel.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        remote.cancel();
    });
    assert_eq!(estimate(&c, w, h, &px, &cancel), Err(DepthError::Cancelled));
    t.join().unwrap();
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    let pids = pids(dir);
    assert_eq!(pids.len(), 2, "a fresh worker after the cancel");
    assert!(!alive(pids[0]));
}

fn crash_restart(dir: &std::path::Path) {
    let c = config(dir, "crash_first");
    let (w, h, px) = image();
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    assert_eq!(pids(dir).len(), 2, "restarted once");
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    assert_eq!(pids(dir).len(), 2);
}

fn crash_always(dir: &std::path::Path) {
    let c = config(dir, "crash_always");
    let (w, h, px) = image();
    let Err(DepthError::Failed(msg)) = estimate(&c, w, h, &px, &CancelToken::new()) else { panic!("expected failure") };
    assert!(msg.contains("fake crash"), "{msg}");
    assert_eq!(pids(dir).len(), 2, "exactly one restart");
}

fn timeout(dir: &std::path::Path) {
    let mut c = config(dir, "hang");
    c.timeout = Duration::from_millis(400);
    let (w, h, px) = image();
    let start = Instant::now();
    let Err(DepthError::Failed(msg)) = estimate(&c, w, h, &px, &CancelToken::new()) else { panic!("expected failure") };
    assert!(msg.contains("timed out"), "{msg}");
    assert!(start.elapsed() < Duration::from_secs(10));
    let all = pids(dir);
    assert_eq!(all.len(), 2, "restarted once after the timeout");
    assert!(all.iter().all(|p| !alive(*p)));
}

fn one_shot(dir: &std::path::Path) {
    let mut c = config(dir, "oneshot");
    c.handshake_timeout = Duration::from_millis(500);
    let (w, h, px) = image();
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    assert_eq!(pids(dir).len(), 2, "handshake probe + one-shot process");
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    assert_eq!(pids(dir).len(), 3, "no further handshake, one process per request");
}

fn worker_error(dir: &std::path::Path) {
    let c = config(dir, "error");
    let (w, h, px) = image();
    for _ in 0..2 {
        let Err(DepthError::Failed(msg)) = estimate(&c, w, h, &px, &CancelToken::new()) else { panic!("expected failure") };
        assert!(msg.contains("bad image"), "{msg}");
    }
    assert_eq!(pids(dir).len(), 1, "an error reply is not a crash");
}

fn cancel_after(token: &CancelToken, ms: u64) -> std::thread::JoinHandle<()> {
    let token = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        token.cancel();
    })
}

fn strict_old(dir: &std::path::Path) {
    let mut c = config(dir, "strict");
    c.handshake_timeout = Duration::from_secs(5);
    let (w, h, px) = image();
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    assert_eq!(pids(dir).len(), 2, "hello probe that exited + one-shot process");
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    assert_eq!(pids(dir).len(), 3);
}

fn bad_version(dir: &std::path::Path) {
    let c = config(dir, "badversion");
    let (w, h, px) = image();
    let Err(DepthError::Failed(msg)) = estimate(&c, w, h, &px, &CancelToken::new()) else { panic!("expected failure") };
    assert!(msg.contains("unsupported protocol version 9"), "{msg}");
    assert!(!alive(pids(dir)[0]));
}

fn shutdown_interrupts(dir: &std::path::Path) {
    let c = config(dir, "hang");
    let (w, h, px) = image();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        shutdown_depth_workers();
    });
    let start = Instant::now();
    assert_eq!(estimate(&c, w, h, &px, &CancelToken::new()), Err(DepthError::Cancelled));
    t.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!alive(pids(dir)[0]));
}

fn config_change(dir: &std::path::Path) {
    let (w, h, px) = image();
    check_ok(estimate(&config(dir, "ok"), w, h, &px, &CancelToken::new()));
    let first = pids(dir)[0];
    assert!(alive(first));
    check_ok(estimate(&config(dir, "ok2"), w, h, &px, &CancelToken::new()));
    assert_eq!(pids(dir).len(), 2);
    assert!(!alive(first), "the old worker stops when the configuration changes");
}

fn cancel_in_queue(dir: &std::path::Path) {
    let c = config(dir, "hang");
    let (w, h, px) = image();
    let first = CancelToken::new();
    let (c1, px1, first1) = (c.clone(), px.clone(), first.clone());
    let holder = std::thread::spawn(move || estimate(&c1, w, h, &px1, &first1));
    std::thread::sleep(Duration::from_millis(500));
    let second = CancelToken::new();
    let t = cancel_after(&second, 200);
    let start = Instant::now();
    assert_eq!(estimate(&c, w, h, &px, &second), Err(DepthError::Cancelled));
    assert!(start.elapsed() < Duration::from_millis(700), "waited {:?}", start.elapsed());
    t.join().unwrap();
    assert_eq!(pids(dir).len(), 1, "the queued request never started a worker");
    first.cancel();
    assert_eq!(holder.join().unwrap(), Err(DepthError::Cancelled));
}

fn cancel_handshake(dir: &std::path::Path) {
    let mut c = config(dir, "oneshot_hang");
    c.handshake_timeout = Duration::from_secs(30);
    let (w, h, px) = image();
    let cancel = CancelToken::new();
    let t = cancel_after(&cancel, 400);
    let start = Instant::now();
    assert_eq!(estimate(&c, w, h, &px, &cancel), Err(DepthError::Cancelled));
    assert!(start.elapsed() < Duration::from_secs(2));
    t.join().unwrap();
    assert!(!alive(pids(dir)[0]));
}

fn noisy(dir: &std::path::Path) {
    let c = config(dir, "noisy");
    let (w, h, px) = image();
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
    assert_eq!(pids(dir).len(), 1);
}

fn split_frames(dir: &std::path::Path) {
    let c = config(dir, "split");
    let (w, h, px) = image();
    check_ok(estimate(&c, w, h, &px, &CancelToken::new()));
}

fn garbage(dir: &std::path::Path) {
    let c = config(dir, "garbage");
    let (w, h, px) = image();
    let Err(DepthError::Failed(msg)) = estimate(&c, w, h, &px, &CancelToken::new()) else { panic!("expected failure") };
    assert!(msg.contains("invalid worker response"), "{msg}");
}

fn one_shot_cancel(dir: &std::path::Path) {
    let mut c = config(dir, "oneshot_hang");
    c.handshake_timeout = Duration::from_millis(300);
    let (w, h, px) = image();
    let cancel = CancelToken::new();
    let t = cancel_after(&cancel, 1500);
    assert_eq!(estimate(&c, w, h, &px, &cancel), Err(DepthError::Cancelled));
    t.join().unwrap();
    let all = pids(dir);
    assert_eq!(all.len(), 2, "handshake probe + the one-shot process");
    assert!(all.iter().all(|p| !alive(*p)));
}

fn one_shot_timeout(dir: &std::path::Path) {
    let mut c = config(dir, "oneshot_hang");
    c.handshake_timeout = Duration::from_millis(300);
    c.timeout = Duration::from_millis(400);
    let (w, h, px) = image();
    let Err(DepthError::Failed(msg)) = estimate(&c, w, h, &px, &CancelToken::new()) else { panic!("expected failure") };
    assert!(msg.contains("timed out"), "{msg}");
    assert!(pids(dir).iter().all(|p| !alive(*p)));
}

fn grandchild(dir: &std::path::Path) {
    let c = config(dir, "grandchild");
    let (w, h, px) = image();
    let cancel = CancelToken::new();
    let t = cancel_after(&cancel, 1000);
    assert_eq!(estimate(&c, w, h, &px, &cancel), Err(DepthError::Cancelled));
    t.join().unwrap();
    let child: u32 = std::fs::read_to_string(dir.join("log.txt.child")).expect("grandchild started").trim().parse().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while alive(child) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(child), "grandchild {child} survived");
}
