//! Live parity against the Python original.
//!
//! `tests/golden.rs` checks the port against a *recording* of the script. This
//! one runs the script itself, so it catches the case the recording cannot: the
//! script changing under the port. It is skipped unless `STATUS_LINE_PY` points
//! at a copy of `status-line.py`, because most machines running `cargo test` do
//! not have one:
//!
//! ```text
//! STATUS_LINE_PY=~/.claude/scripts/status-line.py cargo test --test parity
//! ```
//!
//! Both sides read the real clock here — no `CLAUDE_STATUSLINE_NOW` — so a
//! clock-sensitive fixture has its `resets_at` shifted to sit the same distance
//! from *this* run's "now" as it sat from the recording's. Two processes cannot
//! start at the same instant, so each fixture is bracketed: Python, then the
//! binary, then Python again. If the two Python runs disagree the comparison
//! straddled a rounding boundary (a minute ticking over in `fmt_eta`, say) and
//! the fixture is retried rather than failed.

mod support;

use support::{fixtures, run_binary, run_python, unix_now};

const ATTEMPTS: usize = 4;

#[test]
fn the_port_agrees_with_the_python_script_it_was_ported_from() {
    let Ok(script) = std::env::var("STATUS_LINE_PY") else {
        eprintln!("STATUS_LINE_PY is unset — skipping live parity against the Python original");
        return;
    };
    let script = shellexpand_home(&script);
    assert!(
        std::path::Path::new(&script).exists(),
        "STATUS_LINE_PY={script} does not exist"
    );

    let mut failures = Vec::new();
    let cases = fixtures();
    for case in &cases {
        let mut unstable = 0;
        for attempt in 1..=ATTEMPTS {
            let payload = case.shifted(unix_now());
            let before = run_python(&script, &payload);
            let ours = run_binary(&payload, None);
            let after = run_python(&script, &payload);

            if before.stdout != after.stdout {
                // A boundary ticked over between the two Python runs; nothing
                // the binary printed could have matched both.
                unstable = attempt;
                continue;
            }
            if ours.stdout != before.stdout || ours.status.code() != before.status.code() {
                failures.push(format!(
                    "{}\n  python: {:?} (exit {:?})\n  rust:   {:?} (exit {:?})",
                    case.name,
                    String::from_utf8_lossy(&before.stdout),
                    before.status.code(),
                    String::from_utf8_lossy(&ours.stdout),
                    ours.status.code(),
                ));
            }
            unstable = 0;
            break;
        }
        if unstable != 0 {
            failures.push(format!(
                "{}: python disagreed with itself on all {ATTEMPTS} attempts",
                case.name
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} fixtures diverged from {script}:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

fn shellexpand_home(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => p.to_string(),
        },
        None => p.to_string(),
    }
}
