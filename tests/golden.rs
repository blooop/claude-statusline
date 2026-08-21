//! Golden tests: every fixture's stdin, run through the binary, must produce
//! the bytes in its `expected.txt`.
//!
//! Those bytes were not written by hand — each one is the stdout of the Python
//! `status-line.py` this crate is a port of, captured by feeding it the very
//! `input.json` beside it. So the file is a recording of the original's
//! behaviour, and a diff here is the port drifting from it.
//!
//! A fixture whose output depends on the clock (any `resets_at` in the future)
//! carries a `now` file: the epoch second the recording was made at, replayed
//! into the binary through `CLAUDE_STATUSLINE_NOW`. Fixtures without one are
//! clock-independent by construction — no `resets_at`, an unparseable one, or
//! one already in the past, which pins `remaining` at zero forever.
//!
//! `tests/parity.rs` is the other half: it re-runs the real Python script live.

mod support;

use support::{fixtures, run_binary};

#[test]
fn every_fixture_matches_the_recorded_python_output() {
    let mut failures = Vec::new();
    let cases = fixtures();
    assert!(cases.len() > 10, "fixtures went missing");

    for case in &cases {
        let out = run_binary(&case.input, case.now);
        if out.stdout != case.expected {
            failures.push(format!(
                "{}\n  expected: {:?}\n  actual:   {:?}",
                case.name,
                String::from_utf8_lossy(&case.expected),
                String::from_utf8_lossy(&out.stdout),
            ));
        }
        if !out.status.success() {
            failures.push(format!("{}: exited {}", case.name, out.status));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} fixtures diverged:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn a_payload_that_is_not_an_object_prints_nothing_and_fails() {
    // Python reaches `data.get(...)` on a list and raises AttributeError:
    // empty stdout, exit 1. The port keeps both.
    for raw in [&b"[]"[..], b"null", b"5", b"\"x\""] {
        let out = run_binary(raw, None);
        assert!(out.stdout.is_empty(), "{raw:?} printed {:?}", out.stdout);
        assert_eq!(out.status.code(), Some(1), "{raw:?}");
    }
}

#[test]
fn nothing_on_stdin_is_the_hourglass() {
    let out = run_binary(b"", None);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "\u{23f3}\n");
    assert!(out.status.success());
}
