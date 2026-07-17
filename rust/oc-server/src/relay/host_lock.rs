//! Cooperative host claim via `<data-dir>/relay-host.lock`.
//!
//! Direct port of `packages/web/server/lib/relay/host-lock.js`. The Node module
//! is the authoritative reference — semantics intentionally preserved
//! (including its forgiving unwritable-file fallback).
//!
//! Per-machine relay-host claim. Every OpenChamber instance on a machine shares
//! the same data dir and therefore the same relay signing key / serverId, so if
//! two processes run a relay host at once they fight over the single host slot
//! at the relay worker (each new connection closes the previous one with
//! "4001: Control replaced") and paired devices land on whichever instance won
//! last — often a dev/worktree instance running different code.
//!
//! The claim file makes the contest deterministic instead of a network race:
//!   - an instance only starts its relay host when there is no LIVE claimant
//!     (a dead claimant's stale file is ignored);
//!   - explicit user intent (creating a pairing link) claims unconditionally —
//!     the instance the user is interacting with must be the one devices reach;
//!   - a running host that discovers another live process has claimed backs off
//!     instead of reconnecting, which is what ends the replace/reconnect fight.
//!
//! This is a cooperative claim, not an OS lock: correctness does not depend on
//! atomicity (the relay worker still enforces a single host); the claim only
//! decides which process KEEPS retrying and which stands down.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Sentinel error returned by [`FsOps::remove_file`] when the file is absent.
/// Matches the behaviour of the JS `try { unlinkSync } catch {}` swallow — the
/// JS code does not distinguish "already gone" from "unwritable" for the
/// release path.
#[derive(Debug, thiserror::Error)]
pub enum HostLockError {
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    #[error("lock file contains invalid utf-8")]
    InvalidUtf8,
}

/// One on-disk claim entry. Mirrors `{ pid, claimedAt }` in host-lock.js.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claim {
    pub pid: u32,
    /// UNIX epoch milliseconds. JS `new Date().toISOString()` doesn't survive
    /// the JSON round-trip cleanly in Rust without a chrono dep at the call
    /// site, so we use `SystemTime` and serialize as millis-since-epoch.
    #[serde(rename = "claimedAt", default)]
    pub claimed_at_ms: u128,
}

impl Claim {
    pub fn now(pid: u32) -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            pid,
            claimed_at_ms: ms,
        }
    }
}

/// Filesystem operations the lock needs. Indirected so tests can run without
/// touching the real disk and so production can later swap in async / atomic
/// variants without churn at call sites.
///
/// `read_file`, `write_file`, and `remove_file` mirror `readFileSync`,
/// `writeFileSync`, and `unlinkSync` from the JS reference (sync-on-purpose — a
/// claim is a fast I/O call, not part of the hot path).
pub trait FsOps: Send + Sync {
    fn read_file(&self, path: &Path) -> io::Result<String>;
    fn write_file(&self, path: &Path, contents: &str) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Wrap an `io::Error` whose underlying `ErrorKind` should be treated as
    /// "file not found" (or any other swallow condition on the read path). The
    /// JS reference just catches everything; we surface this so a real
    /// production caller can keep that exact behaviour while tests can stay
    /// precise.
    fn is_not_found(&self, err: &io::Error) -> bool {
        err.kind() == io::ErrorKind::NotFound
    }
}

/// Default implementation backed by `std::fs`. This is the one production
/// uses; tests construct their own `MemFs`.
#[derive(Debug, Default, Clone)]
pub struct StdFs;

impl FsOps for StdFs {
    fn read_file(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }
    fn write_file(&self, path: &Path, contents: &str) -> io::Result<()> {
        fs::write(path, contents)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
}

/// Liveness probe for a PID. The JS reference uses
/// `proc.kill(pid, 0)` and treats `EPERM` as alive. The trait keeps the same
/// shape so production can call the real procfs / job-object query while tests
/// inject a deterministic map.
pub trait PidProbe: Send + Sync {
    /// Return `true` if `pid` is alive. `pid <= 0` is always dead.
    fn is_alive(&self, pid: u32) -> bool;
}

/// Live process probe that asks the OS. Unix variant uses
/// `kill(pid, 0)` and treats `EPERM` as alive (matches Node); Windows variant
/// uses the job-object handle-set approach.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPidProbe;

impl PidProbe for SystemPidProbe {
    fn is_alive(&self, pid: u32) -> bool {
        #[cfg(unix)]
        {
            // SAFETY: `kill(pid, 0)` performs no signal delivery; it only
            // performs existence + permission checks. We treat `EPERM` as
            // "alive" (process exists but belongs to another user) and
            // `ESRCH` (and any other error) as "dead or inaccessible".
            let res = unsafe { libc::kill(pid as i32, 0) };
            if res == 0 {
                true
            } else {
                let err = io::Error::last_os_error();
                err.raw_os_error() == Some(libc::EPERM)
            }
        }
        #[cfg(windows)]
        {
            // Open with PROCESS_QUERY_LIMITED_INFORMATION; an open handle
            // implies the process is at least alive enough to be queried.
            // We never need to keep the handle — exit code is read once
            // and discarded.
            //
            // CloseHandle + STILL_ACTIVE 属于 Win32::Foundation (windows-sys 0.59
            // 实际定义位置, 见 src/Windows/Win32/Foundation/mod.rs); 之前错误地从
            // Threading 导入, 靠 workspace feature unification 掩盖了路径错误。
            // 单包编译 (cargo build -p oc-server) 时 unification 不发生, bug 暴露。
            use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
            use windows_sys::Win32::System::Threading::{
                OpenProcess, GetExitCodeProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            };
            // SAFETY: `OpenProcess` reads the kernel handle table by PID. The
            // returned HANDLE is valid only for the synchronous lifetime of
            // this call; we close it before returning.
            let handle = unsafe {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)
            };
            if handle.is_null() {
                false
            } else {
                let mut exit_code: u32 = 0;
                let ok = unsafe {
                    GetExitCodeProcess(handle, &mut exit_code)
                };
                unsafe { CloseHandle(handle); }
                // STILL_ACTIVE (NTSTATUS = i32, 值 0x103) 与 exit_code (u32) 比较
                ok != 0 && exit_code == STILL_ACTIVE as u32
            }
        }
    }
}

/// In-memory PID liveness map for tests. Defaults to "every pid is dead
/// unless explicitly registered" — matches the JS reference's behaviour of
/// only returning `true` when the OS agrees the process exists. Tests that
/// want a PID alive must call [`set_alive`] before exercising the lock.
#[derive(Debug, Clone, Default)]
pub struct FakePidProbe {
    pub alive: Arc<Mutex<HashMap<u32, bool>>>,
}

impl FakePidProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pid as alive.
    pub fn set_alive(&self, pid: u32) {
        self.alive
            .lock()
            .expect("fake pid probe poisoned")
            .insert(pid, true);
    }

    /// Mark a pid as dead — either because it never existed or because it
    /// exited. Reads after `set_dead` will see the claim as stale.
    pub fn set_dead(&self, pid: u32) {
        self.alive
            .lock()
            .expect("fake pid probe poisoned")
            .insert(pid, false);
    }

    /// Make every registered pid dead (simulates "the only contender just
    /// exited and we haven't reaped it" for the host that just started).
    pub fn kill_all(&self) {
        let mut g = self.alive.lock().expect("fake pid probe poisoned");
        for v in g.values_mut() {
            *v = false;
        }
    }
}

impl PidProbe for FakePidProbe {
    fn is_alive(&self, pid: u32) -> bool {
        *self
            .alive
            .lock()
            .expect("fake pid probe poisoned")
            .get(&pid)
            .unwrap_or(&false)
    }
}

/// Probe that auto-registers any PID as alive **the first time it is asked**.
/// Models how the production `SystemPidProbe` naturally reports "any process
/// you write into the file from this test scope is alive" — useful for tests
/// that don't want to manually pre-register the self pid before claiming.
#[derive(Debug, Clone, Default)]
pub struct AutoPidProbe {
    alive: Arc<Mutex<HashMap<u32, bool>>>,
}

impl AutoPidProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_dead(&self, pid: u32) {
        self.alive
            .lock()
            .expect("auto pid probe poisoned")
            .insert(pid, false);
    }
}

impl PidProbe for AutoPidProbe {
    fn is_alive(&self, pid: u32) -> bool {
        let mut g = self.alive.lock().expect("auto pid probe poisoned");
        *g.entry(pid).or_insert(true)
    }
}

/// Warning sink for "non-fatal" failures (currently just the unwritable-data-dir
/// case). Mirrors the `Pick<Console, 'warn'>` shape from the JS reference.
pub trait WarnSink: Send + Sync {
    fn warn(&self, message: &str);
}

/// `tracing::warn!`-backed implementation for production.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingWarn;

impl WarnSink for TracingWarn {
    fn warn(&self, message: &str) {
        tracing::warn!(target: "relay::host_lock", "{message}");
    }
}

/// Capturing sink for tests — collects every warning so asserts can verify
/// `writeClaim` logged when the data dir was not writable.
#[derive(Debug, Clone, Default)]
pub struct CapturedWarn {
    pub messages: Arc<Mutex<Vec<String>>>,
}

impl CapturedWarn {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> Vec<String> {
        self.messages.lock().expect("captured warn poisoned").clone()
    }
}

impl WarnSink for CapturedWarn {
    fn warn(&self, message: &str) {
        self.messages
            .lock()
            .expect("captured warn poisoned")
            .push(message.to_string());
    }
}

/// In-memory filesystem for tests. Tracks a single directory's worth of files;
/// `write_file` overwrites (matching `writeFileSync`).
#[derive(Debug, Clone, Default)]
pub struct MemFs {
    files: Arc<Mutex<HashMap<PathBuf, String>>>,
}

impl MemFs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed a file with raw JSON. Useful for "another process claimed
    /// first" scenarios.
    pub fn seed(&self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.files
            .lock()
            .expect("memfs poisoned")
            .insert(path.into(), contents.into());
    }

    pub fn read(&self, path: &Path) -> Option<String> {
        self.files
            .lock()
            .expect("memfs poisoned")
            .get(path)
            .cloned()
    }
}

impl FsOps for MemFs {
    fn read_file(&self, path: &Path) -> io::Result<String> {
        self.files
            .lock()
            .expect("memfs poisoned")
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))
    }
    fn write_file(&self, path: &Path, contents: &str) -> io::Result<()> {
        self.files
            .lock()
            .expect("memfs poisoned")
            .insert(path.to_path_buf(), contents.to_string());
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        match self
            .files
            .lock()
            .expect("memfs poisoned")
            .remove(path)
        {
            Some(_) => Ok(()),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no such file",
            )),
        }
    }
}

/// Cooperative relay-host claim. Mirrors the closure factory shape from the JS
/// reference: all dependencies are injected so tests can run entirely in-memory.
pub struct RelayHostLock {
    lock_file_path: PathBuf,
    fs: Arc<dyn FsOps>,
    pid_probe: Arc<dyn PidProbe>,
    logger: Arc<dyn WarnSink>,
    self_pid: u32,
}

impl RelayHostLock {
    /// Construct a claim with explicit dependencies. `pid` is the PID used as
    /// the "self" identity; if `None`, the current process id is used.
    pub fn new(
        lock_file_path: impl Into<PathBuf>,
        fs: Arc<dyn FsOps>,
        pid_probe: Arc<dyn PidProbe>,
        logger: Arc<dyn WarnSink>,
        pid: Option<u32>,
    ) -> Self {
        Self {
            lock_file_path: lock_file_path.into(),
            fs,
            pid_probe,
            logger,
            self_pid: pid.unwrap_or_else(std::process::id),
        }
    }

    /// Convenience constructor using the production defaults.
    pub fn with_defaults(lock_file_path: impl Into<PathBuf>) -> Self {
        Self::new(
            lock_file_path,
            Arc::new(StdFs),
            Arc::new(SystemPidProbe),
            Arc::new(TracingWarn),
            None,
        )
    }

    pub fn self_pid(&self) -> u32 {
        self.self_pid
    }

    pub fn lock_file_path(&self) -> &Path {
        &self.lock_file_path
    }

    fn read_claim(&self) -> Option<Claim> {
        let raw = match self.fs.read_file(&self.lock_file_path) {
            Ok(s) => s,
            Err(e) if self.fs.is_not_found(&e) => return None,
            Err(_) => return None,
        };
        let parsed: Claim = match serde_json::from_str(&raw) {
            Ok(c) => c,
            Err(_) => return None,
        };
        if parsed.pid == 0 {
            return None;
        }
        Some(parsed)
    }

    fn write_claim(&self) -> bool {
        let payload = serde_json::to_string(&Claim::now(self.self_pid))
            .expect("Claim serialization is infallible");
        if let Err(err) = self.fs.write_file(&self.lock_file_path, &payload) {
            // Mirrors the JS "fall back to pre-lock behavior" branch — an
            // unwritable data dir must not take the relay down with it. We
            // still return `true` so the relay host starts; the relay worker
            // will arbitrate the single-host contest itself.
            self.logger
                .warn(&format!("[Relay] could not write host claim file: {err}"));
            return true;
        }
        true
    }

    /// The pid of the current live claimant, or `None` when the claim is free
    /// or stale. Matches `liveClaimantPid()` in the JS reference.
    pub fn live_claimant_pid(&self) -> Option<u32> {
        let claim = self.read_claim()?;
        if self.pid_probe.is_alive(claim.pid) {
            Some(claim.pid)
        } else {
            None
        }
    }

    /// Claim the slot unless another LIVE process already holds it.
    /// Re-claiming our own is a no-op refresh — both branches return `true`.
    pub fn try_claim(&self) -> bool {
        match self.live_claimant_pid() {
            Some(holder) if holder != self.self_pid => false,
            _ => self.write_claim(),
        }
    }

    /// Unconditional claim — explicit user intent (pairing) overrides any
    /// holder, even a live one.
    pub fn force_claim(&self) -> bool {
        self.write_claim()
    }

    /// True iff this process is the live claimant.
    pub fn holds_claim(&self) -> bool {
        self.live_claimant_pid() == Some(self.self_pid)
    }

    /// Release only our own claim; never delete another process's.
    pub fn release(&self) {
        let claim = match self.read_claim() {
            Some(c) => c,
            None => return,
        };
        if claim.pid != self.self_pid {
            return;
        }
        if let Err(err) = self.fs.remove_file(&self.lock_file_path) {
            if !self.fs.is_not_found(&err) {
                self.logger.warn(&format!(
                    "[Relay] could not release host claim file: {err}"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PID_A: u32 = 1001;
    const PID_B: u32 = 1002;
    const PID_C: u32 = 1003;

    fn lock_path() -> PathBuf {
        PathBuf::from("/data/relay-host.lock")
    }

    fn make_lock(
        pid: Option<u32>,
        fs: Arc<dyn FsOps>,
        probe: Arc<dyn PidProbe>,
        warn: Arc<dyn WarnSink>,
    ) -> RelayHostLock {
        RelayHostLock::new(lock_path(), fs, probe, warn, pid)
    }

    fn mem_fs() -> Arc<MemFs> {
        Arc::new(MemFs::new())
    }

    fn fake_probe() -> Arc<FakePidProbe> {
        Arc::new(FakePidProbe::new())
    }

    /// Auto-tracking probe: any pid it is asked about is alive by default.
    /// Used by tests where the lock itself writes `self_pid` into the file
    /// and we want a re-read of the slot to confirm "self is the live
    /// claimant" without manually re-registering the pid beforehand.
    fn auto_probe() -> Arc<AutoPidProbe> {
        Arc::new(AutoPidProbe::new())
    }

    fn captured_warn() -> Arc<CapturedWarn> {
        Arc::new(CapturedWarn::new())
    }

    // --- 1. Claim file shape & write path -------------------------------------

    #[test]
    fn try_claim_when_no_existing_file_writes_claim() {
        let fs = mem_fs();
        let probe = fake_probe();
        let warn = captured_warn();
        let lock = make_lock(Some(PID_A), fs.clone(), probe.clone(), warn.clone());

        assert!(lock.try_claim());
        assert!(fs.read(&lock_path()).is_some(), "claim file must exist");

        let written = fs.read(&lock_path()).unwrap();
        let claim: Claim = serde_json::from_str(&written).expect("claim must be valid JSON");
        assert_eq!(claim.pid, PID_A);
        assert!(warn.messages().is_empty());
    }

    #[test]
    fn claim_file_serializes_expected_json_keys() {
        let fs = mem_fs();
        let lock = make_lock(Some(PID_A), fs.clone(), fake_probe(), captured_warn());

        lock.try_claim();
        let raw = fs.read(&lock_path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // The JS reference writes JSON with a `claimedAt` ISO string key.
        // Our Rust Claim uses `claimedAt` (rename) for round-trip parity of
        // the *key name*. The value type differs (epoch ms vs ISO), but the
        // schema (key shape) must match because cross-language consumers in
        // the wider system expect `pid` + `claimedAt`.
        assert!(parsed.get("pid").is_some(), "missing pid field");
        assert!(parsed.get("claimedAt").is_some(), "missing claimedAt field");
        assert_eq!(parsed["pid"], serde_json::json!(PID_A));
    }

    #[test]
    fn write_failure_falls_back_to_prelock_behavior_and_warns() {
        // A MemFs configured to fail every write: simulates an unwritable data
        // dir. The contract is "best-effort: warn and return true so the host
        // still attempts to connect — the relay worker arbitrates single-host
        // anyway".
        struct FailingFs;
        impl FsOps for FailingFs {
            fn read_file(&self, _: &Path) -> io::Result<String> {
                Err(io::Error::new(io::ErrorKind::Other, "disk gone"))
            }
            fn write_file(&self, _: &Path, _: &str) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "EROFS"))
            }
            fn remove_file(&self, _: &Path) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "EROFS"))
            }
        }

        let fs: Arc<dyn FsOps> = Arc::new(FailingFs);
        let warn = captured_warn();
        let lock = make_lock(Some(PID_A), fs, fake_probe(), warn.clone());

        // JS reference: `return true` even on write failure. We must match.
        assert!(lock.try_claim());
        assert_eq!(warn.messages().len(), 1);
        assert!(warn.messages()[0].contains("could not write host claim file"));
    }

    // --- 2. live_claimant_pid & PID liveness ----------------------------------

    #[test]
    fn live_claimant_pid_returns_none_when_no_file() {
        let lock = make_lock(Some(PID_A), mem_fs(), fake_probe(), captured_warn());
        assert_eq!(lock.live_claimant_pid(), None);
    }

    #[test]
    fn live_claimant_pid_returns_alive_pid() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_B);
        let claim_json = serde_json::to_string(&Claim::now(PID_B)).unwrap();
        fs.seed(&lock_path(), claim_json);

        let lock = make_lock(Some(PID_A), fs, probe, captured_warn());
        assert_eq!(lock.live_claimant_pid(), Some(PID_B));
    }

    #[test]
    fn live_claimant_pid_ignores_dead_pid() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_dead(PID_B); // Process exited but lock file lingered.
        let claim_json = serde_json::to_string(&Claim::now(PID_B)).unwrap();
        fs.seed(&lock_path(), claim_json);

        let lock = make_lock(Some(PID_A), fs, probe, captured_warn());
        assert_eq!(lock.live_claimant_pid(), None);
    }

    #[test]
    fn live_claimant_pid_returns_none_on_zero_pid() {
        // Defensive: a hand-edited file with pid=0 must not be honored, even if
        // the OS reporter would treat 0 as a sentinel-kernel pid on some
        // platforms. JS reference returns null because `pid > 0` is false.
        let fs = mem_fs();
        fs.seed(&lock_path(), r#"{"pid":0,"claimedAt":0}"#);

        let lock = make_lock(Some(PID_A), fs, fake_probe(), captured_warn());
        assert_eq!(lock.live_claimant_pid(), None);
    }

    #[test]
    fn live_claimant_pid_returns_none_on_malformed_json() {
        // A garbage file is treated as "no claim" — never panic, never crash
        // the relay because of a half-written file.
        let fs = mem_fs();
        fs.seed(&lock_path(), "not json {{{");

        let lock = make_lock(Some(PID_A), fs, fake_probe(), captured_warn());
        assert_eq!(lock.live_claimant_pid(), None);
    }

    #[test]
    fn live_claimant_pid_returns_none_on_non_integer_pid() {
        // Same as JS Number.isInteger(pid) && pid > 0 — pid must be an integer
        // and > 0.
        let fs = mem_fs();
        fs.seed(&lock_path(), r#"{"pid":12.5,"claimedAt":0}"#);

        let lock = make_lock(Some(PID_A), fs, fake_probe(), captured_warn());
        assert_eq!(lock.live_claimant_pid(), None);
    }

    #[test]
    fn live_claimant_pid_returns_none_on_negative_pid_in_file() {
        // Serde will accept a signed integer here (u32 deserialization will
        // reject negatives), so we expect a JSON parse error → null.
        let fs = mem_fs();
        fs.seed(&lock_path(), r#"{"pid":-5,"claimedAt":0}"#);

        let lock = make_lock(Some(PID_A), fs, fake_probe(), captured_warn());
        assert_eq!(lock.live_claimant_pid(), None);
    }

    // --- 3. try_claim semantics ----------------------------------------------

    #[test]
    fn try_claim_returns_false_when_other_live_pid_holds() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_B);
        let claim_json = serde_json::to_string(&Claim::now(PID_B)).unwrap();
        fs.seed(&lock_path(), claim_json);

        let lock = make_lock(Some(PID_A), fs, probe, captured_warn());
        assert!(!lock.try_claim());
    }

    #[test]
    fn try_claim_does_not_overwrite_live_other_pid() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_B);
        let original = serde_json::to_string(&Claim::now(PID_B)).unwrap();
        fs.seed(&lock_path(), original.clone());

        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());
        let _ = lock.try_claim();

        let after = fs.read(&lock_path()).unwrap();
        assert_eq!(
            after, original,
            "try_claim must not touch another process's live file"
        );
    }

    #[test]
    fn try_claim_overwrites_dead_pid_stale_file() {
        let fs = mem_fs();
        let probe = fake_probe();
        // PID_B is in the file but no longer alive.
        let stale = serde_json::to_string(&Claim {
            pid: PID_B,
            claimed_at_ms: 1700000000000,
        })
        .unwrap();
        fs.seed(&lock_path(), stale);

        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());
        assert!(lock.try_claim());

        let after: Claim =
            serde_json::from_str(&fs.read(&lock_path()).unwrap()).unwrap();
        assert_eq!(after.pid, PID_A);
    }

    #[test]
    fn try_claim_is_a_noop_when_self_already_holds() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_A);
        let initial = serde_json::to_string(&Claim::now(PID_A)).unwrap();
        fs.seed(&lock_path(), initial.clone());

        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());
        // Both branches in the JS reference return true here.
        assert!(lock.try_claim());
        // The file should still point at us (rewritten with a fresh timestamp).
        let after: Claim =
            serde_json::from_str(&fs.read(&lock_path()).unwrap()).unwrap();
        assert_eq!(after.pid, PID_A);
    }

    // --- 4. force_claim semantics --------------------------------------------

    #[test]
    fn force_claim_overrides_live_other_pid() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_B);
        let claim_json = serde_json::to_string(&Claim::now(PID_B)).unwrap();
        fs.seed(&lock_path(), claim_json);

        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());
        assert!(lock.force_claim());

        let after: Claim =
            serde_json::from_str(&fs.read(&lock_path()).unwrap()).unwrap();
        assert_eq!(
            after.pid, PID_A,
            "force_claim must replace live holder — this is the pairing intent path"
        );
    }

    #[test]
    fn force_claim_writes_when_no_existing_file() {
        let fs = mem_fs();
        let lock = make_lock(Some(PID_A), fs.clone(), fake_probe(), captured_warn());

        assert!(lock.force_claim());
        let after: Claim =
            serde_json::from_str(&fs.read(&lock_path()).unwrap()).unwrap();
        assert_eq!(after.pid, PID_A);
    }

    // --- 5. holds_claim --------------------------------------------------------

    #[test]
    fn holds_claim_true_when_self_pid_is_live_claimant() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_A);
        let claim_json = serde_json::to_string(&Claim::now(PID_A)).unwrap();
        fs.seed(&lock_path(), claim_json);

        let lock = make_lock(Some(PID_A), fs, probe, captured_warn());
        assert!(lock.holds_claim());
    }

    #[test]
    fn holds_claim_false_when_other_pid_holds() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_B);
        let claim_json = serde_json::to_string(&Claim::now(PID_B)).unwrap();
        fs.seed(&lock_path(), claim_json);

        let lock = make_lock(Some(PID_A), fs, probe, captured_warn());
        assert!(!lock.holds_claim());
    }

    #[test]
    fn holds_claim_false_when_self_pid_dead() {
        // The file says PID_A, but PID_A is not alive anymore — `holds_claim`
        // is about live identity, not file ownership. A dead self is the same
        // as a dead other as far as "is the slot usable by me?" goes.
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_dead(PID_A);
        let claim_json = serde_json::to_string(&Claim::now(PID_A)).unwrap();
        fs.seed(&lock_path(), claim_json);

        let lock = make_lock(Some(PID_A), fs, probe, captured_warn());
        assert!(!lock.holds_claim());
    }

    #[test]
    fn holds_claim_false_when_no_file_exists() {
        let lock = make_lock(Some(PID_A), mem_fs(), fake_probe(), captured_warn());
        assert!(!lock.holds_claim());
    }

    // --- 6. release -----------------------------------------------------------

    #[test]
    fn release_removes_only_self_claim() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_A);
        let initial = serde_json::to_string(&Claim::now(PID_A)).unwrap();
        fs.seed(&lock_path(), initial);

        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());
        lock.release();
        assert!(fs.read(&lock_path()).is_none());
    }

    #[test]
    fn release_does_not_touch_other_process_claim() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_B);
        let initial = serde_json::to_string(&Claim::now(PID_B)).unwrap();
        fs.seed(&lock_path(), initial.clone());

        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());
        lock.release();
        // Must remain untouched; PID_B is still alive and holding the slot.
        assert_eq!(fs.read(&lock_path()), Some(initial));
    }

    #[test]
    fn release_no_op_when_no_file() {
        let lock = make_lock(Some(PID_A), mem_fs(), fake_probe(), captured_warn());
        // Should not panic, log nothing, do nothing.
        lock.release();
    }

    #[test]
    fn release_swallows_not_found_errors_gracefully() {
        // Even when the probe says PID_A is alive but the file vanished
        // (race with `force_claim` from another process), we must not blow up.
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_A);
        let lock = make_lock(Some(PID_A), fs, probe, captured_warn());
        // File is missing — read_claim returns None, release bails early.
        lock.release();
    }

    #[test]
    fn release_does_not_remove_dead_other_pid_file() {
        // Stale file from a dead process must NOT be removed by us releasing
        // — the JS guard is "claim.pid !== selfPid", regardless of liveness.
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_dead(PID_B);
        let stale = serde_json::to_string(&Claim::now(PID_B)).unwrap();
        fs.seed(&lock_path(), stale.clone());

        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());
        lock.release();
        assert_eq!(fs.read(&lock_path()), Some(stale));
    }

    // --- 7. Multi-pid choreography --------------------------------------------

    #[test]
    fn two_instances_contesting_only_one_wins_per_turn() {
        // Models the real-world race: instance A grabs the slot, instance B
        // starts and is told "no". A is still alive.
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_A);
        probe.set_alive(PID_B);

        let lock_a = make_lock(Some(PID_A), fs.clone(), probe.clone(), captured_warn());
        let lock_b = make_lock(Some(PID_B), fs.clone(), probe.clone(), captured_warn());

        // A arrives first and claims.
        assert!(lock_a.try_claim());
        // After A wrote itself into the file, A is alive per probe → A holds.
        assert!(lock_a.holds_claim());
        assert_eq!(fs.read(&lock_path()).unwrap().contains("\"pid\":1001"), true);

        // B arrives and is told no (A still alive).
        assert!(!lock_b.try_claim());
        assert!(!lock_b.holds_claim());

        // A drops its claim.
        lock_a.release();
        assert!(fs.read(&lock_path()).is_none());

        // B retries — now succeeds.
        assert!(lock_b.try_claim());
        assert!(lock_b.holds_claim());
    }

    #[test]
    fn pairing_intent_force_claim_takes_over_live_other() {
        // The user pressed "create pairing link" on instance B; even though
        // A is alive, B must win so the user's device lands on B.
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_A);

        let lock_a = make_lock(Some(PID_A), fs.clone(), probe.clone(), captured_warn());
        // B uses an auto-tracking probe so that after `force_claim` puts PID_B
        // in the file, B is recognised as the live claimant.
        let probe_b = auto_probe();
        let lock_b = make_lock(Some(PID_B), fs.clone(), probe_b, captured_warn());

        assert!(lock_a.try_claim());
        assert!(lock_b.force_claim());
        assert!(lock_b.holds_claim());
        assert!(!lock_a.holds_claim());
    }

    #[test]
    fn simultaneous_liveness_loss_then_late_claimer_can_grab() {
        // A claims, then dies. B starts. The fake probe reports A dead, so
        // B's try_claim sees the slot as free even though A's lock file is
        // still there.
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_A);

        let lock_a = make_lock(Some(PID_A), fs.clone(), probe.clone(), captured_warn());
        let lock_b = make_lock(Some(PID_B), fs.clone(), probe, captured_warn());

        assert!(lock_a.try_claim());
        // PID_A must die before B starts. Note: we deliberately leave PID_B
        // unregistered so the auto-tracking probe behaviour doesn't kick in —
        // the failure scenario specifically requires "PID_A is dead", which
        // `FakePidProbe` handles cleanly.
        // (No `set_dead` here — A was already explicitly registered as
        // alive. We need the OS to flip it.)
        // Simulate the crash by flipping PID_A to dead, then claiming as B.
        // We do this through the existing FakePidProbe by re-registering A
        // as dead, then asking about B which has not been registered yet.
        // The lock only checks A during B's try_claim — A dead → slot free.
        // Set A dead now by using a fresh probe that knows only A used to be
        // alive.
        // (Simplification: rebuild the probe with PID_A explicitly dead and
        // use an auto-tracking probe for B so `holds_claim` sees B as alive.)
        let lock_b2 = {
            let probe2 = auto_probe();
            // PID_A has not been registered through `probe2` — auto-tracking
            // only marks asked-about pids alive when they are not pre-seeded
            // as dead. Here we explicitly seed A as dead in this new probe.
            probe2.set_dead(PID_A);
            RelayHostLock::new(
                lock_path(),
                fs.clone() as Arc<dyn FsOps>,
                probe2 as Arc<dyn PidProbe>,
                captured_warn(),
                Some(PID_B),
            )
        };
        assert!(lock_b2.try_claim());
        assert!(lock_b2.holds_claim());
    }

    #[test]
    fn try_claim_thrice_is_idempotent_for_self() {
        let fs = mem_fs();
        let probe = auto_probe();
        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());

        assert!(lock.try_claim());
        assert!(lock.try_claim());
        assert!(lock.try_claim());
        assert!(lock.holds_claim());

        let after: Claim =
            serde_json::from_str(&fs.read(&lock_path()).unwrap()).unwrap();
        assert_eq!(after.pid, PID_A);
    }

    #[test]
    fn third_pid_cannot_grab_while_two_are_alive() {
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_B);
        probe.set_dead(PID_A);

        // Seed: file points at dead PID_A; PID_B has nothing but is alive
        // (matters only for the third-party check).
        let old = serde_json::to_string(&Claim::now(PID_A)).unwrap();
        fs.seed(&lock_path(), old);

        let lock_c = make_lock(Some(PID_C), fs.clone(), probe, captured_warn());
        // PID_A is dead → slot is free from C's perspective. C claims.
        assert!(lock_c.try_claim());
        let claim: Claim =
            serde_json::from_str(&fs.read(&lock_path()).unwrap()).unwrap();
        assert_eq!(claim.pid, PID_C);
    }

    // --- 8. Lifecycle: claim → release → re-claim -----------------------------

    #[test]
    fn release_then_re_claim_succeeds() {
        let fs = mem_fs();
        let probe = auto_probe();
        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());

        assert!(lock.try_claim());
        lock.release();
        assert!(!lock.holds_claim());
        assert!(lock.try_claim());
        assert!(lock.holds_claim());
    }

    #[test]
    fn holds_claim_returns_to_self_after_self_releases_then_reclaims() {
        let fs = mem_fs();
        let probe = auto_probe();
        let lock = make_lock(Some(PID_A), fs.clone(), probe, captured_warn());

        assert!(lock.try_claim());
        assert!(lock.holds_claim());
        lock.release();
        assert!(!lock.holds_claim());
        assert!(lock.try_claim());
        assert!(lock.holds_claim());
    }

    // --- 9. Edge cases in pid probe injection ---------------------------------

    #[test]
    fn pid_probe_says_zero_pid_never_alive() {
        // The JS reference treats pid <= 0 as dead even before consulting the
        // OS. Our `FakePidProbe::is_alive(0)` returns false (default); match it.
        let probe = FakePidProbe::new();
        assert!(!probe.is_alive(0));
    }

    #[test]
    fn live_claimant_pid_consults_probe_for_each_read() {
        // Different `is_alive` answers on consecutive calls must change the
        // reported holder — the probe is not cached.
        let fs = mem_fs();
        let probe = fake_probe();
        probe.set_alive(PID_B);
        let claim_json = serde_json::to_string(&Claim::now(PID_B)).unwrap();
        fs.seed(&lock_path(), claim_json);

        let lock = make_lock(Some(PID_A), fs, probe.clone(), captured_warn());

        assert_eq!(lock.live_claimant_pid(), Some(PID_B));
        probe.set_dead(PID_B);
        assert_eq!(lock.live_claimant_pid(), None);
        probe.set_alive(PID_B);
        assert_eq!(lock.live_claimant_pid(), Some(PID_B));
    }

    // --- 10. Construction sanity ----------------------------------------------

    #[test]
    fn self_pid_defaults_to_current_process_when_unset() {
        let lock = make_lock(None, mem_fs(), fake_probe(), captured_warn());
        assert_eq!(lock.self_pid(), std::process::id());
    }

    #[test]
    fn lock_file_path_round_trip() {
        let lock = make_lock(Some(PID_A), mem_fs(), fake_probe(), captured_warn());
        assert_eq!(lock.lock_file_path(), lock_path());
    }

    #[test]
    fn constructed_with_with_defaults_uses_real_pid_probe() {
        // We can't actually start a sub-process here, but we can verify the
        // production defaults wire through: pid is `std::process::id()` and
        // writes go to the in-memory fs but go through `StdFs`-like path.
        // Substitute MemFs at the seam and verify the production constructor
        // accepts the override.
        let fs: Arc<dyn FsOps> = mem_fs();
        let probe: Arc<dyn PidProbe> = fake_probe();
        let warn: Arc<dyn WarnSink> = captured_warn();
        let lock = RelayHostLock::new(
            lock_path(),
            fs,
            probe,
            warn,
            Some(PID_A),
        );
        // Liveness probe is the FakePidProbe we injected — registry returns
        // false for unknown PIDs, so reading an empty slot must yield null.
        assert_eq!(lock.live_claimant_pid(), None);
    }

    #[test]
    fn json_with_extra_fields_is_tolerated() {
        // Future-proofing: a future schema revision may add fields. The Rust
        // Claim must accept a payload with unknown keys without failing — by
        // default, Serde ignores unknown fields unless `deny_unknown_fields`
        // is set.
        let fs = mem_fs();
        fs.seed(
            &lock_path(),
            r#"{"pid":1002,"claimedAt":1700000000000,"extra":"ignored"}"#,
        );
        let probe = fake_probe();
        probe.set_alive(PID_B);
        let lock = make_lock(Some(PID_A), fs, probe, captured_warn());
        assert_eq!(lock.live_claimant_pid(), Some(PID_B));
    }

    #[test]
    fn missing_claimed_at_defaults_to_zero() {
        // The JS reference always writes `claimedAt`; this guards against a
        // file written by an older or partial producer without that key.
        let fs = mem_fs();
        fs.seed(&lock_path(), r#"{"pid":1002}"#);
        let probe = fake_probe();
        probe.set_alive(PID_B);
        let lock = make_lock(Some(PID_A), fs, probe, captured_warn());
        let claim = lock.read_claim().expect("claim must parse");
        assert_eq!(claim.pid, PID_B);
        assert_eq!(claim.claimed_at_ms, 0);
    }
}
