//! Claude Code status line — a faithful Rust port of `status-line.py`.
//!
//! The design commentary lives in the Python original and is not repeated here;
//! what follows are notes on the *port*, i.e. the places where "do what Python
//! does" needed a decision.
//!
//! Rate-limit usage is shown as a pacing gauge per window (5h / 7d): a
//! `used/even` pair in percent, where `even` is the usage you would have at a
//! perfectly steady burn. One colour per window block, tracking the projected
//! finish (`used/even × 100`), so warmer is worse. A window reads
//! `🔥20%/87% ⏳42m/5h`.
//!
//! ## Port notes
//!
//! * **Float formatting.** Rust's `{:.N}` and Python's `{:.Nf}` both round the
//!   exact binary value to nearest, ties to even, so `f"{x:.0f}"` and
//!   `format!("{x:.0}")` agree byte for byte — including the cases where that
//!   surprises (`0.5 → "0"`, `1.5 → "2"`, `2.675 → "2.67"`). Nothing here
//!   re-implements rounding.
//! * **Time.** No date crate: the script never prints a date, only differences
//!   of epoch seconds, so `civil` does the calendar arithmetic `parse_reset`
//!   needs and everything downstream is `f64` seconds. `now` is injectable
//!   (see [`now_epoch`]) purely so the golden tests can pin it.
//! * **Malformed input.** A JSON parse failure prints `⏳`, as in Python. A
//!   top-level value that is not an object is [`Render::Crash`]: Python raises
//!   `AttributeError` there, printing nothing and exiting 1, and the port
//!   matches that on stdout and exit code (the stderr text differs — it is not
//!   a Python traceback). Wrong-typed values *inside* the object are the one
//!   deliberate divergence: Python raises for e.g. a string
//!   `used_percentage`, while this port treats a value of the wrong type as
//!   absent. Nothing produces such a payload, and a status line that renders
//!   beats one that leaves an exception in the prompt.

use serde_json::Value;

pub mod civil;
pub mod pr;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// A 256-colour SGR escape, the `_c(n)` of the original.
macro_rules! c {
    ($n:literal) => {
        concat!("\x1b[38;5;", $n, "m")
    };
}

/// absent data only: `--` placeholders
const GREY: &str = c!(245);
/// structural dividers only — seen, not read
const SEP: &str = c!(243);
/// aquamarine — session spend
const COST: &str = c!(79);
/// steel blue — session wall-clock
const DUR: &str = c!(68);

/// One hue and one glyph per pull-request state. The glyph is doing the work
/// the hue only reinforces: `●` and `◆` are as different to a monochrome
/// terminal, or to a red-green eye, as green and purple are to everything else.
/// Draft is the only hollow one, because it is the only state that is not yet a
/// thing that exists.
const PR_OPEN: (&str, &str) = (c!(41), "●");
const PR_DRAFT: (&str, &str) = (c!(102), "○");
const PR_MERGED: (&str, &str) = (c!(99), "◆");
const PR_CLOSED: (&str, &str) = (c!(160), "✕");
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// One hue per model family, matched as a substring of the lowercased display
/// name. The order is the item-rarity ladder — uncommon → rare → epic →
/// legendary — and it is load-bearing: the first match wins, exactly as
/// Python's insertion-ordered dict does.
const MODEL_COLORS: [(&str, &str); 4] = [
    ("haiku", c!(80)),  // cyan — uncommon
    ("sonnet", c!(75)), // sky blue — rare
    ("fable", c!(141)), // violet — epic
    ("opus", c!(208)),  // orange — legendary
];
/// unknown family — unranked, but still not grey
const MODEL_FALLBACK: &str = c!(111);

/// Green → yellow → red gradient for the rate-limit gauges, walked by
/// [`proj_color`].
const RAMP: [&str; 11] = [
    c!(46),
    c!(82),
    c!(118),
    c!(154),
    c!(190),
    c!(226),
    c!(220),
    c!(214),
    c!(208),
    c!(202),
    c!(196),
];

/// (rate_limits key, short label, window length in seconds)
const WINDOWS: [(&str, &str, f64); 2] = [
    ("five_hour", "5h", 5.0 * 3600.0),
    ("seven_day", "7d", 7.0 * 86400.0),
];

/// Projected end-of-window usage, in percent, that the ramp spans.
const PROJ_GREEN: f64 = 100.0;
const PROJ_RED: f64 = 200.0;
/// Elapsed fraction the projection divides by, floored.
const PROJ_FLOOR: f64 = 0.15;

/// sand still running = window still open
const TIME_GLYPH: &str = "⏳";
const FIRE: &str = "🔥";
const TORTOISE: &str = "🐢";
/// Percentage points ahead of pace before it counts as burning hot.
const PACE_DEADBAND: f64 = 1.0;

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn format_duration(ms: Option<f64>) -> String {
    // Python's `if not ms` — absent, null and zero are all "no duration".
    let ms = match ms {
        Some(v) if v != 0.0 => v,
        _ => return String::new(),
    };
    let s = (ms / 1000.0) as i64; // int() truncates toward zero
    if s < 60 {
        return format!("{s}s");
    }
    if s < 3600 {
        return format!("{}m{:02}s", s / 60, s % 60);
    }
    let (h, rem) = (s / 3600, s % 3600);
    format!("{}h{:02}m", h, rem / 60)
}

fn format_cost(usd: Option<f64>) -> String {
    match usd {
        None => String::new(),
        Some(v) if v < 0.01 => format!("${v:.4}"),
        Some(v) => format!("${v:.2}"),
    }
}

/// `f"{x:.1f}".rstrip("0").rstrip(".")`
fn trim_trailing_zeros(s: &str) -> &str {
    s.trim_end_matches('0').trim_end_matches('.')
}

fn fmt_tokens(n: f64) -> String {
    let n = n as i64; // int()
    if n < 1000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let k = n as f64 / 1000.0;
        let s = if k < 100.0 {
            trim_trailing_zeros(&format!("{k:.1}")).to_string()
        } else {
            format!("{k:.0}")
        };
        return format!("{s}k");
    }
    let m = n as f64 / 1_000_000.0;
    format!("{}M", trim_trailing_zeros(&format!("{m:.1}")))
}

/// Pick from `RAMP` by position along it, 0.0 → green, 1.0 → red.
fn ramp(frac: f64) -> &'static str {
    let frac = frac.clamp(0.0, 1.0);
    RAMP[((frac * RAMP.len() as f64) as usize).min(RAMP.len() - 1)]
}

/// Colour by how much of a budget is spent — "how much is in the tank?".
fn usage_color(usage_pct: Option<f64>) -> &'static str {
    match usage_pct {
        None => GREY,
        Some(p) => ramp(p / 100.0),
    }
}

/// Where this window lands at reset if the current rate holds.
fn projected_pct(usage_pct: f64, elapsed_pct: f64) -> f64 {
    usage_pct / (elapsed_pct / 100.0).max(PROJ_FLOOR)
}

/// Colour by projected end-of-window usage — "will I run dry before reset?".
fn proj_color(usage_pct: f64, elapsed_pct: f64) -> &'static str {
    ramp((projected_pct(usage_pct, elapsed_pct) - PROJ_GREEN) / (PROJ_RED - PROJ_GREEN))
}

/// Burning-hot verdict on the pacing margin — see `PACE_DEADBAND`.
fn pace_glyph(usage_pct: f64, elapsed_pct: f64) -> &'static str {
    if usage_pct - elapsed_pct > PACE_DEADBAND {
        FIRE
    } else {
        TORTOISE
    }
}

/// Rarity-ladder hue for the model family named in the display name.
fn model_color(display_name: &str) -> &'static str {
    let low = display_name.to_lowercase();
    for (family, color) in MODEL_COLORS {
        if low.contains(family) {
            return color;
        }
    }
    MODEL_FALLBACK
}

/// Percentage to its nearest whole number, signed: 9.6 → 10%.
fn fmt_pct(x: f64) -> String {
    format!("{x:.0}%")
}

fn fmt_eta(seconds: f64) -> String {
    let s = seconds.max(0.0) as i64;
    let (d, rem) = (s / 86400, s % 86400);
    let (h, rem) = (rem / 3600, rem % 3600);
    let m = rem / 60;
    if d != 0 {
        format!("{d}d{h}h")
    } else if h != 0 {
        format!("{h}h{m:02}m")
    } else if m != 0 {
        format!("{m}m")
    } else {
        "<1m".to_string()
    }
}

/// `str(x)` for a JSON number, the way Python would print it.
///
/// Only reached by the `+added/-removed` field, which prints the value
/// verbatim. Integers are integers; a float keeps Python's trailing `.0`,
/// which Rust's `{}` drops.
fn py_num_str(v: &Value) -> String {
    if let Some(i) = v.as_i64() {
        return i.to_string();
    }
    if let Some(u) = v.as_u64() {
        return u.to_string();
    }
    match v.as_f64() {
        None => "0".to_string(),
        Some(f) if f.is_nan() => "nan".to_string(),
        Some(f) if f.is_infinite() => {
            if f > 0.0 {
                "inf".to_string()
            } else {
                "-inf".to_string()
            }
        }
        Some(f) => {
            let s = format!("{f}");
            if s.contains(['.', 'e', 'E']) {
                s
            } else {
                format!("{s}.0")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// resets_at parsing
// ---------------------------------------------------------------------------

/// Parse a `resets_at` value (epoch seconds/ms or ISO-8601) to epoch seconds.
fn parse_reset(v: Option<&Value>) -> Option<f64> {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(_)) => None,
        Some(Value::Number(_)) => from_timestamp(v.and_then(Value::as_f64)?),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // Python tries `float(s)` first, so a bare numeric string is an
            // epoch and never a date.
            if let Ok(f) = s.parse::<f64>() {
                return from_timestamp(f);
            }
            parse_isoformat(&s.replace('Z', "+00:00"))
        }
        _ => None,
    }
}

/// `datetime.fromtimestamp(ts, tz=utc)` with the ms heuristic in front of it.
fn from_timestamp(v: f64) -> Option<f64> {
    let ts = if v > 1e12 { v / 1000.0 } else { v };
    // fromtimestamp raises OverflowError/OSError/ValueError outside the range
    // `datetime` can represent; the original catches all three and yields None.
    if ts.is_nan() || !(civil::DT_MIN..=civil::DT_MAX).contains(&ts) {
        return None;
    }
    Some(ts)
}

/// The subset of `datetime.fromisoformat` that a `resets_at` can plausibly be.
///
/// Extended (`2026-08-21T14:03:00+01:00`) and basic (`20260821T140300Z`)
/// calendar forms, an optional fractional second, an optional offset in
/// `±HH`, `±HH:MM` or `±HHMM`. Anything else is a `ValueError`, i.e. `None`.
fn parse_isoformat(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if !b.iter().all(|c| c.is_ascii()) {
        return None;
    }
    // Split the offset off the tail. It can only start after the date, so the
    // search begins past the longest date form.
    let mut tz: Option<i64> = None;
    let mut body = s;
    if let Some(pos) = s
        .char_indices()
        .skip(10)
        .find(|(_, c)| *c == '+' || *c == '-')
        .map(|(i, _)| i)
    {
        let (head, off) = s.split_at(pos);
        tz = Some(parse_offset(off)?);
        body = head;
    }

    let (date, time) = match body.len() {
        // date only
        8 | 10 => (body, ""),
        _ if body.len() > 10 => {
            // Any single character separates date from time, as CPython allows.
            let split = if body.as_bytes()[4] == b'-' { 10 } else { 8 };
            if body.len() <= split {
                return None;
            }
            (&body[..split], &body[split + 1..])
        }
        _ => return None,
    };

    let (year, month, day) = parse_date(date)?;
    let (hour, minute, second, micro) = if time.is_empty() {
        (0, 0, 0, 0)
    } else {
        parse_time(time)?
    };

    civil::Civil {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micro,
        offset: tz,
    }
    .to_epoch()
}

fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn parse_date(s: &str) -> Option<(i64, i64, i64)> {
    match s.len() {
        10 if s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' => {
            Some((digits(&s[..4])?, digits(&s[5..7])?, digits(&s[8..10])?))
        }
        8 => Some((digits(&s[..4])?, digits(&s[4..6])?, digits(&s[6..8])?)),
        _ => None,
    }
}

fn parse_time(s: &str) -> Option<(i64, i64, i64, i64)> {
    // Fractional second first: `.` or `,`, then 1..6 digits (CPython 3.11+
    // also tolerates 7-9 and truncates; do the same).
    let (main, frac) = match s.find(['.', ',']) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let micro = if frac.is_empty() {
        0
    } else {
        if frac.len() > 9 || !frac.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let padded = format!("{frac:0<6}");
        digits(&padded[..6])?
    };

    let extended = main.contains(':');
    let (h, m, sec) = if extended {
        let mut it = main.split(':');
        let h = digits(it.next()?)?;
        let m = match it.next() {
            Some(p) => digits(p)?,
            None => 0,
        };
        let sec = match it.next() {
            Some(p) => digits(p)?,
            None => 0,
        };
        if it.next().is_some() {
            return None;
        }
        (h, m, sec)
    } else {
        match main.len() {
            2 => (digits(main)?, 0, 0),
            4 => (digits(&main[..2])?, digits(&main[2..4])?, 0),
            6 => (
                digits(&main[..2])?,
                digits(&main[2..4])?,
                digits(&main[4..6])?,
            ),
            _ => return None,
        }
    };
    Some((h, m, sec, micro))
}

fn parse_offset(s: &str) -> Option<i64> {
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &s[1..];
    // Offsets carry seconds and microseconds in CPython; a `resets_at` will
    // not, so `±HH`, `±HH:MM` and `±HHMM` are the accepted forms.
    let (h, m) = match rest.len() {
        2 => (digits(rest)?, 0),
        4 => (digits(&rest[..2])?, digits(&rest[2..4])?),
        5 if rest.as_bytes()[2] == b':' => (digits(&rest[..2])?, digits(&rest[3..5])?),
        _ => return None,
    };
    if h > 23 || m > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64())
}

/// `d.get(key)` where a non-object `d` behaves like an empty one.
fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v.get(key) {
        Some(Value::Null) | None => None,
        other => other,
    }
}

fn context_usage(ctx: &Value) -> String {
    let used = num(get(ctx, "total_input_tokens")).unwrap_or(0.0);
    let size = num(get(ctx, "context_window_size")).unwrap_or(0.0);
    if size == 0.0 {
        return format!("{GREY}--/--{RESET}");
    }
    // context fills up long before it is "spent" the way a rate limit is —
    // compaction looms around 60-70% — so the ramp is walked at 1.6× to reach
    // red near 60% rather than 100%
    let p = (used / size * 100.0).clamp(0.0, 100.0);
    format!(
        "{}{}/{}{}",
        usage_color(Some(p * 1.6)),
        fmt_tokens(used),
        fmt_tokens(size),
        RESET
    )
}

fn window_part(label: &str, window_len: f64, sub: Option<&Value>, now: f64) -> String {
    let usage = sub
        .filter(|s| s.is_object())
        .and_then(|s| num(get(s, "used_percentage")));
    let Some(usage) = usage else {
        // no data yet (e.g. before the first message) — show the slot anyway
        return format!("{GREY}{TIME_GLYPH}{label} --{RESET}");
    };
    // floored, not clamped above: a window reported past 100 should read past
    // 100 rather than be quietly capped into looking merely spent
    let used = usage.max(0.0);
    let reset = parse_reset(sub.and_then(|s| get(s, "resets_at")));

    let Some(reset) = reset else {
        // no reset clock — nothing to count down, and no "even" to pace
        // against, so the block falls back to absolute usage for its colour.
        return format!(
            "{}{} {TIME_GLYPH}{label}{RESET}",
            usage_color(Some(used)),
            fmt_pct(used)
        );
    };
    let remaining = (reset - now).max(0.0);
    let even = ((window_len - remaining) / window_len * 100.0).clamp(0.0, 100.0);
    // one colour for the whole block, on the projected finish — including the
    // separators, the % signs and the clock, so nothing in the field fragments
    // into a second signal
    format!(
        "{}{}{}/{} {TIME_GLYPH}{}/{label}{RESET}",
        proj_color(used, even),
        pace_glyph(used, even),
        fmt_pct(used),
        fmt_pct(even),
        fmt_eta(remaining)
    )
}

/// The PR segment: the full URL, glyphed and hued by state, and also an OSC 8
/// hyperlink pointing at itself.
///
/// The URL is printed in full rather than as a tidy `#11573` because the two
/// ways a terminal opens a link do not look at the same thing. OSC 8 —
/// `ESC ] 8 ;; <url> ESC \\`, the text, then the same with an empty url to
/// close it — hands the target to the terminal out of band, which is what a
/// ctrl-click follows. Keyboard URL pickers do not read it: kitty's hints
/// kitten (`ctrl+shift+e`), and every tmux and Vim equivalent, scan the visible
/// *text* for something matching a URL. A `#11573` offers them nothing to
/// match, so the link was reachable only by mouse.
///
/// Printing the URL serves both, which is why the segment moved to the end of
/// the line: it is ~55 columns, and last is where the line is already designed
/// to give way. Being last also keeps the match clean — nothing follows the URL
/// for a greedy pattern to swallow.
fn pr_part(pr: &pr::Pr) -> String {
    let (color, glyph) = match pr.state {
        pr::State::Open => PR_OPEN,
        pr::State::Draft => PR_DRAFT,
        pr::State::Merged => PR_MERGED,
        pr::State::Closed => PR_CLOSED,
    };
    format!(
        "\x1b]8;;{0}\x1b\\{color}{glyph}{0}{RESET}\x1b]8;;\x1b\\",
        pr.url
    )
}

/// What the process should do with an input.
#[derive(Debug, PartialEq, Eq)]
pub enum Render {
    /// Print this line (a trailing newline is added by the caller).
    Line(String),
    /// Python raised before printing anything: no stdout, exit status 1.
    Crash(&'static str),
}

/// Render one status line from a stdin payload.
///
/// `now` is epoch seconds, injected so the golden tests can pin it. `pr` is
/// passed in rather than looked up, for the same reason: it is the one part of
/// the line that comes from outside the payload, and keeping the lookup in
/// `main` leaves this a pure function of its arguments. `None` — no repo, no
/// PR, or nothing cached yet — renders no segment rather than a placeholder.
pub fn render(input: &[u8], now: f64, pr: Option<&pr::Pr>) -> Render {
    let data: Value = match std::str::from_utf8(input)
        .ok()
        .and_then(|s| serde_json::from_str(s).ok())
    {
        Some(v) => v,
        // json.JSONDecodeError / EOFError
        None => return Render::Line(TIME_GLYPH.to_string()),
    };
    if !data.is_object() {
        // `data.get(...)` on a list/str/int/None — AttributeError.
        return Render::Crash("status line payload is not a JSON object");
    }

    let model = get(&data, "model")
        .and_then(|m| get(m, "display_name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("?");
    let empty = Value::Object(Default::default());
    let ctx = get(&data, "context_window").unwrap_or(&empty);
    let cost_obj = get(&data, "cost").unwrap_or(&empty);
    let rate = get(&data, "rate_limits").unwrap_or(&empty);

    let cost = format_cost(num(get(cost_obj, "total_cost_usd")));
    let duration = format_duration(num(get(cost_obj, "total_duration_ms")));
    let added = get(cost_obj, "total_lines_added").filter(|v| v.is_number());
    let removed = get(cost_obj, "total_lines_removed").filter(|v| v.is_number());

    let mut parts = vec![
        format!("{}{model}{RESET}", model_color(model)),
        context_usage(ctx),
    ];

    for (key, label, window_len) in WINDOWS {
        let mut sub = get(rate, key);
        if sub.is_none() && key == "seven_day" {
            sub = get(rate, "weekly"); // tolerate an alternate weekly key
        }
        parts.push(window_part(label, window_len, sub, now));
    }

    // session metrics last — least important, first to clip on narrow terminals
    if !cost.is_empty() {
        parts.push(format!("{COST}{cost}{RESET}"));
    }
    if !duration.is_empty() {
        parts.push(format!("{DUR}{duration}{RESET}"));
    }
    let nonzero = |v: Option<&Value>| v.and_then(Value::as_f64).is_some_and(|f| f != 0.0);
    if nonzero(added) || nonzero(removed) {
        let a = added.map(py_num_str).unwrap_or_else(|| "0".into());
        let r = removed.map(py_num_str).unwrap_or_else(|| "0".into());
        parts.push(format!("{GREEN}+{a}{RESET}/{RED}-{r}{RESET}"));
    }
    // Last, and after the session metrics: it is the longest thing on the line
    // by some way, and a keyboard URL picker wants it with nothing after it.
    if let Some(pr) = pr {
        parts.push(pr_part(pr));
    }

    Render::Line(parts.join(&format!("{SEP}│{RESET}")))
}

/// Wall-clock epoch seconds, overridable by `CLAUDE_STATUSLINE_NOW`.
///
/// The override exists for the golden tests: `resets_at` is only meaningful
/// relative to "now", so a fixture that exercises a mid-window block has to pin
/// the instant its expected output was captured at. It is read only from the
/// environment and never from the payload, so it cannot change what a status
/// line looks like in normal use.
pub fn now_epoch() -> f64 {
    if let Ok(s) = std::env::var("CLAUDE_STATUSLINE_NOW") {
        if let Ok(v) = s.trim().parse::<f64>() {
            return v;
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_match_the_python_branches() {
        assert_eq!(format_duration(None), "");
        assert_eq!(format_duration(Some(0.0)), "");
        assert_eq!(format_duration(Some(1500.0)), "1s");
        assert_eq!(format_duration(Some(59_999.0)), "59s");
        assert_eq!(format_duration(Some(60_000.0)), "1m00s");
        assert_eq!(format_duration(Some(125_000.0)), "2m05s");
        assert_eq!(format_duration(Some(3_600_000.0)), "1h00m");
        assert_eq!(format_duration(Some(7_845_000.0)), "2h10m");
    }

    #[test]
    fn costs_switch_precision_at_a_cent() {
        assert_eq!(format_cost(None), "");
        assert_eq!(format_cost(Some(0.0)), "$0.0000");
        assert_eq!(format_cost(Some(0.0037)), "$0.0037");
        assert_eq!(format_cost(Some(0.01)), "$0.01");
        assert_eq!(format_cost(Some(12.345)), "$12.35");
    }

    #[test]
    fn token_counts_trim_the_way_rstrip_does() {
        assert_eq!(fmt_tokens(0.0), "0");
        assert_eq!(fmt_tokens(999.0), "999");
        assert_eq!(fmt_tokens(1000.0), "1k");
        assert_eq!(fmt_tokens(1500.0), "1.5k");
        assert_eq!(fmt_tokens(20_000.0), "20k");
        assert_eq!(fmt_tokens(99_960.0), "100k");
        assert_eq!(fmt_tokens(123_400.0), "123k");
        assert_eq!(fmt_tokens(1_000_000.0), "1M");
        assert_eq!(fmt_tokens(1_234_567.0), "1.2M");
    }

    #[test]
    fn etas_pick_the_coarsest_two_units() {
        assert_eq!(fmt_eta(0.0), "<1m");
        assert_eq!(fmt_eta(-5.0), "<1m");
        assert_eq!(fmt_eta(59.0), "<1m");
        assert_eq!(fmt_eta(60.0), "1m");
        assert_eq!(fmt_eta(3600.0), "1h00m");
        assert_eq!(fmt_eta(9330.0), "2h35m");
        assert_eq!(fmt_eta(285_630.0), "3d7h");
    }

    #[test]
    fn the_ramp_saturates_at_both_ends() {
        assert_eq!(ramp(-1.0), RAMP[0]);
        assert_eq!(ramp(0.0), RAMP[0]);
        assert_eq!(ramp(0.5), RAMP[5]);
        assert_eq!(ramp(1.0), RAMP[10]);
        assert_eq!(ramp(9.0), RAMP[10]);
    }

    #[test]
    fn model_families_are_matched_in_ladder_order() {
        assert_eq!(model_color("Claude Haiku 4.5"), c!(80));
        assert_eq!(model_color("Sonnet 4.6"), c!(75));
        assert_eq!(model_color("Fable"), c!(141));
        assert_eq!(model_color("Opus 5 (1M context)"), c!(208));
        assert_eq!(model_color("something else"), MODEL_FALLBACK);
        // sonnet is earlier in the ladder than opus, so it wins a tie
        assert_eq!(model_color("sonnet-opus"), c!(75));
    }

    #[test]
    fn resets_at_accepts_every_shape_the_python_did() {
        let n = |v: serde_json::Value| parse_reset(Some(&v));
        assert_eq!(n(serde_json::json!(1_700_000_000)), Some(1.7e9));
        // > 1e12 is milliseconds
        assert_eq!(n(serde_json::json!(1_700_000_000_000i64)), Some(1.7e9));
        assert_eq!(n(serde_json::json!("1700000000")), Some(1.7e9));
        assert_eq!(n(serde_json::json!("2023-11-14T22:13:20Z")), Some(1.7e9));
        assert_eq!(
            n(serde_json::json!("2023-11-14T22:13:20+00:00")),
            Some(1.7e9)
        );
        assert_eq!(
            n(serde_json::json!("2023-11-14T23:13:20+01:00")),
            Some(1.7e9)
        );
        assert_eq!(n(serde_json::json!("20231114T221320Z")), Some(1.7e9));
        assert_eq!(n(serde_json::json!("2023-11-14 22:13:20Z")), Some(1.7e9));
        assert_eq!(n(serde_json::json!("2023-11-14")), Some(1_699_920_000.0));
        assert_eq!(n(serde_json::json!("  ")), None);
        assert_eq!(n(serde_json::json!("")), None);
        assert_eq!(n(serde_json::json!("tomorrow")), None);
        assert_eq!(n(serde_json::json!(true)), None);
        assert_eq!(n(serde_json::json!(null)), None);
        // out of datetime's range
        assert_eq!(n(serde_json::json!(1e12)), None);
        assert_eq!(n(serde_json::json!(-1e18)), None);
    }

    #[test]
    fn a_non_object_payload_is_a_crash_not_a_line() {
        assert_eq!(
            render(b"[]", 0.0, None),
            Render::Crash("status line payload is not a JSON object")
        );
        assert_eq!(render(b"", 0.0, None), Render::Line("\u{23f3}".into()));
        assert_eq!(render(b"nope", 0.0, None), Render::Line("\u{23f3}".into()));
    }
}
