//! Depth Map worker client.
//!
//! One persistent worker process per configuration (worker + script + model + timeouts) is
//! started lazily and reused for every request. It speaks the length-prefixed `RGD2` protocol
//! over stdin/stdout. A worker that does not answer the handshake is treated as an old
//! one-shot `RGD1` worker (one process per request) and stays in that mode while the
//! configuration is unchanged.
//!
//! RGD2 frames: `u32 LE length` + payload; the payload starts with a kind byte.
//! - `0x01` hello: `"RGD2"` + version byte (1). Reply `0x81` with the same body.
//! - `0x02` request: `u32 width, u32 height` + RGBA `f32` pixels (LE). Reply `0x82`: `u32 width,
//!   u32 height` + one `f32` depth sample per pixel, or `0xFF` + UTF-8 message (the request
//!   failed, the worker stays usable).
//! - `0x03` shutdown (no reply); closing stdin also ends a worker.
//!
//! Limits: the whole request is copied once into a byte buffer (16 bytes per pixel, so a
//! 20 MP image costs about 320 MB extra) and one frame is below 2 GiB, which limits the image to
//! about 134 MP. A worker that hangs is killed after the timeout (default 120 s) and restarted once.
//!
//! Old one-shot mode (`RGD1`): `"RGD1" u32 w u32 h` + RGBA f32 on stdin until EOF; the reply is
//! `"RGD1" u32 w u32 h` + f32 samples on stdout.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::CancelToken;

const VERSION: u8 = 1;
const MAX_FRAME: usize = 1 << 31;
const POLL: Duration = Duration::from_millis(20);
const KIND_HELLO: u8 = 0x01;
const KIND_REQUEST: u8 = 0x02;
const KIND_SHUTDOWN: u8 = 0x03;
const KIND_HELLO_REPLY: u8 = 0x81;
const KIND_RESULT: u8 = 0x82;
const KIND_ERROR: u8 = 0xFF;

/// How to start and talk to the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepthConfig {
    pub worker: OsString,
    pub args: Vec<OsString>,
    /// Only part of the identity: a changed model restarts the worker (the child inherits the
    /// environment, which is where the worker reads it from).
    pub model: Option<OsString>,
    /// Per request, including the first model load.
    pub timeout: Duration,
    /// How long a new process may take to answer the hello before it counts as an old one-shot worker.
    pub handshake_timeout: Duration,
}

impl DepthConfig {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
    pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    pub fn new(worker: impl Into<OsString>) -> Self {
        Self { worker: worker.into(), args: Vec::new(), model: None, timeout: Self::DEFAULT_TIMEOUT, handshake_timeout: Self::DEFAULT_HANDSHAKE_TIMEOUT }
    }

    /// `RAWGRAPH_DEPTH_WORKER` (+ optional `RAWGRAPH_DEPTH_SCRIPT`, `RAWGRAPH_DEPTH_MODEL`,
    /// `RAWGRAPH_DEPTH_TIMEOUT_SECS`, `RAWGRAPH_DEPTH_HANDSHAKE_SECS`).
    pub fn from_env() -> Result<Self, String> {
        let worker = std::env::var_os("RAWGRAPH_DEPTH_WORKER")
            .ok_or("Depth Map: set RAWGRAPH_DEPTH_WORKER to the configured executable; see docs/depth-estimate.md")?;
        let secs = |name: &str, default: Duration| parse_secs(std::env::var(name).ok().as_deref(), default);
        let mut config = Self::new(worker);
        config.args.extend(std::env::var_os("RAWGRAPH_DEPTH_SCRIPT"));
        config.model = std::env::var_os("RAWGRAPH_DEPTH_MODEL");
        config.timeout = secs("RAWGRAPH_DEPTH_TIMEOUT_SECS", Self::DEFAULT_TIMEOUT);
        config.handshake_timeout = secs("RAWGRAPH_DEPTH_HANDSHAKE_SECS", Self::DEFAULT_HANDSHAKE_TIMEOUT);
        Ok(config)
    }
}

/// Longest wait ever used; larger or invalid settings are clamped, never a panic.
const MAX_WAIT: Duration = Duration::from_secs(24 * 3600);

/// Seconds from an environment variable: positive, finite, at most 24 h; otherwise `default`.
pub fn parse_secs(value: Option<&str>, default: Duration) -> Duration {
    match value.and_then(|v| v.trim().parse::<f64>().ok()).filter(|s| *s > 0.0) {
        Some(s) => Duration::try_from_secs_f64(s).map_or(MAX_WAIT, |d| d.min(MAX_WAIT)),
        None => default,
    }
}

fn deadline_after(wait: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(wait.min(MAX_WAIT)).unwrap_or(now)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepthError {
    Cancelled,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unknown,
    Persistent,
    OneShot,
}

/// How long a one-shot decision holds before the handshake is tried again.
const REPROBE_AFTER: Duration = Duration::from_secs(600);

struct State {
    config: Option<DepthConfig>,
    mode: Mode,
    worker: Option<Proc>,
    /// A valid hello reply was seen for this configuration.
    hello_seen: bool,
    /// A one-shot request worked for this configuration.
    oneshot_ok: bool,
    oneshot_since: Option<Instant>,
}

static POOL: Mutex<State> = Mutex::new(State { config: None, mode: Mode::Unknown, worker: None, hello_seen: false, oneshot_ok: false, oneshot_since: None });
/// Bumped by [`shutdown_depth_workers`]; running requests treat a change like a cancel.
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct Stop<'a> {
    cancel: &'a CancelToken,
    generation: u64,
}

impl Stop<'_> {
    fn fired(&self) -> bool {
        self.cancel.is_cancelled() || GENERATION.load(Ordering::Acquire) != self.generation
    }
}

/// Stops the pooled worker (and its children) and forgets the mode decision. Call on app exit;
/// a request running in another thread is interrupted and reports `Cancelled`.
pub fn shutdown_depth_workers() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
    if let Some(mut state) = lock_pool(None) {
        state.shutdown();
    }
}

/// Locks the pool; with a `stop`, gives up (None) as soon as it fires instead of waiting for
/// another request to finish.
fn lock_pool(stop: Option<&Stop>) -> Option<MutexGuard<'static, State>> {
    loop {
        match POOL.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => {
                if stop.is_some_and(Stop::fired) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

impl State {
    fn shutdown(&mut self) {
        if let Some(mut w) = self.worker.take() {
            w.shutdown_gracefully();
        }
        self.config = None;
        self.mode = Mode::Unknown;
        self.hello_seen = false;
        self.oneshot_ok = false;
        self.oneshot_since = None;
    }
}

/// Estimates depth for an RGBA f32 image. Returns one sample in 0..=1 per pixel.
pub fn estimate(config: &DepthConfig, width: u32, height: u32, rgba: &[f32], cancel: &CancelToken) -> Result<Vec<f32>, DepthError> {
    let pixels = (width as usize).checked_mul(height as usize).ok_or_else(|| fail("image too large"))?;
    if pixels == 0 || rgba.len() != pixels * 4 {
        return Err(fail("invalid image dimensions"));
    }
    let stop = Stop { cancel, generation: GENERATION.load(Ordering::Acquire) };
    if stop.fired() {
        return Err(DepthError::Cancelled);
    }
    let mut body = Vec::with_capacity(8 + rgba.len() * 4);
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    for v in rgba {
        body.extend_from_slice(&v.to_le_bytes());
    }
    if body.len() + 1 >= MAX_FRAME {
        return Err(fail("image too large"));
    }
    let body = Arc::new(body);

    let Some(mut state) = lock_pool(Some(&stop)) else { return Err(DepthError::Cancelled) };
    if stop.fired() {
        return Err(DepthError::Cancelled);
    }
    if state.config.as_ref() != Some(config) {
        state.shutdown();
        state.config = Some(config.clone());
    }
    if state.mode == Mode::OneShot && state.oneshot_since.is_some_and(|t| t.elapsed() > REPROBE_AFTER) {
        state.mode = Mode::Unknown;
    }
    let mut crashes = 0;
    loop {
        if stop.fired() {
            state.shutdown_keep_config();
            return Err(DepthError::Cancelled);
        }
        let outcome = if state.mode == Mode::OneShot {
            one_shot_attempt(config, &body, width, height, &stop)
        } else {
            persistent_attempt(&mut state, config, &body, width, height, &stop)
        };
        match outcome {
            Attempt::Done(v) => {
                if state.mode == Mode::OneShot {
                    state.oneshot_ok = true;
                }
                return Ok(v);
            }
            Attempt::Stopped => {
                state.shutdown_keep_config();
                return Err(DepthError::Cancelled);
            }
            Attempt::Fatal(msg) => {
                state.forget_unproven_one_shot();
                return Err(DepthError::Failed(msg));
            }
            Attempt::Retry(msg) => {
                crashes += 1;
                if crashes >= 2 {
                    state.forget_unproven_one_shot();
                    return Err(DepthError::Failed(msg));
                }
            }
            Attempt::SwitchToOneShot(why) => {
                eprintln!("Depth Map: the worker did not complete the RGD2 handshake ({why}); using one-shot mode (one process per request), checked again in 10 minutes");
                state.mode = Mode::OneShot;
                state.oneshot_since = Some(Instant::now());
                state.oneshot_ok = false;
            }
        }
    }
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 1) as u32).to_le_bytes().to_vec();
    out.push(kind);
    out.extend_from_slice(body);
    out
}

fn fail(msg: &str) -> DepthError {
    DepthError::Failed(format!("Depth Map: {msg}"))
}

enum Attempt {
    Done(Vec<f32>),
    Stopped,
    /// Worker crashed or hung: restart once, then report.
    Retry(String),
    /// Worker answered with an error or garbage: report right away.
    Fatal(String),
    /// The hello was not answered (or the worker exited on it): use one-shot mode.
    SwitchToOneShot(String),
}

impl State {
    /// A one-shot decision that never produced a result may have been a broken worker, not an
    /// old one: look at the handshake again next time.
    fn forget_unproven_one_shot(&mut self) {
        if self.mode == Mode::OneShot && !self.oneshot_ok {
            self.mode = Mode::Unknown;
        }
    }

    /// Kill the worker but keep the configuration and the mode decision.
    fn shutdown_keep_config(&mut self) {
        if let Some(mut w) = self.worker.take() {
            w.kill();
        }
    }
}

fn persistent_attempt(state: &mut State, config: &DepthConfig, body: &Arc<Vec<u8>>, width: u32, height: u32, stop: &Stop) -> Attempt {
    if state.worker.is_none() {
        let mut proc = match Proc::spawn(config) {
            Ok(p) => p,
            Err(e) => return Attempt::Fatal(e),
        };
        proc.send(Arc::new(frame(KIND_HELLO, &[b"RGD2".as_slice(), &[VERSION]].concat())));
        match proc.read_frame(deadline_after(config.handshake_timeout), stop) {
            Ok(reply) if reply.len() == 6 && reply[0] == KIND_HELLO_REPLY && &reply[1..5] == b"RGD2" => {
                if reply[5] != VERSION {
                    proc.kill();
                    return Attempt::Fatal(format!("Depth Map worker speaks unsupported protocol version {}", reply[5]));
                }
                state.mode = Mode::Persistent;
                state.hello_seen = true;
                state.worker = Some(proc);
            }
            Ok(_) => {
                proc.kill();
                return if state.hello_seen { Attempt::Retry("Depth Map: invalid worker response".into()) } else { Attempt::SwitchToOneShot("unexpected reply".into()) };
            }
            Err(Fail::Stopped) => {
                proc.kill();
                return Attempt::Stopped;
            }
            Err(Fail::Timeout) => {
                proc.kill();
                let msg = format!("no answer within {:.1} s", config.handshake_timeout.as_secs_f64());
                return if state.hello_seen { Attempt::Retry(format!("Depth Map worker did not respond ({msg})")) } else { Attempt::SwitchToOneShot(msg) };
            }
            Err(Fail::Closed | Fail::Bad) => {
                let msg = proc.exit_message();
                return if state.hello_seen { Attempt::Retry(msg) } else { Attempt::SwitchToOneShot(msg) };
            }
        }
    }
    let Some(proc) = state.worker.as_mut() else { return Attempt::Retry("Depth Map worker is not running".into()) };
    let mut prefix = ((body.len() + 1) as u32).to_le_bytes().to_vec();
    prefix.push(KIND_REQUEST);
    let prefix = Arc::new(prefix);
    proc.send(prefix);
    proc.send(body.clone());
    let result = proc.read_frame(deadline_after(config.timeout), stop);
    match result {
        Ok(reply) => match reply.first() {
            Some(&KIND_RESULT) => match decode_samples(width, height, &reply[1..]) {
                Ok(v) => Attempt::Done(v),
                Err(e) => {
                    discard(state);
                    Attempt::Fatal(e)
                }
            },
            Some(&KIND_ERROR) => Attempt::Fatal(format!("Depth Map worker failed: {}", String::from_utf8_lossy(&reply[1..]).trim())),
            _ => {
                discard(state);
                Attempt::Fatal("Depth Map: invalid worker response".into())
            }
        },
        Err(Fail::Stopped) => {
            discard(state);
            Attempt::Stopped
        }
        Err(Fail::Timeout) => {
            discard(state);
            Attempt::Retry(format!("Depth Map worker timed out after {:.0} s", config.timeout.as_secs_f64()))
        }
        Err(Fail::Closed | Fail::Bad) => {
            let msg = state.worker.as_mut().map(Proc::exit_message).unwrap_or_default();
            discard(state);
            Attempt::Retry(msg)
        }
    }
}

fn discard(state: &mut State) {
    state.shutdown_keep_config();
}

fn one_shot_attempt(config: &DepthConfig, body: &Arc<Vec<u8>>, width: u32, height: u32, stop: &Stop) -> Attempt {
    let mut proc = match Proc::spawn(config) {
        Ok(p) => p,
        Err(e) => return Attempt::Fatal(e),
    };
    proc.send(Arc::new(b"RGD1".to_vec()));
    proc.send(body.clone());
    proc.close_stdin();
    let out = proc.read_to_end(deadline_after(config.timeout), stop);
    match out {
        Ok(bytes) => {
            if proc.wait_status() != Some(true) {
                return Attempt::Retry(proc.exit_message());
            }
            match decode_reply(width, height, &bytes) {
                Ok(v) => Attempt::Done(v),
                Err(e) => Attempt::Fatal(e),
            }
        }
        Err(Fail::Stopped) => {
            proc.kill();
            Attempt::Stopped
        }
        Err(Fail::Timeout) => {
            proc.kill();
            Attempt::Retry(format!("Depth Map worker timed out after {:.0} s", config.timeout.as_secs_f64()))
        }
        Err(Fail::Closed | Fail::Bad) => Attempt::Retry(proc.exit_message()),
    }
}

/// Validates an old-style `RGD1` reply and returns the samples.
pub fn decode_reply(width: u32, height: u32, bytes: &[u8]) -> Result<Vec<f32>, String> {
    let expected = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)).and_then(|n| n.checked_add(12)).ok_or("Depth Map: image too large")?;
    if bytes.len() != expected || &bytes[..4] != b"RGD1" || bytes[4..8] != width.to_le_bytes() || bytes[8..12] != height.to_le_bytes() {
        return Err("Depth Map: invalid worker response".into());
    }
    check_samples(&bytes[12..])
}

/// Validates the body of an RGD2 result (`u32 w, u32 h` + samples).
fn decode_samples(width: u32, height: u32, body: &[u8]) -> Result<Vec<f32>, String> {
    let expected = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)).and_then(|n| n.checked_add(8)).ok_or("Depth Map: image too large")?;
    if body.len() != expected || body[..4] != width.to_le_bytes() || body[4..8] != height.to_le_bytes() {
        return Err("Depth Map: invalid worker response".into());
    }
    check_samples(&body[8..])
}

fn check_samples(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let data: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    if data.iter().any(|v| !v.is_finite() || !(0.0..=1.0).contains(v)) {
        return Err("Depth Map: invalid depth values".into());
    }
    Ok(data)
}

enum Fail {
    Stopped,
    Timeout,
    Closed,
    Bad,
}

enum Pumped {
    Data,
    Idle,
    Eof,
}

/// A running worker process with writer/reader threads, so waiting can always be interrupted.
struct Proc {
    child: Child,
    stdin_tx: Option<mpsc::Sender<Arc<Vec<u8>>>>,
    out_rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Killed, or exited and reaped: nothing is signalled any more (a reaped process group id
    /// may already belong to somebody else).
    dead: bool,
    exit: Option<std::process::ExitStatus>,
    #[cfg(windows)]
    job: Option<job::Job>,
}

/// Windows: every worker runs in a job object that kills all its processes when terminated or
/// when the last handle closes, which also happens when RawGraph itself dies.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);

    // SAFETY: a job object handle may be used from any thread.
    unsafe impl Send for Job {}

    impl Job {
        pub fn assign(child: &std::process::Child) -> Option<Job> {
            // SAFETY: plain Win32 calls with valid arguments; the handle is closed on every failure path.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if set == 0 || AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) == 0 {
                    CloseHandle(handle);
                    return None;
                }
                Some(Job(handle))
            }
        }

        pub fn terminate(&self) {
            // SAFETY: the handle is valid until `drop`.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing the handle we own; KILL_ON_JOB_CLOSE ends any survivors.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

impl Proc {
    fn spawn(config: &DepthConfig) -> Result<Self, String> {
        let mut command = Command::new(&config.worker);
        command.args(&config.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|e| format!("Depth Map: cannot start worker: {e}"))?;
        #[cfg(windows)]
        let job = job::Job::assign(&child);
        let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            return Err("Depth Map: cannot attach to worker pipes".into());
        };
        let (stdin_tx, stdin_rx) = mpsc::channel::<Arc<Vec<u8>>>();
        std::thread::spawn(move || {
            for chunk in stdin_rx {
                if stdin.write_all(&chunk).is_err() {
                    break;
                }
            }
            let _ = stdin.flush();
        });
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut chunk = vec![0_u8; 1 << 18];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out_tx.send(chunk[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = out_tx.send(Vec::new());
        });
        let tail = Arc::new(Mutex::new(Vec::new()));
        let tail_writer = tail.clone();
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut t) = tail_writer.lock() {
                    t.extend_from_slice(&chunk[..n]);
                    let excess = t.len().saturating_sub(4096);
                    t.drain(..excess);
                }
            }
        });
        Ok(Self {
            child,
            stdin_tx: Some(stdin_tx),
            out_rx,
            buf: Vec::new(),
            stderr: tail,
            dead: false,
            exit: None,
            #[cfg(windows)]
            job,
        })
    }

    fn send(&self, chunk: Arc<Vec<u8>>) {
        if let Some(tx) = &self.stdin_tx {
            let _ = tx.send(chunk);
        }
    }

    fn close_stdin(&mut self) {
        self.stdin_tx = None;
    }

    fn pump(&mut self, deadline: Instant, stop: &Stop) -> Result<Pumped, Fail> {
        if stop.fired() {
            return Err(Fail::Stopped);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Fail::Timeout);
        }
        match self.out_rx.recv_timeout(POLL.min(deadline - now)) {
            Ok(chunk) if chunk.is_empty() => Ok(Pumped::Eof),
            Ok(chunk) => {
                self.buf.extend_from_slice(&chunk);
                Ok(Pumped::Data)
            }
            Err(RecvTimeoutError::Timeout) => Ok(Pumped::Idle),
            Err(RecvTimeoutError::Disconnected) => Ok(Pumped::Eof),
        }
    }

    fn read_frame(&mut self, deadline: Instant, stop: &Stop) -> Result<Vec<u8>, Fail> {
        loop {
            if self.buf.len() >= 4 {
                let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
                if len == 0 || len >= MAX_FRAME {
                    return Err(Fail::Bad);
                }
                if self.buf.len() >= 4 + len {
                    let payload = self.buf[4..4 + len].to_vec();
                    self.buf.drain(..4 + len);
                    return Ok(payload);
                }
            }
            if let Pumped::Eof = self.pump(deadline, stop)? {
                return Err(Fail::Closed);
            }
        }
    }

    fn read_to_end(&mut self, deadline: Instant, stop: &Stop) -> Result<Vec<u8>, Fail> {
        loop {
            if let Pumped::Eof = self.pump(deadline, stop)? {
                return Ok(std::mem::take(&mut self.buf));
            }
        }
    }

    /// Waits briefly for the process to exit on its own. `Some(success)` once it has (it is then
    /// reaped and nothing is signalled to its id any more), `None` if it did not.
    fn wait_status(&mut self) -> Option<bool> {
        if self.dead {
            return self.exit.map(|s| s.success());
        }
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.exit = Some(status);
                    self.dead = true;
                    return Some(status.success());
                }
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
                _ => return None,
            }
        }
    }

    /// Kills the worker (so survivors of a crashed one cannot linger) and describes why it ended.
    fn exit_message(&mut self) -> String {
        self.kill();
        // Let the stderr thread catch up with the last lines.
        std::thread::sleep(Duration::from_millis(20));
        let text = self.stderr.lock().map(|t| String::from_utf8_lossy(&t).trim().to_string()).unwrap_or_default();
        let status = self.exit.map_or_else(|| "unknown status".to_owned(), |s| s.to_string());
        format!("Depth Map worker exited unexpectedly ({status}){}", if text.is_empty() { String::new() } else { format!(": {text}") })
    }

    fn shutdown_gracefully(&mut self) {
        if !self.dead {
            self.send(Arc::new(frame(KIND_SHUTDOWN, &[])));
            self.close_stdin();
            let until = Instant::now() + Duration::from_millis(300);
            while Instant::now() < until {
                if let Ok(Some(status)) = self.child.try_wait() {
                    self.exit = Some(status);
                    self.dead = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if self.dead {
            // It left on its own: whatever it started goes too (Windows job object).
            #[cfg(windows)]
            if let Some(job) = self.job.take() {
                job.terminate();
            }
        } else {
            self.kill();
        }
    }

    /// Kills the process and everything it started, then reaps it. The process group is
    /// signalled before the leader is reaped, so its id cannot have been reused.
    fn kill(&mut self) {
        if self.dead {
            return;
        }
        self.dead = true;
        self.stdin_tx = None;
        #[cfg(windows)]
        match self.job.take() {
            Some(job) => job.terminate(),
            None => kill_tree_fallback(self.child.id()),
        }
        #[cfg(unix)]
        kill_group(self.child.id());
        let _ = self.child.kill();
        self.exit = self.child.wait().ok();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Only used when the job object could not be created.
#[cfg(windows)]
fn kill_tree_fallback(pid: u32) {
    use std::os::windows::process::CommandExt;
    let root = std::env::var_os("SystemRoot").map_or_else(|| std::path::PathBuf::from(r"C:\Windows"), std::path::PathBuf::from);
    let _ = Command::new(root.join("System32").join("taskkill.exe"))
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .status();
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    // The worker leads its own process group (see `spawn`); signal all of it.
    let Some(kill) = ["/bin/kill", "/usr/bin/kill"].into_iter().map(std::path::Path::new).find(|p| p.exists()) else { return };
    let _ = Command::new(kill).args(["-s", "KILL", "--", &format!("-{pid}")]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_validated() {
        let mut body = Vec::new();
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&0.5_f32.to_le_bytes());
        assert_eq!(decode_samples(1, 1, &body).unwrap(), vec![0.5]);
        assert!(decode_samples(2, 1, &body).is_err());
        let n = body.len();
        body[n - 4..].copy_from_slice(&2.0_f32.to_le_bytes());
        assert!(decode_samples(1, 1, &body).is_err());
    }

    #[test]
    fn timeouts_from_the_environment_never_panic() {
        let d = Duration::from_secs(120);
        assert_eq!(parse_secs(None, d), d);
        for bad in ["abc", "-5", "0", "NaN", "", "-inf"] {
            assert_eq!(parse_secs(Some(bad), d), d, "{bad}");
        }
        assert_eq!(parse_secs(Some(" 2.5 "), d), Duration::from_millis(2500));
        for huge in ["inf", "1e30", "1e300", "100000", "18446744073709551616"] {
            assert_eq!(parse_secs(Some(huge), d), MAX_WAIT, "{huge}");
        }
        // A huge configured wait cannot overflow the deadline either.
        let _ = deadline_after(Duration::MAX);
    }

    #[test]
    fn config_requires_worker_variable_message() {
        if std::env::var_os("RAWGRAPH_DEPTH_WORKER").is_none() {
            assert!(DepthConfig::from_env().unwrap_err().contains("RAWGRAPH_DEPTH_WORKER"));
        }
    }
}
