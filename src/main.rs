//! Read the Claude Code status-line payload on stdin, print one line.
//!
//! See `lib.rs` for what the line means and how the port relates to the Python
//! original, and `pr.rs` for the one segment that is not read off stdin.

use std::io::{Read, Write};

use claude_statusline::pr;
use claude_statusline::{now_epoch, render, Render};

fn main() {
    // Refresh mode: not a user-facing interface, just how the status line's own
    // background lookup re-enters this binary. It prints nothing and touches
    // only the cache, so it is handled before stdin is read — there is none.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(pr::REFRESH_FLAG) {
        if let [_, slug, branch] = args.as_slice() {
            pr::refresh(slug, branch);
        }
        return;
    }

    let mut input = Vec::new();
    // A read error is not a decode error: the original would raise, so this
    // does the same rather than inventing a payload.
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        eprintln!("claude-statusline: could not read stdin: {e}");
        std::process::exit(1);
    }

    // File reads only — never the network. A stale or missing answer spawns a
    // refresh for the next render and yields None for this one.
    let found = pr::payload_dir(&input).and_then(|dir| pr::lookup(&dir));

    match render(&input, now_epoch(), found.as_ref()) {
        Render::Line(line) => {
            let mut out = std::io::stdout().lock();
            // A broken pipe is the status line's reader going away, which is
            // not worth a message.
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
        Render::Crash(msg) => {
            eprintln!("claude-statusline: {msg}");
            std::process::exit(1);
        }
    }
}
