//! `system.processes` — PROCESS OBSERVATORY: a LIVE, STRICTLY READ-ONLY,
//! system-wide process-table feed.
//!
//! DARWIN "sees its machine working": a bounded poll ([procwatch].poll_secs)
//! that snapshots the LIVE process table via DIRECT libproc struct reads and
//! reduces it to one SECRET-FREE `system.processes` frame for the HUD — total
//! process count, top-N by CPU and by memory, how many processes are NEW since
//! the last poll, and the load average as context.
//!
//! ## Contract: READ-ONLY, unprivileged, honest (the vitals.rs discipline)
//!
//!   * READ-ONLY — it OBSERVES and REPORTS. It NEVER kills, signals, renices,
//!     suspends, or otherwise touches any process. No such code path exists in
//!     this module at all — acting on a process would be consequential and is
//!     out of scope by construction.
//!   * SECRET-FREE AT THE SYSCALL BOUNDARY — the collector performs, per poll,
//!     EXACTLY these reads and nothing else:
//!       - `proc_listallpids` (twice: size probe, then fill) for the pid list,
//!       - per pid: `proc_pidinfo(PROC_PIDTBSDINFO)` — one fixed-size
//!         `proc_bsdinfo` struct (short name, ppid, uid, start time) — and
//!         `proc_pidinfo(PROC_PIDTASKINFO)` — one fixed-size `proc_taskinfo`
//!         struct (resident bytes, cumulative CPU time),
//!       - `getloadavg` for the load context,
//!       - plus a one-time cached `mach_timebase_info` to convert CPU ticks.
//!
//!     The kernel's argv/environment block (the `KERN_PROCARGS2` sysctl) is
//!     NEVER issued and no path flavor (`proc_pidpath`, exe/cwd/fd/region
//!     info) is ever requested — argv and env routinely carry secrets (tokens,
//!     Authorization headers, key material), so not one argv byte, env byte,
//!     or file path ever transits this process's address space. This is
//!     deliberately NOT sysinfo: sysinfo's macOS backend unconditionally
//!     copies every readable process's full argv+env block into the caller's
//!     heap on refresh (its refresh-kind gates only skip the parsing, not the
//!     read), which would expose terminal-exported secrets to core dumps /
//!     debuggers / any future memory-disclosure bug.
//!
//!     WHOLE-DAEMON INVARIANT (a checked property, not just this module's).
//!     NO code path anywhere in darwind issues `KERN_PROCARGS2` or any
//!     argv/env/path flavor:
//!       - The daemon's TWO per-process readers — this module AND introspect.rs's
//!         resource sentinel — read ONLY the fixed-size libproc structs:
//!         `proc_pidinfo(PROC_PIDTBSDINFO)` (name/ppid/uid/start-time, here) and
//!         `proc_pidinfo(PROC_PIDTASKINFO)` (resident bytes + cumulative CPU
//!         ticks, in BOTH). Neither struct has an argv, environment, or
//!         exe/cwd/open-file field, so nothing downstream can surface one.
//!         introspect deliberately SHARES this module's reader ([`pidinfo`] /
//!         [`ticks_to_ns`]) rather than sysinfo's per-process API: sysinfo's
//!         `refresh_processes` would sysctl KERN_PROCARGS2 and copy its OWN
//!         tracked micro-apps' env — including a live `DARWIN_APP_TOKEN` — onto
//!         the daemon heap every sentinel tick (review-caught: introspect used
//!         sysinfo's per-process API before this).
//!       - The THREE host-level sysinfo users (vitals.rs, telemetry.rs,
//!         actions.rs) construct `System` with cpu + memory refresh kinds ONLY
//!         and NEVER refresh the process table (no `new_all` / `refresh_all` /
//!         `refresh_processes` / `.process(_)` / `.processes()` anywhere), so
//!         sysinfo never reaches its per-process sysctl path (review-caught:
//!         they previously used `System::new_all()`, whose everything-refresh
//!         DID sysctl and retain every process's argv+env).
//!
//!     The mechanical check is a repo-wide grep for `refresh_processes` /
//!     `ProcessRefreshKind` / `.process(` / `.processes()` / `new_all` over
//!     `daemon/src/*.rs`: it must find NO per-process sysinfo call in the daemon.
//!   * HONEST / degrades cleanly — every field is a REAL read; anything that
//!     cannot be read degrades to `None`/JSON null, NEVER a fabricated value
//!     (the vitals.rs `on_ac` precedent). In particular CPU % is a TWO-SAMPLE
//!     delta (cumulative task CPU time across consecutive polls): the FIRST
//!     poll has no baseline, so every `cpu_pct` is null and the top-CPU list
//!     is honestly EMPTY (a warm-up, never a fabricated 0.0%), exactly like
//!     `new_since_poll: null`. A process first seen this poll likewise reads
//!     null until its second sample. Memory is a point-in-time read and is
//!     honest from the first poll. `total` is the KERNEL'S pid count (the
//!     `proc_listallpids` list length), not the inspectable-row count — an
//!     unprivileged daemon can't read every pid's info (other-uid processes
//!     refuse TBSDINFO), and reporting only the readable subset as "total"
//!     would silently understate the machine.
//!   * BOUNDED — top-N is capped at [`PROCWATCH_MAX_TOP_N`], every name at
//!     [`PROCWATCH_MAX_NAME_CHARS`] chars (the kernel's own `pbi_name` is 32
//!     bytes anyway), and the counts/load are fixed-size scalars, so the frame
//!     has a fixed maximum size regardless of how many processes exist or how
//!     hostile their names are.
//!
//! ## PURE seam vs DEVICE-GATED runner (mirrors vitals.rs)
//!
//! The sample -> frame pipeline is a PURE, unit-tested seam over plain values:
//! [`baseline`] (the (pid, start-time) -> CPU-time map the next poll diffs
//! against), [`derive_records`] (the two-sample CPU-delta computation),
//! [`top_by_cpu`] / [`top_by_mem`] (deterministic tie-break; unmeasured
//! readings excluded, never ranked as 0), [`count_new`] (pid+start-time keyed,
//! so a reused pid still counts as new), [`truncate_name`], and
//! [`ProcSnapshot::to_json`]. The LIVE libproc read ([`procwatch_task`] +
//! `read_samples`) is a thin DEVICE-GATED runner and is NEVER exercised under
//! test.
//!
//! ## Boundary: NOT the Persistence Sentinel
//!
//! persistence.rs ("Autoruns for the Mac") watches the AUTOSTART surfaces —
//! LaunchAgents/LaunchDaemons directories, login items, cron — for CHANGES
//! against a baseline. procwatch is about the LIVE process table only: what is
//! running right now and what it costs. The two do not overlap and neither
//! duplicates the other.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;

/// Hard floor on the poll cadence, so a hostile/typo'd `poll_secs = 0` can
/// never busy-spin the read loop (the vitals.rs discipline). Walking the whole
/// process table is cheap but not free; a live panel needs nothing under 2s.
pub const PROCWATCH_MIN_POLL_SECS: u64 = 2;

/// Hard cap on the per-list top-N regardless of config, so a hostile/typo'd
/// `top_n` can never flood the frame / the HUD DOM. The HUD parser caps again.
pub const PROCWATCH_MAX_TOP_N: usize = 32;

/// Cap on the surfaced process-name length in chars (lossy-truncated), so a
/// hostile giant name can never balloon the frame. (The kernel's own
/// `pbi_name` field is 32 bytes, but the cap holds for ANY record source.)
pub const PROCWATCH_MAX_NAME_CHARS: usize = 64;

/// Sanity ceiling on a per-process CPU percent. Unlike a per-core reading, a
/// process percent can honestly exceed 100 on a multi-core machine (the delta
/// sums all its threads across cores), so this is NOT clamped to 100 — the cap
/// only bounds the frame against a garbage reading (64 cores x 100%).
const PROCWATCH_CPU_PCT_CAP: f32 = 6400.0;

// ---------------------------------------------------------------------------
// THE VALUES — plain, SECRET-FREE-by-construction process readings.
// ---------------------------------------------------------------------------

/// One RAW per-pid reading, straight off the two fixed-size kernel structs.
/// SECRET-FREE BY CONSTRUCTION: there is no argv, no environment, no
/// exe/cwd/open-file field here AT ALL — the collector never issues the
/// syscalls that carry them, so no downstream code can ever surface them.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcSample {
    pub pid: u32,
    /// Parent pid, or `None` when unreadable (honest absent).
    pub ppid: Option<u32>,
    /// The kernel's SHORT process name (`pbi_name`/`pbi_comm` — never a
    /// command line), capped at [`PROCWATCH_MAX_NAME_CHARS`] at assemble time.
    pub name: String,
    /// Unix start time (seconds). Half of the (pid, start) identity key.
    pub start_time_secs: u64,
    /// Owning uid, or `None` where unavailable (honest absent).
    pub uid: Option<u32>,
    /// Resident memory bytes, or `None` when TASKINFO was unreadable.
    pub mem_bytes: Option<u64>,
    /// CUMULATIVE task CPU time (user+system) in ns, or `None` when TASKINFO
    /// or the timebase was unreadable. The delta of two of these across a poll
    /// interval is the honest CPU %.
    pub cpu_time_ns: Option<u64>,
}

/// One DERIVED process record — a [`ProcSample`] with its CPU % computed from
/// the previous poll's baseline. `cpu_pct: None` means "no honest two-sample
/// delta exists yet" (first poll, brand-new process, or unreadable time) and
/// serializes to null — NEVER a fabricated 0.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcRecord {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
    pub start_time_secs: u64,
    pub uid: Option<u32>,
}

/// The identity key for "same process across two polls": pid ALONE is not
/// enough (macOS reuses pids), so the start time disambiguates — a reused pid
/// with a different start time is a NEW process.
pub type ProcKey = (u32, u64);

/// The previous poll's per-process cumulative CPU time (ns; `None` where it
/// was unreadable then). The KEY SET doubles as the new-process baseline.
pub type CpuBaseline = HashMap<ProcKey, Option<u64>>;

// ---------------------------------------------------------------------------
// PURE REDUCTION SEAMS — unit-tested on synthetic values, no live system.
// ---------------------------------------------------------------------------

/// Build the [`CpuBaseline`] the NEXT poll will diff against. PURE.
pub fn baseline(samples: &[ProcSample]) -> CpuBaseline {
    samples
        .iter()
        .map(|s| ((s.pid, s.start_time_secs), s.cpu_time_ns))
        .collect()
}

/// The two-sample CPU delta: `Some(pct)` only when BOTH endpoints of the delta
/// were actually measured for this exact (pid, start) identity and real time
/// elapsed — otherwise `None` (an honest "not measured yet", never 0). PURE.
fn cpu_delta_pct(
    prev: Option<&CpuBaseline>,
    key: ProcKey,
    now_ns: Option<u64>,
    elapsed_ns: u64,
) -> Option<f32> {
    let prev_ns = (*prev?.get(&key)?)?;
    let now_ns = now_ns?;
    if elapsed_ns == 0 {
        return None;
    }
    // A cumulative counter can't honestly go backwards; a clock quirk clamps
    // to a MEASURED 0, not a fabricated one.
    let delta = now_ns.saturating_sub(prev_ns);
    Some(((delta as f64 / elapsed_ns as f64) * 100.0) as f32)
}

/// Derive the records for this poll: carry every sample's point-in-time fields
/// and compute its CPU % against the previous poll's baseline. On the FIRST
/// poll (`prev: None`) every `cpu_pct` is `None` — the honest warm-up. PURE.
pub fn derive_records(
    samples: Vec<ProcSample>,
    prev: Option<&CpuBaseline>,
    elapsed_ns: u64,
) -> Vec<ProcRecord> {
    samples
        .into_iter()
        .map(|s| {
            let cpu_pct =
                cpu_delta_pct(prev, (s.pid, s.start_time_secs), s.cpu_time_ns, elapsed_ns);
            ProcRecord {
                pid: s.pid,
                ppid: s.ppid,
                name: s.name,
                cpu_pct,
                mem_bytes: s.mem_bytes,
                start_time_secs: s.start_time_secs,
                uid: s.uid,
            }
        })
        .collect()
}

/// Sanitize a CPU percent for ordering/serialization: non-finite -> 0 (never a
/// fabricated load), clamped into 0..=[`PROCWATCH_CPU_PCT_CAP`].
fn sane_cpu(c: f32) -> f32 {
    if c.is_finite() {
        c.clamp(0.0, PROCWATCH_CPU_PCT_CAP)
    } else {
        0.0
    }
}

/// Round a sanitized CPU percent to 1 decimal (as f64 for a clean JSON number).
fn round_pct(c: f32) -> f64 {
    ((sane_cpu(c) as f64) * 10.0).round() / 10.0
}

/// Round a load-average component to 2 decimals; a non-finite value -> 0
/// (same shape as vitals.rs).
fn round_load(x: f64) -> f64 {
    if x.is_finite() {
        (x.max(0.0) * 100.0).round() / 100.0
    } else {
        0.0
    }
}

/// Cap a process name at [`PROCWATCH_MAX_NAME_CHARS`] chars (char-boundary
/// safe, so a hostile multi-byte name can never split a code point). PURE.
pub fn truncate_name(name: &str) -> String {
    name.chars().take(PROCWATCH_MAX_NAME_CHARS).collect()
}

/// Top `n` MEASURED records by CPU, descending, tie-broken by ascending pid so
/// equal readings order DETERMINISTICALLY. Records with `cpu_pct: None` (no
/// honest delta yet) are EXCLUDED, never ranked as a fabricated 0 — so the
/// first poll yields an honestly EMPTY list. `n` is capped at
/// [`PROCWATCH_MAX_TOP_N`]. PURE.
pub fn top_by_cpu(procs: &[ProcRecord], n: usize) -> Vec<&ProcRecord> {
    let mut v: Vec<&ProcRecord> = procs.iter().filter(|p| p.cpu_pct.is_some()).collect();
    v.sort_by(|a, b| {
        // sane_cpu never yields NaN, so partial_cmp is always Some; the
        // Equal fallback is belt-and-braces, not a reachable arm.
        sane_cpu(b.cpu_pct.unwrap_or(0.0))
            .partial_cmp(&sane_cpu(a.cpu_pct.unwrap_or(0.0)))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.pid.cmp(&b.pid))
    });
    v.truncate(n.min(PROCWATCH_MAX_TOP_N));
    v
}

/// Top `n` MEASURED records by memory, descending, tie-broken by ascending
/// pid. Records with `mem_bytes: None` are EXCLUDED (an unreadable reading
/// can't honestly claim a top slot). `n` is capped at
/// [`PROCWATCH_MAX_TOP_N`]. PURE.
pub fn top_by_mem(procs: &[ProcRecord], n: usize) -> Vec<&ProcRecord> {
    let mut v: Vec<&ProcRecord> = procs.iter().filter(|p| p.mem_bytes.is_some()).collect();
    v.sort_by(|a, b| {
        b.mem_bytes
            .unwrap_or(0)
            .cmp(&a.mem_bytes.unwrap_or(0))
            .then(a.pid.cmp(&b.pid))
    });
    v.truncate(n.min(PROCWATCH_MAX_TOP_N));
    v
}

/// Count the processes in `procs` whose (pid, start-time) key is absent from
/// the previous poll's baseline — i.e. processes STARTED since the last poll.
/// Keyed on pid + start time so a REUSED pid still counts as new. PURE.
pub fn count_new(procs: &[ProcRecord], prev: &CpuBaseline) -> usize {
    procs
        .iter()
        .filter(|p| !prev.contains_key(&(p.pid, p.start_time_secs)))
        .count()
}

// ---------------------------------------------------------------------------
// THE SNAPSHOT — a PURE value; `to_json` is the tested assemble seam.
// ---------------------------------------------------------------------------

/// One assembled reading of the process table. A PURE value — the live task
/// fills it from the libproc read, and [`to_json`](Self::to_json) is the
/// tested reduce/assemble seam.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcSnapshot {
    /// The kernel's OWN live pid count (the `proc_listallpids` list length) —
    /// NOT `procs.len()`: an unprivileged daemon cannot inspect every pid
    /// (other-uid processes refuse TBSDINFO), so the inspectable rows are a
    /// subset. Reporting only the subset as "total" would silently understate
    /// the machine; the kernel's count is a real read and is carried honestly.
    pub total: usize,
    /// The INSPECTABLE rows (TBSDINFO succeeded). Only these can appear in the
    /// top lists / the new-since-poll count.
    pub procs: Vec<ProcRecord>,
    /// Load average (1 / 5 / 15 minute) — the frame's load context — or `None`
    /// when unreadable (serializes to an honest null, never fabricated zeros).
    pub load_avg: Option<(f64, f64, f64)>,
}

/// Serialize one top-list entry. SECRET-FREE: exactly name/pid/ppid/uid/
/// cpu_pct/mem_bytes — the input record has no argv/env/path field to leak.
/// Unmeasured cpu/mem serialize to null, never a fabricated 0.
fn entry_json(p: &ProcRecord) -> Value {
    json!({
        "name": truncate_name(&p.name),
        "pid": p.pid,
        "ppid": p.ppid,
        "uid": p.uid,
        "cpu_pct": p.cpu_pct.map(round_pct),
        "mem_bytes": p.mem_bytes,
    })
}

impl ProcSnapshot {
    /// Assemble the SECRET-FREE `system.processes` wire JSON. PURE — bounded
    /// top-N by CPU (measured records only — honestly EMPTY on the first,
    /// baseline-less poll) and by memory, the total count, the
    /// new-since-last-poll count (`prev` is the previous poll's baseline;
    /// `None` on the FIRST poll serializes to an honest null — we genuinely
    /// have no baseline yet, and a fabricated 0 would claim "nothing new" we
    /// never measured), and the load average (null when unreadable).
    /// Unit-tested on synthetic records.
    pub fn to_json(&self, prev: Option<&CpuBaseline>, top_n: usize) -> Value {
        let top_cpu: Vec<Value> =
            top_by_cpu(&self.procs, top_n).into_iter().map(entry_json).collect();
        let top_mem: Vec<Value> =
            top_by_mem(&self.procs, top_n).into_iter().map(entry_json).collect();
        let new_since_poll: Option<usize> = prev.map(|set| count_new(&self.procs, set));
        let load_avg: Value = match self.load_avg {
            Some((one, five, fifteen)) => {
                json!([round_load(one), round_load(five), round_load(fifteen)])
            }
            None => Value::Null,
        };
        json!({
            "total": self.total,
            "new_since_poll": new_since_poll,
            "top_cpu": top_cpu,
            "top_mem": top_mem,
            "load_avg": load_avg,
        })
    }
}

// ---------------------------------------------------------------------------
// DEVICE-GATED RUNNER — the live libproc reads (NEVER exercised under test).
//
// The complete per-poll syscall inventory: proc_listallpids (size probe +
// fill), then per pid ONE proc_pidinfo(PROC_PIDTBSDINFO) + ONE
// proc_pidinfo(PROC_PIDTASKINFO) — both fixed-size struct fills — and ONE
// getloadavg. Nothing else: no KERN_PROCARGS2, no proc_pidpath, no fd/region
// flavors. mach_timebase_info runs once and is cached.
// ---------------------------------------------------------------------------

/// Mach timebase for converting task CPU ticks to nanoseconds. Declared
/// locally (the power.rs shim precedent) because libc's re-export is
/// deprecated; the symbol lives in libSystem, so the bare extern links with
/// nothing extra.
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

#[cfg(target_os = "macos")]
extern "C" {
    // READ-ONLY: fills one 8-byte numer/denom struct; returns KERN_SUCCESS(0).
    fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
}

/// The cached tick->ns ratio, or `None` when the timebase itself was
/// unreadable — in which case CPU times degrade to `None` rather than being
/// scaled by a GUESSED ratio (on Apple Silicon the ratio is not 1:1, so a
/// fabricated identity would misreport CPU by ~41x).
#[cfg(target_os = "macos")]
fn timebase() -> Option<(u64, u64)> {
    use std::sync::OnceLock;
    static TB: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = MachTimebase { numer: 0, denom: 0 };
        // SAFETY: fills exactly one 8-byte out-struct we own; no aliasing.
        let rc = unsafe { mach_timebase_info(&mut info) };
        (rc == 0 && info.numer != 0 && info.denom != 0)
            .then(|| (u64::from(info.numer), u64::from(info.denom)))
    })
}

/// Convert mach CPU ticks to nanoseconds, or `None` when the timebase is
/// unknown (honest absent, never a guessed scale). Also reused by introspect.rs's
/// resource sentinel so it shares this KERN_PROCARGS2-free reader (see module doc).
#[cfg(target_os = "macos")]
pub(crate) fn ticks_to_ns(ticks: u64) -> Option<u64> {
    let (numer, denom) = timebase()?;
    Some(((u128::from(ticks) * u128::from(numer)) / u128::from(denom)) as u64)
}

/// NUL-terminated fixed-size kernel char array -> lossy String. The input is
/// one of the two SHORT-NAME fields (16/32 bytes) — never a command line.
#[cfg(target_os = "macos")]
fn cstr_lossy(bytes: &[libc::c_char]) -> String {
    let raw: Vec<u8> = bytes.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&raw).into_owned()
}

/// One fixed-size `proc_pidinfo` struct fill, or `None` on any failure (dead
/// pid, EPERM, short read) — the caller degrades honestly. Reused by
/// introspect.rs's resource sentinel (for `PROC_PIDTASKINFO`) so BOTH per-process
/// readers issue only fixed-size struct reads, never KERN_PROCARGS2 (module doc).
#[cfg(target_os = "macos")]
pub(crate) fn pidinfo<T: Copy>(pid: i32, flavor: libc::c_int) -> Option<T> {
    let size = std::mem::size_of::<T>() as libc::c_int;
    // SAFETY: T is a plain-old-data kernel struct (proc_bsdinfo /
    // proc_taskinfo) for which all-zeroes is a valid bit pattern; the kernel
    // writes at most `size` bytes into it and returns how many it filled
    // (== size on success, <= 0 / short on failure).
    let mut info: T = unsafe { std::mem::zeroed() };
    let n = unsafe {
        libc::proc_pidinfo(pid, flavor, 0, (&mut info as *mut T).cast::<libc::c_void>(), size)
    };
    (n == size).then_some(info)
}

/// The kernel's current pid list via `proc_listallpids` (size probe + fill,
/// with headroom for processes spawned in between). An empty Vec on failure —
/// honest empty, never a fabricated table.
#[cfg(target_os = "macos")]
fn read_pids() -> (usize, Vec<i32>) {
    // SAFETY: a NULL buffer asks the kernel for the current pid count only.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return (0, Vec::new());
    }
    let cap = n as usize + 64;
    let mut pids = vec![0i32; cap];
    // SAFETY: the buffer really is `cap` i32s and `buffersize` says so in
    // bytes; the kernel fills at most that and returns how many pids it wrote.
    let filled = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast::<libc::c_void>(),
            (cap * std::mem::size_of::<i32>()) as libc::c_int,
        )
    };
    if filled <= 0 {
        return (0, Vec::new());
    }
    pids.truncate(filled as usize);
    // `kernel_count` is the honest total: the kernel's own list length,
    // INCLUDING pid 0 (kernel_task). The retain below only prunes the
    // per-pid INSPECTION list — pid 0 refuses TBSDINFO unprivileged, so
    // inspecting it is a guaranteed-wasted syscall — and must never shrink
    // the reported total (review-caught off-by-one: total was the kernel
    // list length MINUS the real pid-0 entry).
    let kernel_count = pids.len();
    pids.retain(|&p| p > 0);
    (kernel_count, pids)
}

/// One pid -> one [`ProcSample`], from the TWO fixed-size struct reads and
/// nothing else. `None` when even TBSDINFO is unreadable (the pid died / is
/// invisible) — the process is then honestly absent from the frame. A failed
/// TASKINFO (some zombies / protected tasks) degrades mem/cpu to `None`,
/// keeping the row.
#[cfg(target_os = "macos")]
fn read_sample(pid: i32) -> Option<ProcSample> {
    let bsd: libc::proc_bsdinfo = pidinfo(pid, libc::PROC_PIDTBSDINFO)?;
    let long = cstr_lossy(&bsd.pbi_name);
    let name = if long.is_empty() { cstr_lossy(&bsd.pbi_comm) } else { long };
    let task: Option<libc::proc_taskinfo> = pidinfo(pid, libc::PROC_PIDTASKINFO);
    Some(ProcSample {
        pid: pid as u32,
        ppid: Some(bsd.pbi_ppid),
        name,
        start_time_secs: bsd.pbi_start_tvsec,
        uid: Some(bsd.pbi_uid),
        mem_bytes: task.as_ref().map(|t| t.pti_resident_size),
        cpu_time_ns: task
            .as_ref()
            .and_then(|t| ticks_to_ns(t.pti_total_user.saturating_add(t.pti_total_system))),
    })
}

/// The full live table read: the pid list + the two struct reads per pid.
/// Returns `(total, samples)` where `total` is the KERNEL'S pid count — an
/// unprivileged daemon can't inspect every pid (other-uid processes refuse
/// TBSDINFO), so `samples` is the inspectable subset and reporting only its
/// length as "total" would silently understate the machine.
#[cfg(target_os = "macos")]
fn read_samples() -> (usize, Vec<ProcSample>) {
    let (total, pids) = read_pids();
    (total, pids.into_iter().filter_map(read_sample).collect())
}

/// Off macOS there is no libproc: an honest empty table, never a fabrication.
#[cfg(not(target_os = "macos"))]
fn read_samples() -> (usize, Vec<ProcSample>) {
    (0, Vec::new())
}

/// The 1/5/15-minute load average via `getloadavg`, or `None` on failure
/// (serializes to an honest null, never fabricated zeros).
#[cfg(target_os = "macos")]
fn read_load() -> Option<(f64, f64, f64)> {
    let mut la = [0.0f64; 3];
    // SAFETY: getloadavg fills at most 3 doubles into a buffer of exactly 3
    // and returns how many samples it retrieved (-1 on failure).
    let n = unsafe { libc::getloadavg(la.as_mut_ptr(), 3) };
    (n == 3).then_some((la[0], la[1], la[2]))
}

#[cfg(not(target_os = "macos"))]
fn read_load() -> Option<(f64, f64, f64)> {
    None
}

/// The live `system.processes` poll. STRICTLY READ-ONLY: every tick it walks
/// the process table via the fixed-size libproc struct reads listed in the
/// runner header (argv/env/paths are NEVER requested from the kernel) and
/// emits one bounded, SECRET-FREE frame for the HUD. It acts on NOTHING — no
/// kill, no signal, no renice exists here. Gated by [procwatch].enabled — OFF,
/// it returns immediately and never spawns a read. The poll cadence is clamped
/// to [`PROCWATCH_MIN_POLL_SECS`]; top-N to [`PROCWATCH_MAX_TOP_N`].
pub async fn procwatch_task(cfg: Arc<Config>) {
    if !cfg.procwatch.enabled {
        return;
    }
    let poll = cfg.procwatch.poll_secs.max(PROCWATCH_MIN_POLL_SECS);
    let top_n = cfg.procwatch.top_n.min(PROCWATCH_MAX_TOP_N as u64) as usize;
    let mut prev: Option<CpuBaseline> = None;
    let mut last_tick: Option<std::time::Instant> = None;
    let mut interval = tokio::time::interval(Duration::from_secs(poll));
    loop {
        interval.tick().await;
        // The measured monotonic gap between THIS sample and the previous one
        // is the CPU-delta denominator (0 on the first tick, which the pure
        // seam treats as "no delta possible").
        let now = std::time::Instant::now();
        let elapsed_ns = last_tick
            .map(|t| u64::try_from(now.duration_since(t).as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        last_tick = Some(now);
        let (total, samples) = read_samples();
        let next_baseline = baseline(&samples);
        let records = derive_records(samples, prev.as_ref(), elapsed_ns);
        let snapshot = ProcSnapshot { total, procs: records, load_avg: read_load() };
        let frame = snapshot.to_json(prev.as_ref(), top_n);
        prev = Some(next_baseline);
        crate::telemetry::emit("system", "system.processes", frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic DERIVED record — the pure seams never see a live process.
    fn rec(pid: u32, name: &str, cpu: Option<f32>, mem: Option<u64>, start: u64) -> ProcRecord {
        ProcRecord {
            pid,
            ppid: Some(1),
            name: name.into(),
            cpu_pct: cpu,
            mem_bytes: mem,
            start_time_secs: start,
            uid: Some(501),
        }
    }

    /// A synthetic RAW sample (cumulative CPU ns, as the collector reports).
    fn smp(pid: u32, name: &str, start: u64, cpu_ns: Option<u64>, mem: Option<u64>) -> ProcSample {
        ProcSample {
            pid,
            ppid: Some(1),
            name: name.into(),
            start_time_secs: start,
            uid: Some(501),
            mem_bytes: mem,
            cpu_time_ns: cpu_ns,
        }
    }

    fn snap(procs: Vec<ProcRecord>) -> ProcSnapshot {
        ProcSnapshot { total: procs.len(), procs, load_avg: Some((1.234, 0.5, 0.0)) }
    }

    // --- top-N selection (PURE) ----------------------------------------------

    #[test]
    fn top_selection_orders_desc_and_breaks_ties_by_pid() {
        let procs = vec![
            rec(30, "c", Some(50.0), Some(10), 0),
            rec(10, "a", Some(50.0), Some(30), 0), // CPU tie with pid 30 -> lower pid first
            rec(20, "b", Some(90.0), Some(20), 0),
        ];
        let cpu: Vec<u32> = top_by_cpu(&procs, 3).iter().map(|p| p.pid).collect();
        assert_eq!(cpu, vec![20, 10, 30], "cpu desc, ties by ascending pid");
        let mem: Vec<u32> = top_by_mem(&procs, 2).iter().map(|p| p.pid).collect();
        assert_eq!(mem, vec![10, 20], "mem desc, truncated to n");
    }

    #[test]
    fn unmeasured_readings_are_excluded_from_top_lists_never_ranked_as_zero() {
        let procs = vec![
            rec(1, "warmup", None, Some(100), 0), // no cpu delta yet
            rec(2, "busy", Some(5.0), None, 0),   // mem unreadable
        ];
        let cpu: Vec<u32> = top_by_cpu(&procs, 8).iter().map(|p| p.pid).collect();
        assert_eq!(cpu, vec![2], "cpu:None is excluded, not ranked as a fabricated 0");
        let mem: Vec<u32> = top_by_mem(&procs, 8).iter().map(|p| p.pid).collect();
        assert_eq!(mem, vec![1], "mem:None is excluded, not ranked as a fabricated 0");
    }

    #[test]
    fn top_n_is_capped_at_32_even_for_a_hostile_n() {
        let procs: Vec<ProcRecord> = (0..100)
            .map(|i| rec(i, "p", Some(i as f32), Some(u64::from(i)), 0))
            .collect();
        assert_eq!(top_by_cpu(&procs, usize::MAX).len(), PROCWATCH_MAX_TOP_N);
        assert_eq!(top_by_mem(&procs, 10_000).len(), PROCWATCH_MAX_TOP_N);
        let v = snap(procs).to_json(None, usize::MAX);
        assert_eq!(v["top_cpu"].as_array().unwrap().len(), PROCWATCH_MAX_TOP_N);
        assert_eq!(v["top_mem"].as_array().unwrap().len(), PROCWATCH_MAX_TOP_N);
    }

    #[test]
    fn nan_cpu_sorts_as_zero_never_poisons_the_order() {
        let procs = vec![
            rec(1, "nan", Some(f32::NAN), Some(0), 0),
            rec(2, "busy", Some(10.0), Some(0), 0),
        ];
        let top: Vec<u32> = top_by_cpu(&procs, 2).iter().map(|p| p.pid).collect();
        assert_eq!(top, vec![2, 1], "NaN reads as 0, not as greatest");
    }

    // --- name truncation (PURE) ----------------------------------------------

    #[test]
    fn hostile_giant_name_is_truncated_lossy_and_char_safe() {
        // 10k chars of multi-byte content: the cap must count CHARS (never
        // split a code point) and hold the frame bounded.
        let giant = "é".repeat(10_000);
        let t = truncate_name(&giant);
        assert_eq!(t.chars().count(), PROCWATCH_MAX_NAME_CHARS);
        let v = snap(vec![rec(7, &giant, Some(1.0), Some(1), 0)]).to_json(None, 1);
        let name = v["top_cpu"][0]["name"].as_str().unwrap();
        assert_eq!(name.chars().count(), PROCWATCH_MAX_NAME_CHARS);
        // A short name passes through untouched.
        assert_eq!(truncate_name("kernel_task"), "kernel_task");
    }

    // --- CPU delta derivation (PURE, across two synthetic samples) -----------

    #[test]
    fn second_poll_derives_real_cpu_deltas_from_the_baseline() {
        // Poll 1: pid 1 has consumed 1s of CPU; pid 2's time was unreadable.
        let first = vec![
            smp(1, "worker", 100, Some(1_000_000_000), Some(10)),
            smp(2, "opaque", 100, None, Some(10)),
        ];
        let base = baseline(&first);
        // Poll 2, 10s later: pid 1 consumed 1 more second => 10%. pid 2 is
        // readable NOW but had no readable baseline => still None. pid 3 is
        // brand-new => no baseline => None.
        let second = vec![
            smp(1, "worker", 100, Some(2_000_000_000), Some(10)),
            smp(2, "opaque", 100, Some(500_000_000), Some(10)),
            smp(3, "fresh", 200, Some(9_000_000_000), Some(10)),
        ];
        let records = derive_records(second, Some(&base), 10_000_000_000);
        assert_eq!(records[0].cpu_pct, Some(10.0), "1s of CPU over a 10s gap = 10%");
        assert_eq!(records[1].cpu_pct, None, "no readable baseline => no honest delta");
        assert_eq!(records[2].cpu_pct, None, "a brand-new process has no delta yet");
        // A counter that (impossibly) went backwards clamps to a measured 0.
        let shrunk = derive_records(
            vec![smp(1, "worker", 100, Some(500_000_000), Some(10))],
            Some(&base),
            10_000_000_000,
        );
        assert_eq!(shrunk[0].cpu_pct, Some(0.0));
        // Zero elapsed time can't yield a rate.
        let zero = derive_records(
            vec![smp(1, "worker", 100, Some(2_000_000_000), Some(10))],
            Some(&base),
            0,
        );
        assert_eq!(zero[0].cpu_pct, None);
    }

    #[test]
    fn first_poll_every_cpu_is_null_and_top_cpu_is_honestly_empty() {
        // The first poll has NO baseline: cpu needs two samples, so every
        // cpu_pct must be null and the top-CPU list EMPTY — never a fabricated
        // 0.0% pid-ordered list. Memory is a point-in-time read and stays.
        let samples = vec![
            smp(1, "launchd", 0, Some(5_000_000_000), Some(400)),
            smp(2, "darwind", 10, Some(9_000_000_000), Some(900)),
        ];
        let records = derive_records(samples, None, 0);
        assert!(records.iter().all(|r| r.cpu_pct.is_none()));
        let v = snap(records).to_json(None, 8);
        assert_eq!(v["top_cpu"], json!([]), "no measured cpu yet => an EMPTY list");
        assert!(v["new_since_poll"].is_null());
        // top_mem IS honest on the first poll — with cpu_pct null per entry.
        let mem = v["top_mem"].as_array().unwrap();
        assert_eq!(mem.len(), 2);
        assert_eq!(mem[0]["pid"], json!(2));
        assert!(mem[0]["cpu_pct"].is_null(), "first-poll cpu is null, never 0.0");
    }

    // --- new-process counting (PURE, across two synthetic snapshots) ---------

    #[test]
    fn new_process_counting_across_two_snapshots_keys_on_pid_plus_start() {
        let first = vec![
            smp(100, "survivor", 1000, Some(1), Some(1)),
            smp(200, "dies", 1000, Some(1), Some(1)),
            smp(300, "reused-pid", 1000, Some(1), Some(1)),
        ];
        let base = baseline(&first);
        let second = vec![
            rec(100, "survivor", None, Some(1), 1000), // same pid + start -> not new
            rec(300, "reused-pid", None, Some(1), 2000), // SAME pid, new start -> NEW
            rec(400, "fresh", None, Some(1), 2000),    // new pid -> NEW
        ];
        assert_eq!(count_new(&second, &base), 2);
        // And the frame carries it as a number once a baseline exists.
        let v = snap(second).to_json(Some(&base), 8);
        assert_eq!(v["new_since_poll"], json!(2));
    }

    // --- to_json ASSEMBLE seam (PURE) ----------------------------------------

    #[test]
    fn total_is_the_kernels_pid_count_not_the_inspectable_subset() {
        // An unprivileged daemon can't inspect every pid (other-uid processes
        // refuse TBSDINFO): the kernel may list 640 pids while only 378 rows
        // are readable. `total` must carry the KERNEL'S count — reporting the
        // subset length as "total" would silently understate the machine.
        let s = ProcSnapshot {
            total: 640,
            procs: vec![rec(1, "visible", Some(1.0), Some(1), 0)],
            load_avg: None,
        };
        let v = s.to_json(None, 8);
        assert_eq!(v["total"], json!(640), "total = kernel pid count, not rows");
        assert_eq!(v["top_cpu"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn to_json_empty_table_is_honest_empty() {
        let v = snap(vec![]).to_json(None, 12);
        assert_eq!(v["total"], json!(0));
        assert_eq!(v["top_cpu"], json!([]));
        assert_eq!(v["top_mem"], json!([]));
        assert!(v["new_since_poll"].is_null());
        assert_eq!(v["load_avg"], json!([1.23, 0.5, 0.0]));
    }

    #[test]
    fn to_json_entry_carries_exactly_the_secret_free_keys() {
        // The entry object exposes EXACTLY the six secret-free keys (serde_json
        // sorts object keys, so compare as a set) — no argv/cmd/env/exe/cwd/
        // open-file key can exist because ProcRecord has no such field.
        let v = snap(vec![rec(42, "darwind", Some(12.34), Some(1024), 99)]).to_json(None, 4);
        let entry = &v["top_cpu"][0];
        let mut keys: Vec<&str> =
            entry.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["cpu_pct", "mem_bytes", "name", "pid", "ppid", "uid"]);
        assert_eq!(entry["name"], json!("darwind"));
        assert_eq!(entry["pid"], json!(42));
        assert_eq!(entry["ppid"], json!(1));
        assert_eq!(entry["uid"], json!(501));
        assert_eq!(entry["cpu_pct"], json!(12.3));
        assert_eq!(entry["mem_bytes"], json!(1024));
    }

    #[test]
    fn to_json_unreadable_fields_degrade_to_null_not_fabricated() {
        let mut p = rec(9, "orphanish", None, Some(1), 0);
        p.ppid = None;
        p.uid = None;
        let v = snap(vec![p]).to_json(None, 1);
        let entry = &v["top_mem"][0];
        assert!(entry["ppid"].is_null(), "unreadable ppid => null, never a fake 1");
        assert!(entry["uid"].is_null(), "unreadable uid => null, never a fake 501");
        assert!(entry["cpu_pct"].is_null(), "no delta yet => null, never a fake 0.0");
        // An unreadable load average is a null frame field, never [0, 0, 0].
        let s = ProcSnapshot { total: 0, procs: vec![], load_avg: None };
        assert!(s.to_json(None, 1)["load_avg"].is_null());
    }

    #[test]
    fn to_json_sanitizes_cpu_and_load() {
        let procs = vec![
            rec(1, "nan", Some(f32::NAN), Some(1), 0),
            rec(2, "neg", Some(-5.0), Some(1), 0),
            rec(3, "round", Some(10.04), Some(1), 0),
            rec(4, "multi-core", Some(340.0), Some(1), 0), // >100 is HONEST on multi-core
            rec(5, "garbage", Some(1e9), Some(1), 0),      // absurd reading hits the cap
        ];
        let v = snap(procs).to_json(None, 8);
        let by_pid = |pid: u64| -> f64 {
            v["top_mem"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["pid"] == json!(pid))
                .unwrap()["cpu_pct"]
                .as_f64()
                .unwrap()
        };
        assert_eq!(by_pid(1), 0.0, "NaN -> 0, never a fabricated load");
        assert_eq!(by_pid(2), 0.0, "negative clamps to 0");
        assert_eq!(by_pid(3), 10.0, "rounded to 1dp");
        assert_eq!(by_pid(4), 340.0, "multi-core >100% is honest and preserved");
        assert_eq!(by_pid(5), f64::from(PROCWATCH_CPU_PCT_CAP), "garbage hits the sanity cap");
        // Load components: rounded to 2dp, non-finite -> 0.
        let s = ProcSnapshot { total: 0, procs: vec![], load_avg: Some((f64::NAN, -1.0, 2.345)) };
        assert_eq!(s.to_json(None, 1)["load_avg"], json!([0.0, 0.0, 2.35]));
    }
}
