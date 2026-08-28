use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::protocol;

#[derive(Debug)]
pub enum WorkerError {
    Io(String),
    Protocol(String),
    Unavailable(String),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkerError::Io(e) | WorkerError::Protocol(e) | WorkerError::Unavailable(e) => {
                f.write_str(e)
            }
        }
    }
}

pub struct WorkerResponse {
    pub http_status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub restart_worker: bool,
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

pub struct Worker {
    launcher: String,
    version: String,
    request_timeout: Duration,
    startup_timeout: Duration,
    busy_timeout: Duration,
    max_waiters: u32,
    max_startup_failures: u32,
    exit_on_startup_failure: bool,
    next_id: AtomicU32,
    restart_count: AtomicU32,
    timeout_count: AtomicU32,
    waiting_count: AtomicU32,
    startup_failures: AtomicU32,
    pid: AtomicU32,
    proc: Mutex<Option<WorkerProcess>>,
    state: Mutex<WorkerState>,
}

struct WorkerState {
    current: Option<CurrentRequest>,
    last_error: Option<String>,
    last_restart_reason: Option<String>,
}

#[derive(Clone)]
struct CurrentRequest {
    id: u32,
    opcode: u16,
    started: Instant,
}

struct RequestTracker<'a> {
    worker: &'a Worker,
    id: u32,
}

struct WaitTracker<'a> {
    worker: &'a Worker,
}

impl Worker {
    pub fn new(launcher: &str, version: String) -> Self {
        Self {
            launcher: launcher.to_string(),
            version,
            request_timeout: worker_timeout(),
            startup_timeout: worker_startup_timeout(),
            busy_timeout: worker_busy_timeout(),
            max_waiters: worker_max_waiters(),
            max_startup_failures: worker_max_startup_failures(),
            exit_on_startup_failure: worker_exit_on_startup_failure(),
            next_id: AtomicU32::new(1),
            restart_count: AtomicU32::new(0),
            timeout_count: AtomicU32::new(0),
            waiting_count: AtomicU32::new(0),
            startup_failures: AtomicU32::new(0),
            pid: AtomicU32::new(0),
            proc: Mutex::new(None),
            state: Mutex::new(WorkerState {
                current: None,
                last_error: None,
                last_restart_reason: None,
            }),
        }
    }

    pub fn ensure_started(&self) -> Result<(), WorkerError> {
        let mut guard = self
            .proc
            .lock()
            .map_err(|_| WorkerError::Unavailable("worker mutex poisoned".to_string()))?;
        if let Some(p) = guard.as_mut() {
            if p.child.try_wait().map_err(io_err)?.is_none() {
                return Ok(());
            }
            self.pid.store(0, Ordering::Relaxed);
        }
        *guard = Some(self.spawn_ready()?);
        Ok(())
    }

    pub fn health(&self) -> Result<WorkerResponse, WorkerError> {
        self.request_json(protocol::OP_HEALTH, Value::Null)
    }

    pub fn snapshot(&self) -> Value {
        let current = self.state.lock().ok().and_then(|s| s.current.clone());
        let (last_error, last_restart_reason) = self
            .state
            .lock()
            .map(|s| (s.last_error.clone(), s.last_restart_reason.clone()))
            .unwrap_or((None, None));
        let pid = match self.pid.load(Ordering::Relaxed) {
            0 => None,
            pid => Some(pid),
        };
        json!({
            "pid": pid,
            "request_timeout_secs": self.request_timeout.as_secs(),
            "startup_timeout_secs": self.startup_timeout.as_secs(),
            "busy_timeout_ms": self.busy_timeout.as_millis(),
            "max_waiters": self.max_waiters,
            "max_restarts": self.max_startup_failures,
            "restart_count": self.restart_count.load(Ordering::Relaxed),
            "timeout_count": self.timeout_count.load(Ordering::Relaxed),
            "waiting_count": self.waiting_count.load(Ordering::Relaxed),
            "startup_failures": self.startup_failures.load(Ordering::Relaxed),
            "current_request": current.map(|r| json!({
                "id": r.id,
                "opcode": r.opcode,
                "elapsed_ms": r.started.elapsed().as_millis(),
            })),
            "last_error": last_error,
            "last_restart_reason": last_restart_reason,
        })
    }

    pub fn request_json(&self, opcode: u16, payload: Value) -> Result<WorkerResponse, WorkerError> {
        let bytes = if payload.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&payload).map_err(|e| WorkerError::Protocol(e.to_string()))?
        };
        let frame = self.request(opcode, bytes)?;
        parse_worker_response(frame)
    }

    pub fn decrypt_batch(
        &self,
        adam: &str,
        uri: &str,
        samples: Vec<Vec<u8>>,
    ) -> Result<Vec<Vec<u8>>, WorkerError> {
        let payload = protocol::decrypt_batch_payload(adam, uri, &samples)
            .map_err(|e| WorkerError::Protocol(e.to_string()))?;
        let frame = self.request(protocol::OP_DECRYPT_BATCH, payload)?;
        if frame.flags & 1 == 1 {
            return protocol::parse_decrypt_samples_payload(&frame.payload)
                .map_err(|e| WorkerError::Protocol(e.to_string()));
        }
        let r = parse_worker_response(frame)?;
        if r.restart_worker {
            self.restart_after_delay();
        }
        Err(WorkerError::Unavailable(
            String::from_utf8_lossy(&r.body).to_string(),
        ))
    }

    fn request(&self, opcode: u16, payload: Vec<u8>) -> Result<protocol::Frame, WorkerError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request_deadline = Instant::now() + self.request_timeout;

        // Admission control: too many threads already queued behind the single
        // worker means the worker is likely stuck. Fail fast instead of
        // letting the queue grow past the platform's connection timeout.
        if self.waiting_count.load(Ordering::Relaxed) >= self.max_waiters {
            return Err(WorkerError::Unavailable(format!(
                "worker busy: {} requests already waiting",
                self.waiting_count.load(Ordering::Relaxed)
            )));
        }

        let wait_tracker = self.track_wait();
        // Do not let lock contention eat the whole request budget: waiting
        // behind a stuck request past the busy timeout just wastes the
        // client's connection (Heroku kills at 30s regardless).
        let lock_deadline = Instant::now() + self.busy_timeout;
        let mut guard = match lock_worker_timeout(&self.proc, lock_deadline) {
            Ok(g) => g,
            Err(e) => {
                if e.to_string().contains("timed out") {
                    eprintln!(
                        "wrapper: worker request opcode={opcode} timed out waiting for worker after {:?}",
                        self.busy_timeout
                    );
                    self.timeout_count.fetch_add(1, Ordering::Relaxed);
                    self.recover_stuck_worker_or_exit("lock wait timeout");
                }
                self.record_error(e.to_string());
                return Err(e);
            }
        };
        drop(wait_tracker);
        let _tracker = self.track_request(id, opcode);
        if guard.is_none() {
            *guard = Some(self.spawn_ready()?);
        }
        let proc = guard
            .as_mut()
            .ok_or_else(|| WorkerError::Unavailable("worker missing".to_string()))?;
        if proc.child.try_wait().map_err(io_err)?.is_some() {
            *guard = Some(self.spawn_ready()?);
        }
        let proc = guard
            .as_mut()
            .ok_or_else(|| WorkerError::Unavailable("worker missing".to_string()))?;
        let req = protocol::Frame {
            kind: protocol::KIND_REQUEST,
            request_id: id,
            opcode,
            flags: 0,
            payload,
        };
        if let Err(e) = write_frame_timeout(&mut proc.stdin, &req, request_deadline) {
            if e.kind() == io::ErrorKind::TimedOut {
                eprintln!(
                    "wrapper: worker request opcode={opcode} timed out while writing after {:?}; restarting worker",
                    self.request_timeout
                );
                self.timeout_count.fetch_add(1, Ordering::Relaxed);
                self.abandon_locked_worker(&mut guard, "write timeout");
            } else {
                self.abandon_locked_worker(&mut guard, "ipc write error");
            }
            self.record_error(e.to_string());
            return Err(WorkerError::Io(e.to_string()));
        }
        let resp = match read_frame_timeout(&mut proc.stdout, request_deadline) {
            Ok(frame) => frame,
            Err(e) => {
                if e.kind() == io::ErrorKind::TimedOut {
                    eprintln!(
                        "wrapper: worker request opcode={opcode} timed out after {:?}; restarting worker",
                        self.request_timeout
                    );
                    self.timeout_count.fetch_add(1, Ordering::Relaxed);
                    self.abandon_locked_worker(&mut guard, "response timeout");
                } else {
                    self.abandon_locked_worker(&mut guard, "ipc read error");
                }
                self.record_error(e.to_string());
                return Err(WorkerError::Io(e.to_string()));
            }
        };
        if resp.kind != protocol::KIND_RESPONSE || resp.request_id != id || resp.opcode != opcode {
            self.abandon_locked_worker(&mut guard, "mismatched ipc response");
            self.record_error("mismatched ipc response");
            return Err(WorkerError::Protocol("mismatched ipc response".to_string()));
        }
        self.startup_failures.store(0, Ordering::Relaxed);
        Ok(resp)
    }

    fn spawn(&self) -> Result<WorkerProcess, WorkerError> {
        eprintln!("wrapper: starting ipc worker {}", self.launcher);
        setup_bionic_env();
        ensure_apple_state_dirs();
        let mut child = Command::new(&self.launcher)
            .env("WRAPPER_MODE", "ipc-worker")
            .env("ANDROID_DNS_MODE", "local")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(io_err)?;
        let stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WorkerError::Io("worker stdin unavailable".to_string()));
            }
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
                return Err(WorkerError::Io("worker stdout unavailable".to_string()));
            }
        };
        if let Err(e) = set_nonblocking(&stdin).and_then(|_| set_nonblocking(&stdout)) {
            drop(stdin);
            drop(stdout);
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        self.pid.store(child.id(), Ordering::Relaxed);
        let _ = &self.version;
        Ok(WorkerProcess {
            child,
            stdin,
            stdout,
        })
    }

    // Spawn a worker and prove it is actually responsive before handing it
    // to a caller. A worker that hangs during Apple-lib init (startup lease,
    // session restore) never enters its IPC read loop; without this check a
    // wedged worker would sit in the proc slot eating a full request timeout
    // on every request. On probe failure the worker is killed and respawned
    // up to max_startup_failures times, then the supervisor exits so the PaaS
    // platform restarts the dyno cleanly.
    fn spawn_ready(&self) -> Result<WorkerProcess, WorkerError> {
        let attempts = self.max_startup_failures.max(1);
        for attempt in 1..=attempts {
            let mut proc = match self.spawn() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "wrapper: worker spawn failed (attempt {attempt}/{attempts}): {e}"
                    );
                    self.note_startup_failure();
                    if attempt < attempts {
                        thread::sleep(Duration::from_secs(1));
                    }
                    continue;
                }
            };
            match self.probe_ready(&mut proc) {
                Ok(()) => {
                    eprintln!("wrapper: ipc worker ready (pid={})", proc.child.id());
                    self.startup_failures.store(0, Ordering::Relaxed);
                    return Ok(proc);
                }
                Err(e) => {
                    eprintln!(
                        "wrapper: worker startup probe failed (attempt {attempt}/{attempts}): {e}"
                    );
                    self.pid.store(0, Ordering::Relaxed);
                    cleanup_worker(Some(proc), "startup probe failed");
                    self.note_startup_failure();
                    if attempt < attempts {
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        }
        Err(WorkerError::Unavailable(format!(
            "worker failed to become ready after {attempts} attempts"
        )))
    }

    fn note_startup_failure(&self) {
        let failures = self.startup_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if self.exit_on_startup_failure && failures >= self.max_startup_failures.max(1) {
            eprintln!(
                "wrapper: fatal: worker failed to start {failures} consecutive times; exiting for container restart"
            );
            std::process::exit(70);
        }
    }

    // Send an OP_HEALTH frame and require a well-formed response within
    // startup_timeout. Any other outcome means the worker is not usable.
    fn probe_ready(&self, proc: &mut WorkerProcess) -> Result<(), WorkerError> {
        let deadline = Instant::now() + self.startup_timeout;
        let req = protocol::Frame {
            kind: protocol::KIND_REQUEST,
            request_id: 0,
            opcode: protocol::OP_HEALTH,
            flags: 0,
            payload: Vec::new(),
        };
        write_frame_timeout(&mut proc.stdin, &req, deadline)
            .map_err(|e| WorkerError::Io(e.to_string()))?;
        let resp = read_frame_timeout(&mut proc.stdout, deadline)
            .map_err(|e| WorkerError::Io(e.to_string()))?;
        if resp.kind != protocol::KIND_RESPONSE || resp.opcode != protocol::OP_HEALTH {
            return Err(WorkerError::Protocol(
                "worker returned a non-health frame during startup probe".to_string(),
            ));
        }
        Ok(())
    }

    fn restart_after_delay(&self) {
        let old = {
            let mut guard = match self.proc.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard.take()
        };
        self.pid.store(0, Ordering::Relaxed);
        cleanup_worker(old, "restart requested by worker response");
        self.record_restart("restart requested by worker response");
        thread::sleep(Duration::from_secs(1));
        let mut guard = match self.proc.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if guard.is_some() {
            return;
        }
        match self.spawn_ready() {
            Ok(p) => *guard = Some(p),
            Err(e) => eprintln!("wrapper: worker restart failed: {e}"),
        }
    }

    fn track_wait(&self) -> WaitTracker<'_> {
        self.waiting_count.fetch_add(1, Ordering::Relaxed);
        WaitTracker { worker: self }
    }

    fn track_request(&self, id: u32, opcode: u16) -> RequestTracker<'_> {
        if let Ok(mut state) = self.state.lock() {
            state.current = Some(CurrentRequest {
                id,
                opcode,
                started: Instant::now(),
            });
        }
        RequestTracker { worker: self, id }
    }

    fn record_error(&self, error: impl Into<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.last_error = Some(error.into());
        }
    }

    fn record_restart(&self, reason: impl Into<String>) {
        self.restart_count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut state) = self.state.lock() {
            state.last_restart_reason = Some(reason.into());
        }
    }

    fn abandon_locked_worker(
        &self,
        guard: &mut MutexGuard<'_, Option<WorkerProcess>>,
        reason: &'static str,
    ) {
        let old = guard.take();
        self.pid.store(0, Ordering::Relaxed);
        cleanup_worker(old, reason);
        self.record_restart(reason);
    }

    fn recover_stuck_worker_or_exit(&self, reason: &'static str) {
        let stuck = self
            .state
            .lock()
            .ok()
            .and_then(|s| s.current.clone())
            .map(|r| r.started.elapsed() >= self.busy_timeout)
            .unwrap_or(false);
        if !stuck {
            return;
        }
        let pid = self.pid.swap(0, Ordering::Relaxed);
        if pid == 0 {
            eprintln!(
                "wrapper: fatal: worker request is stale, but no worker pid is available; exiting for container restart"
            );
            std::process::exit(70);
        }
        self.record_restart(reason);
        thread::spawn(move || {
            eprintln!("wrapper: killing stuck worker pid={pid}: {reason}");
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        });
    }
}

fn set_nonblocking<T: AsRawFd>(fd: &T) -> Result<(), WorkerError> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io_err(io::Error::last_os_error()));
    }
    let rc = unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(io_err(io::Error::last_os_error()));
    }
    Ok(())
}

impl Drop for WaitTracker<'_> {
    fn drop(&mut self) {
        self.worker.waiting_count.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Drop for RequestTracker<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.worker.state.lock() {
            if state.current.as_ref().map(|r| r.id) == Some(self.id) {
                state.current = None;
            }
        }
    }
}

fn worker_timeout() -> Duration {
    std::env::var("WRAPPER_WORKER_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(60))
}

// How long to wait for a freshly-spawned worker to answer the OP_HEALTH
// readiness probe. A worker that hangs during Apple-lib init (e.g. the
// startup lease/restore network calls) never reaches its IPC read loop, so
// without this probe a wedged worker would eat a full request timeout before
// being discarded.
fn worker_startup_timeout() -> Duration {
    std::env::var("WRAPPER_WORKER_STARTUP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(30))
}

// Cap on how long a request will wait for the single worker mutex while
// another request is in flight. On PaaS the platform router kills connections
// long before the request timeout (Heroku H12 at 30s), so queueing every
// waiting request behind a stuck one just burns time; fail fast with 503
// instead so clients can retry.
fn worker_busy_timeout() -> Duration {
    std::env::var("WRAPPER_WORKER_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(10))
}

// Maximum number of requests queued behind the current one before new
// requests fail immediately with "worker busy".
fn worker_max_waiters() -> u32 {
    std::env::var("WRAPPER_WORKER_MAX_WAITERS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(16)
}

// Consecutive failed worker startups (spawn error or readiness probe
// timeout) before the supervisor gives up and exits so the PaaS platform
// restarts the dyno cleanly. Set WRAPPER_EXIT_ON_STARTUP_FAILURE=0 to keep
// serving 503s instead of exiting.
fn worker_max_startup_failures() -> u32 {
    std::env::var("WRAPPER_WORKER_MAX_RESTARTS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(3)
}

fn worker_exit_on_startup_failure() -> bool {
    let v = std::env::var("WRAPPER_EXIT_ON_STARTUP_FAILURE").unwrap_or_default();
    !matches!(v.trim(), "" | "0" | "false" | "no")
}

// Rootless worker setup (replaces the former C launcher). The daemon at
// /system/bin/main is exec'd through Android's linker64 via PT_INTERP, so
// bionic needs its environment variables and a writable Apple state dir.
fn setup_bionic_env() {
    let ensure = |name: &str, value: &str, overwrite: bool| {
        let missing = std::env::var(name).map(|v| v.is_empty()).unwrap_or(true);
        if overwrite || missing {
            std::env::set_var(name, value);
        }
    };
    ensure("ANDROID_DNS_MODE", "local", true);
    ensure("ANDROID_DATA", "/data", false);
    ensure("ANDROID_ROOT", "/system", false);
    ensure("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt", false);
    ensure("CURL_CA_BUNDLE", "/etc/ssl/certs/ca-certificates.crt", false);
}

fn ensure_apple_state_dirs() {
    let base_dir = std::env::var("WRAPPER_BASE_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/data/data/com.apple.android.music/files".to_string());
    if let Err(e) = std::fs::create_dir_all(format!("{base_dir}/mpl_db")) {
        eprintln!("wrapper: mkdir {base_dir}/mpl_db: {e}");
    }
}

fn io_err(e: io::Error) -> WorkerError {
    WorkerError::Io(e.to_string())
}

fn lock_worker_timeout(
    proc: &Mutex<Option<WorkerProcess>>,
    deadline: Instant,
) -> Result<MutexGuard<'_, Option<WorkerProcess>>, WorkerError> {
    loop {
        match proc.try_lock() {
            Ok(g) => return Ok(g),
            Err(TryLockError::Poisoned(_)) => {
                return Err(WorkerError::Unavailable(
                    "worker mutex poisoned".to_string(),
                ));
            }
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(WorkerError::Unavailable(
                        "worker busy timed out".to_string(),
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn cleanup_worker(proc: Option<WorkerProcess>, reason: &'static str) {
    if let Some(mut proc) = proc {
        thread::spawn(move || {
            eprintln!("wrapper: cleaning up old worker: {reason}");
            let _ = proc.child.kill();
            let _ = proc.child.wait();
        });
    }
}

fn write_frame_timeout(
    stdin: &mut ChildStdin,
    frame: &protocol::Frame,
    deadline: Instant,
) -> io::Result<()> {
    if frame.payload.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload too large",
        ));
    }
    let mut bytes = Vec::with_capacity(20 + frame.payload.len());
    bytes.extend_from_slice(&protocol::MAGIC.to_be_bytes());
    bytes.extend_from_slice(&protocol::VERSION.to_be_bytes());
    bytes.extend_from_slice(&frame.kind.to_be_bytes());
    bytes.extend_from_slice(&frame.request_id.to_be_bytes());
    bytes.extend_from_slice(&frame.opcode.to_be_bytes());
    bytes.extend_from_slice(&frame.flags.to_be_bytes());
    bytes.extend_from_slice(&(frame.payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&frame.payload);
    write_all_timeout(stdin, &bytes, deadline)?;
    stdin.flush()
}

fn read_frame_timeout(stdout: &mut ChildStdout, deadline: Instant) -> io::Result<protocol::Frame> {
    let mut h = [0u8; 20];
    read_exact_timeout(stdout, &mut h, deadline)?;
    let magic = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
    let version = u16::from_be_bytes([h[4], h[5]]);
    if magic != protocol::MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad ipc magic"));
    }
    if version != protocol::VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad ipc version",
        ));
    }
    let kind = u16::from_be_bytes([h[6], h[7]]);
    let request_id = u32::from_be_bytes([h[8], h[9], h[10], h[11]]);
    let opcode = u16::from_be_bytes([h[12], h[13]]);
    let flags = u16::from_be_bytes([h[14], h[15]]);
    let payload_len = u32::from_be_bytes([h[16], h[17], h[18], h[19]]) as usize;
    let mut payload = vec![0u8; payload_len];
    read_exact_timeout(stdout, &mut payload, deadline)?;
    Ok(protocol::Frame {
        kind,
        request_id,
        opcode,
        flags,
        payload,
    })
}

fn write_all_timeout<W: Write + AsRawFd>(
    writer: &mut W,
    mut buf: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    while !buf.is_empty() {
        wait_writable(writer.as_raw_fd(), deadline)?;
        match writer.write(buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "worker stdin closed",
                ))
            }
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_exact_timeout<R: Read + AsRawFd>(
    reader: &mut R,
    mut buf: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !buf.is_empty() {
        wait_readable(reader.as_raw_fd(), deadline)?;
        match reader.read(buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "worker stdout closed",
                ))
            }
            Ok(n) => {
                let tmp = buf;
                buf = &mut tmp[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn wait_readable(fd: i32, deadline: Instant) -> io::Result<()> {
    wait_fd(fd, libc::POLLIN, "worker stdout closed", deadline)
}

fn wait_writable(fd: i32, deadline: Instant) -> io::Result<()> {
    wait_fd(fd, libc::POLLOUT, "worker stdin closed", deadline)
}

fn wait_fd(fd: i32, events: i16, closed_message: &str, deadline: Instant) -> io::Result<()> {
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker ipc response timed out",
            ));
        }
        let remaining = deadline.saturating_duration_since(now);
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        pfd.revents = 0;
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n > 0 {
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, closed_message));
            }
            return Ok(());
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker ipc response timed out",
            ));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker ipc response timed out",
            ));
        }
    }
}

fn parse_worker_response(frame: protocol::Frame) -> Result<WorkerResponse, WorkerError> {
    let v: Value =
        serde_json::from_slice(&frame.payload).map_err(|e| WorkerError::Protocol(e.to_string()))?;
    let http_status = v.get("http_status").and_then(|v| v.as_u64()).unwrap_or(502) as u16;
    let content_type = v
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("application/json")
        .to_string();
    let restart_worker = v
        .get("restart_worker")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let body = match v.get("body") {
        Some(Value::String(s)) => s.as_bytes().to_vec(),
        Some(other) => serde_json::to_vec(other).unwrap_or_else(|_| {
            json!({"error":"invalid_worker_body"})
                .to_string()
                .into_bytes()
        }),
        None => Vec::new(),
    };
    Ok(WorkerResponse {
        http_status,
        content_type,
        body,
        restart_worker,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, OnceLock};

    // Fake worker that speaks the WV2I IPC protocol over stdin/stdout.
    // Modes:
    //   healthy - answers OP_HEALTH and every other request immediately
    //   slow    - answers the probe, then sleeps before answering work
    //   wedged  - never reads stdin, simulating a worker stuck during
    //             Apple-lib init before it reaches the IPC read loop
    const WORKER_SCRIPT: &str = r#"#!/usr/bin/env python3
import os, struct, sys, time

MAGIC = 0x57563249
VERSION = 1
KIND_RESPONSE = 2
OP_HEALTH = 1

def read_exact(f, n):
    buf = b""
    while len(buf) < n:
        chunk = f.read(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf

def read_frame():
    h = read_exact(sys.stdin.buffer, 20)
    _magic, _ver, _kind, request_id, opcode, _flags, plen = struct.unpack(">IHHIHHI", h)
    payload = read_exact(sys.stdin.buffer, plen) if plen else b""
    return request_id, opcode, payload

def write_frame(request_id, opcode, payload=b""):
    sys.stdout.buffer.write(struct.pack(">IHHIHHI", MAGIC, VERSION, KIND_RESPONSE, request_id, opcode, 0, len(payload)))
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()

mode = "MODE_PLACEHOLDER"
if mode == "wedged":
    time.sleep(3600)

while True:
    try:
        request_id, opcode, payload = read_frame()
    except EOFError:
        break
    if opcode == OP_HEALTH:
        write_frame(request_id, opcode, b'{"http_status":200,"content_type":"application/json","body":""}')
    elif mode == "slow":
        time.sleep(30)
        write_frame(request_id, opcode, b'{"http_status":200,"content_type":"application/json","body":""}')
    else:
        write_frame(request_id, opcode, b'{"http_status":200,"content_type":"application/json","body":""}')
"#;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    // env manipulation is process-global, so serialize tests that touch it
    // and restore the previous values afterwards.
    fn with_env<K: AsRef<str>, V: AsRef<str>>(vars: &[(K, V)], body: impl FnOnce()) {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.as_ref().to_string(), std::env::var(k.as_ref()).ok()))
            .collect();
        for (k, v) in vars {
            std::env::set_var(k.as_ref(), v.as_ref());
        }
        body();
        for (k, old) in saved {
            match old {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    fn write_worker_script(mode: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("wrapper-worker-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("worker-{mode}.py"));
        if path.exists() {
            return path;
        }
        std::fs::write(&path, WORKER_SCRIPT.replace("MODE_PLACEHOLDER", mode)).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn make_worker(mode: &str) -> Worker {
        Worker::new(write_worker_script(mode).to_str().unwrap(), "test".into())
    }

    fn base_env<'a>() -> Vec<(&'a str, String)> {
        vec![
            ("WRAPPER_BASE_DIR", std::env::temp_dir().to_string_lossy().into_owned()),
            ("WRAPPER_EXIT_ON_STARTUP_FAILURE", "0".into()),
        ]
    }

    #[test]
    fn healthy_worker_passes_probe_and_serves_health() {
        let env = base_env();
        let mut vars: Vec<(&str, String)> = vec![
            ("WRAPPER_WORKER_STARTUP_TIMEOUT_SECS", "5".into()),
            ("WRAPPER_WORKER_BUSY_TIMEOUT_MS", "500".into()),
            ("WRAPPER_WORKER_MAX_RESTARTS", "2".into()),
            ("WRAPPER_WORKER_TIMEOUT_SECS", "5".into()),
        ];
        vars.extend(env);
        let refs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        with_env(&refs, || {
            let w = make_worker("healthy");
            w.ensure_started().expect("worker should become ready");
            let h = w.health().expect("health should succeed");
            assert_eq!(h.http_status, 200);
            let snap = w.snapshot();
            assert!(snap["pid"].as_u64().unwrap() > 0);
            assert_eq!(snap["startup_failures"].as_u64().unwrap(), 0);
        });
    }

    #[test]
    fn wedged_worker_fails_probe_within_startup_timeout() {
        let env = base_env();
        let mut vars: Vec<(&str, String)> = vec![
            ("WRAPPER_WORKER_STARTUP_TIMEOUT_SECS", "1".into()),
            ("WRAPPER_WORKER_BUSY_TIMEOUT_MS", "500".into()),
            ("WRAPPER_WORKER_MAX_RESTARTS", "2".into()),
            ("WRAPPER_WORKER_TIMEOUT_SECS", "10".into()),
        ];
        vars.extend(env);
        let refs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        with_env(&refs, || {
            let w = make_worker("wedged");
            let start = Instant::now();
            let err = w.ensure_started().unwrap_err();
            assert!(
                err.to_string().contains("failed to become ready"),
                "unexpected error: {err}"
            );
            assert!(
                start.elapsed() < Duration::from_secs(8),
                "probe failure took too long: {:?}",
                start.elapsed()
            );
            assert_eq!(w.snapshot()["startup_failures"].as_u64().unwrap(), 2);
            // give the async worker-cleanup threads a moment to land
            thread::sleep(Duration::from_millis(300));
        });
    }

    #[test]
    fn busy_worker_fails_fast_instead_of_waiting_full_timeout() {
        let env = base_env();
        let mut vars: Vec<(&str, String)> = vec![
            ("WRAPPER_WORKER_STARTUP_TIMEOUT_SECS", "5".into()),
            ("WRAPPER_WORKER_BUSY_TIMEOUT_MS", "300".into()),
            ("WRAPPER_WORKER_MAX_RESTARTS", "2".into()),
            ("WRAPPER_WORKER_TIMEOUT_SECS", "3".into()),
        ];
        vars.extend(env);
        let refs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        with_env(&refs, || {
            let w = Arc::new(make_worker("slow"));
            w.ensure_started().expect("worker should become ready");
            let first = Arc::clone(&w);
            let first_thread = thread::spawn(move || {
                let _ = first.request(protocol::OP_PLAYBACK, Vec::new());
            });
            // let the first request claim the worker before the second arrives
            thread::sleep(Duration::from_millis(200));
            let start = Instant::now();
            let err = w.request(protocol::OP_PLAYBACK, Vec::new()).unwrap_err();
            let elapsed = start.elapsed();
            first_thread.join().unwrap();
            assert!(
                err.to_string().contains("worker busy timed out"),
                "unexpected error: {err}"
            );
            assert!(
                elapsed < Duration::from_millis(2500),
                "second request waited too long: {elapsed:?}"
            );
            // give the async worker-cleanup threads a moment to land
            thread::sleep(Duration::from_millis(300));
        });
    }
}
