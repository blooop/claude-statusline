//! Fixture loading and process plumbing shared by the golden and parity tests.
//!
//! Included by both test binaries, each of which uses a different half of it.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

pub struct Fixture {
    pub name: String,
    pub input: Vec<u8>,
    pub expected: Vec<u8>,
    /// The epoch second the expected output was recorded at, for fixtures
    /// whose output moves with the clock.
    pub now: Option<f64>,
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn fixtures() -> Vec<Fixture> {
    let mut out: Vec<Fixture> = std::fs::read_dir(fixtures_dir())
        .expect("tests/fixtures")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .map(|dir| {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            let now = std::fs::read_to_string(dir.join("now"))
                .ok()
                .map(|s| s.trim().parse().expect("now file holds an epoch second"));
            Fixture {
                input: std::fs::read(dir.join("input.json")).expect("input.json"),
                expected: std::fs::read(dir.join("expected.txt")).expect("expected.txt"),
                name,
                now,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub struct Output {
    pub stdout: Vec<u8>,
    pub status: ExitStatus,
}

fn feed(mut cmd: Command, input: &[u8]) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    Output {
        stdout: out.stdout,
        status: out.status,
    }
}

/// Run the built binary, optionally pinning its idea of "now".
pub fn run_binary(input: &[u8], now: Option<f64>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_claude-statusline"));
    match now {
        Some(t) => cmd.env("CLAUDE_STATUSLINE_NOW", format!("{t:?}")),
        None => cmd.env_remove("CLAUDE_STATUSLINE_NOW"),
    };
    feed(cmd, input)
}

/// Run the Python original.
pub fn run_python(script: &str, input: &[u8]) -> Output {
    let mut cmd = Command::new("python3");
    cmd.arg(script);
    feed(cmd, input)
}

pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

impl Fixture {
    /// The fixture's payload with every `resets_at` moved so it sits the same
    /// distance from `target` as it sat from the recorded `now`.
    ///
    /// Clock-independent fixtures (no `now` file) are returned untouched.
    pub fn shifted(&self, target: f64) -> Vec<u8> {
        let Some(recorded) = self.now else {
            return self.input.clone();
        };
        let delta = target - recorded;
        let mut v: serde_json::Value =
            serde_json::from_slice(&self.input).expect("a fixture with a `now` file is JSON");
        shift(&mut v, delta);
        serde_json::to_vec(&v).expect("serialize")
    }
}

fn shift(v: &mut serde_json::Value, delta: f64) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if k == "resets_at" {
                    if let Some(n) = val.as_f64() {
                        *val = serde_json::json!(n + delta);
                        continue;
                    }
                }
                shift(val, delta);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|i| shift(i, delta)),
        _ => {}
    }
}
