// Shared test-support module: each integration-test binary that does
// `mod support;` pulls in the whole module but uses only a subset of these
// helpers, so unused items here are expected per-binary.
#![allow(dead_code)]

use std::process::Command;

/// Counts of PDF content-stream operators after a qpdf `--qdf` expansion.
/// Only tracks the operators we care about in border/text optimization work.
#[derive(Debug, Default, Clone)]
pub struct OpCounts {
    pub m: usize,
    pub l: usize,
    pub c: usize,
    pub re: usize,
    pub s_stroke: usize,
    pub q: usize,
    pub bt: usize,
    pub rg_stroke: usize,
}

/// Run `qpdf --qdf --object-streams=disable` on `pdf_bytes` and count
/// PDF operators. Returns `None` only when qpdf is not installed (tests
/// should skip — CI always has it, local devs may not). Any other
/// failure panics so that bugs don't silently appear as skipped tests.
pub fn count_ops(pdf_bytes: &[u8]) -> Option<OpCounts> {
    // Probe: qpdf binary present? If not, return None (skip). If present,
    // any subsequent failure is a real bug and should panic rather than
    // silently skip, so tests don't pretend to pass.
    match Command::new("qpdf").arg("--version").status() {
        Ok(status) if status.success() => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Ok(status) => panic!("qpdf --version failed: {:?}", status),
        Err(err) => panic!("failed to execute qpdf --version: {err}"),
    }

    // tempdir + plain paths: NamedTempFile keeps an open handle, which on
    // Windows can block qpdf from writing to the output path.
    let dir = tempfile::tempdir().expect("create tempdir");
    let tmp = dir.path().join("input.pdf");
    let out = dir.path().join("output.qdf.pdf");
    std::fs::write(&tmp, pdf_bytes).expect("write tmp pdf");

    let status = Command::new("qpdf")
        .args(["--qdf", "--object-streams=disable"])
        .arg(&tmp)
        .arg(&out)
        .status()
        .expect("spawn qpdf");
    assert!(status.success(), "qpdf --qdf failed: {:?}", status);

    // `qpdf --qdf` does NOT strip binary streams (embedded fonts, inline
    // images, etc.), so the output is not valid UTF-8. Scan bytes
    // directly — PDF operators we care about are ASCII-only and sit at
    // the end of a line, so suffix matching on byte slices works.
    let qdf = std::fs::read(&out).expect("read qdf output");
    let mut c = OpCounts::default();
    for raw in qdf.split(|&b| b == b'\n') {
        // Strip trailing \r on CRLF lines.
        let line: &[u8] = if raw.last() == Some(&b'\r') {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        if line.ends_with(b" m") || line == b"m" {
            c.m += 1;
        } else if line.ends_with(b" l") || line == b"l" {
            c.l += 1;
        } else if line.ends_with(b" c") {
            c.c += 1;
        } else if line.ends_with(b" re") {
            c.re += 1;
        } else if line == b"S" || line.ends_with(b" S") {
            c.s_stroke += 1;
        } else if line == b"q" {
            c.q += 1;
        } else if line == b"BT" {
            c.bt += 1;
        } else if line.ends_with(b" RG") {
            c.rg_stroke += 1;
        }
    }
    Some(c)
}

/// Extract the `f` (vertical translate) operand of every text matrix
/// (`a b c d e f Tm`) in `pdf_bytes`, in document order. Krilla emits one
/// `Tm` per text run, so each returned value is a text run's baseline y in
/// PDF user space. Returns `None` only when qpdf is not installed (skip);
/// any other failure panics so bugs don't masquerade as skips.
///
/// Used to assert vertical placement (e.g. that an end-side margin actually
/// offsets a `bottom:0` absolute element) without rasterizing.
pub fn text_matrix_ys(pdf_bytes: &[u8]) -> Option<Vec<f32>> {
    match Command::new("qpdf").arg("--version").status() {
        Ok(status) if status.success() => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Ok(status) => panic!("qpdf --version failed: {:?}", status),
        Err(err) => panic!("failed to execute qpdf --version: {err}"),
    }

    let dir = tempfile::tempdir().expect("create tempdir");
    let tmp = dir.path().join("input.pdf");
    let out = dir.path().join("output.qdf.pdf");
    std::fs::write(&tmp, pdf_bytes).expect("write tmp pdf");

    let status = Command::new("qpdf")
        .args(["--qdf", "--object-streams=disable"])
        .arg(&tmp)
        .arg(&out)
        .status()
        .expect("spawn qpdf");
    assert!(status.success(), "qpdf --qdf failed: {:?}", status);

    let qdf = std::fs::read(&out).expect("read qdf output");
    let mut ys = Vec::new();
    for raw in qdf.split(|&b| b == b'\n') {
        let line: &[u8] = if raw.last() == Some(&b'\r') {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        if line.ends_with(b" Tm") {
            // `a b c d e f Tm` — the f operand is the 6th number.
            let text = String::from_utf8_lossy(line);
            let nums: Vec<f32> = text
                .split_whitespace()
                .filter_map(|t| t.parse::<f32>().ok())
                .collect();
            if nums.len() == 6 {
                ys.push(nums[5]);
            }
        }
    }
    Some(ys)
}
