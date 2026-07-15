//! `pdfjail` — the MEMORY-JAILED PDF text-extraction helper subprocess.
//!
//! This is the complete, filter- and structure-AGNOSTIC fix for the PDF
//! decompression-bomb residuals that no in-process guard can close (see
//! `docsearch::pdf_decompression_within_budget`). `darwind` never decodes a PDF
//! in its own address space anymore: it spawns THIS short-lived child, pipes the
//! PDF bytes to its stdin, and reads the extracted text from its stdout under a
//! timeout. The child does exactly three things and then exits:
//!
//!   1. ARM A MEMORY JAIL — `setrlimit(RLIMIT_AS, ...)` caps this process's total
//!      address space BEFORE any PDF byte is decoded. Any decompression bomb —
//!      a single FlateDecode content stream, a filter-chain-armored bomb
//!      (ASCII85/LZW -> Flate), or a PARSE-TIME structural bomb inside an
//!      XRef/ObjStm that the parser must inflate just to read the file — makes an
//!      allocation in THIS child fail. Rust's allocator then ABORTS the process
//!      (an alloc failure does not unwind), so the bomb kills the CHILD and can
//!      never reach darwind. The parent sees a non-zero exit and honest-skips.
//!   2. EXTRACT — `pdf_extract::extract_text_from_mem` on the stdin bytes, inside
//!      `catch_unwind` so a parser panic becomes a clean non-zero exit too.
//!   3. EMIT — write the extracted UTF-8 text to stdout and `exit(0)`.
//!
//! Any error at any step => `exit(1)` (an honest skip at the parent). The result
//! channel (stdout) is kept PRISTINE by construction: during extraction FD 1 is
//! pointed at stderr so any stray diagnostic a PDF library might print cannot
//! corrupt the extracted text; the real text is written to a saved stdout FD.
//!
//! macOS/Apple-Silicon note (verified on-device): a process reserves a ~400+ GiB
//! VIRTUAL address space at startup (dyld shared cache + reserved regions), so
//! `setrlimit(RLIMIT_AS, 512 MiB)` is rejected with EINVAL ("can't set below
//! current usage"). The working form is `RLIMIT_AS = <current virtual size> +
//! budget`: a later allocation that would grow the address space past that budget
//! returns NULL and the allocator aborts. We read the baseline virtual size via
//! mach `task_info(MACH_TASK_BASIC_INFO)`. On other Unixes a small absolute
//! RLIMIT_AS is settable, so that path is used there.

use std::io::{Read, Write};

/// Extraction headroom above the process's baseline address space. A born-digital
/// PDF's real text/font streams inflate far below this; a bomb does not.
const AS_BUDGET_BYTES: u64 = 512 << 20; // 512 MiB

/// Never buffer a larger PDF than this from stdin. The parent only ever sends
/// files within the docsearch per-file byte cap, so this is a defensive ceiling.
const MAX_INPUT_BYTES: u64 = 512 << 20; // 512 MiB

/// Cap on the extracted text we emit (the parent caps again). Bounds what a
/// runaway extraction can push back through the pipe.
const MAX_OUTPUT_BYTES: usize = 64 << 20; // 64 MiB

fn main() {
    // (1) Arm the memory jail FIRST — before reading input or decoding — so every
    // allocation from here on is bounded. If arming fails we still run in a
    // SEPARATE process (a bomb can only exhaust THIS child; the OS memory-pressure
    // killer reaps the child, never the parent) and the parent enforces a timeout,
    // so we proceed rather than fail every PDF. The diagnostic goes to stderr,
    // which the parent discards; stdout stays clean.
    if let Err(e) = arm_address_space_limit(AS_BUDGET_BYTES) {
        eprintln!("pdfjail: could not arm RLIMIT_AS ({e}); relying on process isolation + parent timeout");
    }

    // (2) Read the whole PDF from stdin (bounded).
    let mut bytes = Vec::new();
    if std::io::stdin()
        .lock()
        .take(MAX_INPUT_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        std::process::exit(1);
    }

    // (3) Extract with the result channel isolated (see `extract_isolated`). A
    // bomb aborts the process here; a parser error/panic returns None -> exit 1.
    let text = match extract_isolated(&bytes) {
        Some(t) => t,
        None => std::process::exit(1),
    };

    // (4) Emit the extracted text (bounded) on the real stdout and exit 0.
    let out = text.as_bytes();
    let out = &out[..out.len().min(MAX_OUTPUT_BYTES)];
    if write_result(out).is_err() {
        std::process::exit(1);
    }
    std::process::exit(0);
}

/// Run `pdf_extract` with FD 1 temporarily pointed at stderr so any stray stdout
/// write from a PDF library during decoding cannot corrupt our result channel,
/// then restore stdout. Returns the extracted text, or `None` on an extractor
/// error OR a parser panic (both => the caller exits non-zero => parent skips).
/// The real text is written by [`write_result`] to the stdout saved here.
fn extract_isolated(bytes: &[u8]) -> Option<String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let _guard = StdoutShield::engage();
    match catch_unwind(AssertUnwindSafe(|| pdf_extract::extract_text_from_mem(bytes))) {
        Ok(Ok(text)) => Some(text),
        _ => None,
    }
}

/// Write the extracted text to the REAL stdout (the FD saved by [`StdoutShield`]
/// if the shield engaged, else the process stdout).
fn write_result(out: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;
        if let Some(fd) = StdoutShield::saved_fd() {
            // SAFETY: `fd` is a dup of the original stdout owned by the shield; we
            // take ownership here to write + close it exactly once.
            let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
            f.write_all(out)?;
            f.flush()?;
            return Ok(());
        }
    }
    let mut so = std::io::stdout().lock();
    so.write_all(out)?;
    so.flush()
}

/// Redirects FD 1 -> FD 2 (stderr) for the lifetime of the guard, saving the real
/// stdout FD so [`write_result`] can emit the clean text to it afterward. Unix
/// only; a no-op elsewhere (DARWIN is a macOS/Unix host).
struct StdoutShield;

#[cfg(unix)]
mod stdout_shield_state {
    use std::sync::atomic::{AtomicI32, Ordering};
    // The saved real-stdout FD, or -1 when the shield did not engage.
    static SAVED_FD: AtomicI32 = AtomicI32::new(-1);
    pub fn set(fd: i32) {
        SAVED_FD.store(fd, Ordering::SeqCst);
    }
    pub fn get() -> Option<i32> {
        match SAVED_FD.load(Ordering::SeqCst) {
            -1 => None,
            fd => Some(fd),
        }
    }
}

impl StdoutShield {
    #[cfg(unix)]
    fn engage() -> Self {
        // SAFETY: dup/dup2 on the standard FDs; failures fall back to leaving
        // stdout as-is (best effort — the result is still emitted, just without
        // the noise shield).
        unsafe {
            let saved = libc::dup(libc::STDOUT_FILENO);
            if saved >= 0 && libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) >= 0 {
                stdout_shield_state::set(saved);
            } else if saved >= 0 {
                libc::close(saved);
            }
        }
        StdoutShield
    }

    #[cfg(not(unix))]
    fn engage() -> Self {
        StdoutShield
    }

    #[cfg(unix)]
    fn saved_fd() -> Option<i32> {
        stdout_shield_state::get()
    }
}

impl Drop for StdoutShield {
    fn drop(&mut self) {
        // Nothing to restore: FD 1 stays pointed at stderr; the extracted text is
        // written to the saved FD by `write_result`, and the process exits next.
    }
}

// ---------------------------------------------------------------------------
// The memory jail: setrlimit(RLIMIT_AS)
// ---------------------------------------------------------------------------

/// macOS: `RLIMIT_AS = <current virtual size> + budget`. The absolute-small form
/// is unsettable here (the startup virtual size already dwarfs any sane budget),
/// so we read the baseline via mach `task_info` and add the budget on top.
#[cfg(target_os = "macos")]
fn arm_address_space_limit(budget: u64) -> std::io::Result<()> {
    let baseline = current_virtual_size()?;
    let cap = baseline.saturating_add(budget);
    // SAFETY: `rl` is a fully-initialized rlimit; setrlimit reads it by pointer.
    let rc = unsafe {
        let rl = libc::rlimit {
            rlim_cur: cap as libc::rlim_t,
            rlim_max: libc::RLIM_INFINITY,
        };
        libc::setrlimit(libc::RLIMIT_AS, &rl)
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The process's current virtual address-space size, via mach
/// `task_info(MACH_TASK_BASIC_INFO)`. Used to compute a settable RLIMIT_AS.
#[cfg(target_os = "macos")]
fn current_virtual_size() -> std::io::Result<u64> {
    use std::mem;
    // SAFETY: task_info with MACH_TASK_BASIC_INFO fills a mach_task_basic_info of
    // `count` natural_t words; we pass a zeroed struct of exactly that size.
    unsafe {
        let mut info: libc::mach_task_basic_info = mem::zeroed();
        let mut count = (mem::size_of::<libc::mach_task_basic_info>()
            / mem::size_of::<libc::natural_t>()) as libc::mach_msg_type_number_t;
        #[allow(deprecated)] // mach_task_self() is stable+correct; the mach2-crate
        // migration it suggests is not worth a new dependency for one call.
        let task = libc::mach_task_self();
        let kr = libc::task_info(
            task,
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as libc::task_info_t,
            &mut count,
        );
        if kr != libc::KERN_SUCCESS {
            return Err(std::io::Error::other(format!(
                "task_info(MACH_TASK_BASIC_INFO) failed: kern_return {kr}"
            )));
        }
        Ok(info.virtual_size as u64)
    }
}

/// Other Unixes: a small absolute RLIMIT_AS IS settable (no giant reserved
/// address space at startup), so cap directly at the budget.
#[cfg(all(unix, not(target_os = "macos")))]
fn arm_address_space_limit(budget: u64) -> std::io::Result<()> {
    // SAFETY: `rl` is a fully-initialized rlimit; setrlimit reads it by pointer.
    let rc = unsafe {
        let rl = libc::rlimit {
            rlim_cur: budget as libc::rlim_t,
            rlim_max: budget as libc::rlim_t,
        };
        libc::setrlimit(libc::RLIMIT_AS, &rl)
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Non-Unix: no RLIMIT_AS. DARWIN ships on macOS; this only keeps the helper
/// compiling elsewhere (it still provides process isolation + the parent timeout).
#[cfg(not(unix))]
fn arm_address_space_limit(_budget: u64) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "RLIMIT_AS is not available on this platform",
    ))
}
