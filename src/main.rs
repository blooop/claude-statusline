//! Read the Claude Code status-line payload on stdin, print one line.
//!
//! See `lib.rs` for what the line means and how the port relates to the Python
//! original.

use std::io::{Read, Write};

use claude_statusline::{now_epoch, render, Render};

fn main() {
    let mut input = Vec::new();
    // A read error is not a decode error: the original would raise, so this
    // does the same rather than inventing a payload.
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        eprintln!("claude-statusline: could not read stdin: {e}");
        std::process::exit(1);
    }

    match render(&input, now_epoch()) {
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
