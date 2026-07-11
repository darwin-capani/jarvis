//! Integration tests for the `pdfjail` MEMORY-JAIL helper (src/bin/pdfjail.rs).
//!
//! These drive the REAL helper binary (`CARGO_BIN_EXE_pdfjail`) as a subprocess,
//! exactly as `docsearch::pdf_text_jailed` does in the daemon: pipe PDF bytes to
//! its stdin, read extracted text from its stdout, observe its exit status.
//!
//! `jail_aborts_the_child_on_a_decompression_bomb` is `#[ignore]` by default: it
//! is the ON-DEVICE proof that a bomb aborts the CHILD (not the daemon), and it
//! genuinely tries to make the child allocate multiple GiB, so it is not run in a
//! normal `cargo test` sweep. Run it explicitly on-device:
//!
//!   cargo test --test pdf_memory_jail -- --ignored --nocapture
//!
//! The happy-path test IS hermetic and runs normally: it proves the subprocess
//! extraction protocol (stdin bytes -> stdout text, exit 0) end-to-end.

use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Drive the pdfjail helper: feed `input` on stdin, read stdout (bounded), and
/// return `(exit status or None-if-killed, captured stdout)`. Writer + reader run
/// on their own threads so a full pipe can never deadlock; a watchdog kills a
/// child that outlives `timeout`.
fn run_pdfjail(input: Vec<u8>, timeout: Duration) -> (Option<ExitStatus>, Vec<u8>) {
    let bin = env!("CARGO_BIN_EXE_pdfjail");
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pdfjail");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        // drop stdin -> EOF
    });
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        // Read at most ~128 MiB so a runaway child can't make the test OOM.
        let _ = stdout.by_ref().take(128 << 20).read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    (status, out)
}

/// A minimal BORN-DIGITAL PDF (one page, one uncompressed text-show operator) — a
/// port of docsearch's `make_pdf` so this crate owns its fixture bytes.
fn make_pdf(body: &str) -> Vec<u8> {
    let objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_string(),
        {
            let stream = format!("BT /F1 24 Tf 72 700 Td ({body}) Tj ET");
            format!("<< /Length {} >>\nstream\n{}\nendstream", stream.len(), stream)
        },
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    assemble_pdf(objs.iter().map(|s| s.as_bytes().to_vec()).collect())
}

/// A DECOMPRESSION-BOMB PDF: the page's `/Contents` is a FlateDecode stream whose
/// tiny compressed body inflates to ~`decompressed_bytes`. The compressed blob is
/// built by streaming zeros through the encoder, so the FIXTURE itself never holds
/// the huge buffer — only the child does, when it decodes (and aborts under the
/// jail). `/Length` is the exact compressed size, so there is no endstream-in-data
/// ambiguity: the parser reads the stream by length, then decodes it.
fn make_flate_bomb_pdf(decompressed_bytes: usize) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::best());
    let zeros = vec![0u8; 1 << 20]; // 1 MiB of zeros, written repeatedly
    let mut left = decompressed_bytes;
    while left > 0 {
        let n = left.min(zeros.len());
        enc.write_all(&zeros[..n]).unwrap();
        left -= n;
    }
    let compressed = enc.finish().unwrap();

    // Object 4 = the FlateDecode content stream (built as raw bytes so the binary
    // compressed body is preserved verbatim).
    let mut obj4 = Vec::new();
    obj4.extend_from_slice(
        format!("<< /Length {} /Filter /FlateDecode >>\nstream\n", compressed.len()).as_bytes(),
    );
    obj4.extend_from_slice(&compressed);
    obj4.extend_from_slice(b"\nendstream");

    let objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        obj4,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    assemble_pdf(objs)
}

/// Assemble a classic (non-xref-stream) PDF from object bodies, writing the header,
/// each `N 0 obj ... endobj`, the xref table with real byte offsets, and the trailer.
fn assemble_pdf(objs: Vec<Vec<u8>>) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let mut s = String::new();
    write!(s, "xref\n0 {}\n", objs.len() + 1).unwrap();
    s.push_str("0000000000 65535 f \n");
    for off in &offsets {
        writeln!(s, "{off:010} 00000 n ").unwrap();
    }
    write!(
        s,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
        objs.len() + 1,
        xref_pos
    )
    .unwrap();
    out.extend_from_slice(s.as_bytes());
    out
}

/// HERMETIC: a valid born-digital PDF extracts through the REAL subprocess — the
/// stdin -> stdout -> exit-0 protocol the daemon relies on.
#[test]
fn jail_extracts_a_valid_pdf_over_the_subprocess() {
    let pdf = make_pdf("Quarterly budget Subaru Outback");
    let (status, out) = run_pdfjail(pdf, Duration::from_secs(30));
    let status = status.expect("pdfjail must not be killed on a valid PDF");
    assert!(status.success(), "valid PDF must exit 0, got {status:?}");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("Quarterly"), "extracted text via subprocess: {text:?}");
    assert!(text.contains("Subaru"), "extracted text via subprocess: {text:?}");
}

/// HERMETIC: garbage bytes with a PDF header make the extractor error, so the
/// child exits NON-ZERO and emits no text — the honest-skip signal the parent uses.
#[test]
fn jail_exits_nonzero_on_a_corrupt_pdf() {
    let junk = b"%PDF-1.4 this is absolutely not a valid pdf stream".to_vec();
    let (status, out) = run_pdfjail(junk, Duration::from_secs(30));
    let status = status.expect("a corrupt PDF should finish fast, not hang");
    assert!(!status.success(), "a corrupt PDF must exit non-zero (honest skip)");
    assert!(out.is_empty(), "a skipped PDF must emit no text, got {} bytes", out.len());
}

/// ON-DEVICE (`#[ignore]`): a real ~2 GiB FlateDecode content bomb makes the CHILD
/// allocate past its RLIMIT_AS budget, so the child ABORTS (non-zero / killed exit)
/// and produces no usable text — and jarvisd, in the real path, is never touched.
/// This is the residual-closing proof that cannot run hermetically (it needs a real
/// subprocess bomb and multiple GiB of transient child memory).
#[test]
#[ignore = "on-device: allocates multi-GiB in the child to trip RLIMIT_AS"]
fn jail_aborts_the_child_on_a_decompression_bomb() {
    // Inflates to ~2 GiB — far past the child's <baseline>+512 MiB address-space
    // budget — from a ~2 MiB compressed body.
    let bomb = make_flate_bomb_pdf(2 * 1024 * 1024 * 1024);
    assert!(bomb.len() < 32 * 1024 * 1024, "the bomb fixture stays small: {} bytes", bomb.len());

    let (status, out) = run_pdfjail(bomb, Duration::from_secs(60));
    match status {
        Some(s) => assert!(
            !s.success(),
            "the bomb must abort the child (non-zero exit), got success: {s:?}"
        ),
        None => { /* killed by our watchdog — also an acceptable non-success outcome */ }
    }
    // The child never emitted gigabytes of text (it died decoding), so any output
    // is small — certainly not the ~2 GiB the bomb would inflate to.
    assert!(
        out.len() < 64 * 1024 * 1024,
        "a jailed bomb must not stream back a huge result, got {} bytes",
        out.len()
    );
}
