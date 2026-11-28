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
    let tests: [(&str, TestFn); 8] = [
        ("reuse_one_process_for_many_requests", reuse),
        ("cancel_kills_worker_within_500ms", cancel_kills),
        ("fresh_worker_after_cancel", fresh_after_cancel),
        ("crash_restarts_once", crash_restart),
        ("crash_twice_reports_error", crash_always),
        ("hang_times_out", timeout),
        ("old_one_shot_fallback", one_shot),
        ("worker_error_keeps_process", worker_error),
    ];
    let mut failed = 0;
    for (name, test) in tests {
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
    if mode == "oneshot" {
        let mut raw = Vec::new();
        stdin.read_to_end(&mut raw).unwrap();
        let (w, h) = (u32::from_le_bytes(raw[4..8].try_into().unwrap()), u32::from_le_bytes(raw[8..12].try_into().unwrap()));
        stdout.write_all(b"RGD1").unwrap();
        stdout.write_all(&w.to_le_bytes()).unwrap();
        stdout.write_all(&h.to_le_bytes()).unwrap();
        stdout.write_all(&depth_samples(w, h)).unwrap();
        return;
    }
    while let Some(frame) = read_frame(&mut stdin) {
        match frame[0] {
            0x01 => write_frame(&mut stdout, &[&[0x81][..], b"RGD2", &[1]].concat()),
            0x02 => {
                let (w, h) = (u32::from_le_bytes(frame[1..5].try_into().unwrap()), u32::from_le_bytes(frame[5..9].try_into().unwrap()));
                match mode {
                    "hang" => loop {
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
