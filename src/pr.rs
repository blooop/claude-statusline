//! The pull request for the branch the session is sitting on.
//!
//! This is the one segment that is not a function of stdin. Everything else on
//! the line is in the payload; a PR number is not, so it has to be found — and
//! finding it costs a round trip to GitHub, which a status line cannot pay.
//! `gh pr list` against a warm cache and a nearby network is ~450ms, and the
//! line is re-rendered on every message.
//!
//! So nothing here blocks. [`lookup`] is file reads and nothing else: it works
//! out which repo and branch the session is on, reads the answer that a
//! previous run left on disk, and — if that answer has gone stale — spawns a
//! detached copy of this binary to fetch a new one for *next* time. The render
//! it was called from prints whatever was already there, which is at worst a
//! few seconds old and at first nothing at all. A branch with no cached answer
//! shows no segment rather than a placeholder, because "no PR yet" and "not
//! asked yet" should not look different from a branch that simply has no PR.
//!
//! ## Why the git plumbing is read by hand
//!
//! `git rev-parse` would answer both questions, but it is a fork+exec (~4ms)
//! twice over, on the hot path, forever. `.git/HEAD` and `.git/config` are two
//! small file reads that answer the same questions exactly, and reading them
//! directly is what makes the cache key (repo, branch) rather than (directory):
//! switch branch and the line follows on the very next render instead of
//! waiting out a TTL on a stale directory-keyed entry.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// Younger than this, the cached answer is used as-is.
const FRESH: Duration = Duration::from_secs(30);
/// Overrides [`FRESH`], in seconds. It exists for the tests — the same reason
/// `CLAUDE_STATUSLINE_NOW` does — because the alternative is a test that waits
/// out the real window to watch what happens to a stale entry.
const FRESH_ENV: &str = "CLAUDE_STATUSLINE_PR_TTL";
/// A refresh that has not finished in this long is assumed dead, and its lock
/// is cleared so the next render can try again.
const LOCK_STALE: Duration = Duration::from_secs(60);

/// The argv[1] that puts this binary in refresh mode. Not a documented
/// interface: the only thing that passes it is [`spawn_refresh`] below.
pub const REFRESH_FLAG: &str = "--refresh-pr";

// ---------------------------------------------------------------------------
// What we found
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Draft,
    Merged,
    Closed,
}

impl State {
    fn parse(state: &str, is_draft: bool) -> State {
        match state {
            "MERGED" => State::Merged,
            "CLOSED" => State::Closed,
            _ if is_draft => State::Draft,
            _ => State::Open,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    pub number: u64,
    pub url: String,
    pub state: State,
}

/// Which repo, which branch — the pair a PR is looked up by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// `owner/repo`, as `gh --repo` wants it.
    pub slug: String,
    pub branch: String,
}

// ---------------------------------------------------------------------------
// The hot path
// ---------------------------------------------------------------------------

/// The PR for whatever branch `dir` is on, if a previous run has found one.
///
/// Never blocks on the network, and never fails loudly: every step that could
/// go wrong (no repo, detached HEAD, no origin, no cache, unreadable cache)
/// yields `None`, which renders as no segment at all.
pub fn lookup(dir: &Path) -> Option<Pr> {
    if std::env::var_os("CLAUDE_STATUSLINE_NO_PR").is_some() {
        return None;
    }
    let checkout = checkout(dir)?;
    let path = cache_path(&checkout)?;

    let cached = fs::read(&path).ok();
    // `is_none_or` would say this in one line, but it is newer than the MSRV.
    let stale = match age(&path) {
        None => true,
        Some(a) => a > fresh_for(),
    };
    if stale {
        spawn_refresh(&checkout, &path);
    }
    // A cache written by an older build, or truncated by a refresh that died
    // mid-write, parses to None and simply shows nothing until the next one.
    parse_cache(&cached?)
}

fn fresh_for() -> Duration {
    match std::env::var(FRESH_ENV)
        .ok()
        .and_then(|s| s.trim().parse().ok())
    {
        Some(secs) => Duration::from_secs_f64(secs),
        None => FRESH,
    }
}

/// How long ago `path` was written, or `None` if it does not exist.
fn age(path: &Path) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    // A clock that has gone backwards since the write reads as "brand new",
    // which errs towards not spawning a refresher rather than towards spawning
    // one on every render.
    Some(
        SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default(),
    )
}

/// Start a detached refresh, unless one is already running.
///
/// The lock is taken here rather than in the child because the race is here:
/// two renders a millisecond apart would otherwise both spawn. `create_new` is
/// the atomic part — whichever process creates the file wins, and the loser
/// returns without having paid for anything.
fn spawn_refresh(checkout: &Checkout, cache: &Path) {
    let lock = cache.with_extension("lock");
    if age(&lock).is_some_and(|a| a > LOCK_STALE) {
        let _ = fs::remove_file(&lock);
    }
    if let Some(parent) = lock.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .is_err()
    {
        return; // someone else is already on it, or the cache dir is unwritable
    }

    let Ok(exe) = std::env::current_exe() else {
        let _ = fs::remove_file(&lock);
        return;
    };
    // Spawned and deliberately never waited on: this process is about to print
    // a line and exit, and the child is reparented to init when it does. All
    // three stdio handles go to null so a stray `gh` message can never land in
    // the prompt the status line is drawn in.
    let spawned = Command::new(exe)
        .arg(REFRESH_FLAG)
        .arg(&checkout.slug)
        .arg(&checkout.branch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if spawned.is_err() {
        let _ = fs::remove_file(&lock);
    }
}

// ---------------------------------------------------------------------------
// The detached half
// ---------------------------------------------------------------------------

/// Ask GitHub, write the answer, drop the lock. Runs in the spawned child.
pub fn refresh(slug: &str, branch: &str) {
    let checkout = Checkout {
        slug: slug.to_string(),
        branch: branch.to_string(),
    };
    let Some(path) = cache_path(&checkout) else {
        return;
    };
    // `--state all`, because a merged PR is still the answer to "what is this
    // branch": the branch outlives the merge and the link stays useful.
    let out = Command::new("gh")
        .args([
            "pr",
            "list",
            "--repo",
            slug,
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
            "number,url,state,isDraft",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();

    // A `gh` that failed — no auth, no network, not a GitHub remote — writes
    // nothing, so the previous answer stays up and the lock's expiry paces the
    // retries. Only a successful call is allowed to erase a known PR.
    if let Ok(out) = out {
        if out.status.success() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&path, first_pr_json(&out.stdout));
        }
    }
    let _ = fs::remove_file(path.with_extension("lock"));
}

/// The first element of `gh`'s array, or `null` for an empty one.
///
/// Stored as `gh` shaped it rather than re-encoded, so the cache file is
/// readable and the parse below has one shape to handle.
fn first_pr_json(stdout: &[u8]) -> Vec<u8> {
    let parsed: Option<Value> = serde_json::from_slice(stdout).ok();
    match parsed
        .as_ref()
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
    {
        Some(pr) => serde_json::to_vec(pr).unwrap_or_else(|_| b"null".to_vec()),
        None => b"null".to_vec(),
    }
}

fn parse_cache(bytes: &[u8]) -> Option<Pr> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let number = v.get("number")?.as_u64()?;
    let url = v.get("url")?.as_str()?;
    if !is_safe_url(url) {
        return None;
    }
    Some(Pr {
        number,
        url: url.to_string(),
        state: State::parse(
            v.get("state").and_then(Value::as_str).unwrap_or(""),
            v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        ),
    })
}

/// A URL that cannot break out of the escape sequence it is about to sit in.
///
/// The hyperlink below puts this string inside an OSC, where a stray ESC, BEL
/// or newline would end the sequence early and leave the rest of it printed in
/// the prompt. `gh` will not produce such a URL; this is here so that a
/// hand-edited or corrupted cache file cannot either.
fn is_safe_url(url: &str) -> bool {
    !url.is_empty()
        && url.len() < 512
        && !url.chars().any(|c| c.is_control())
        && (url.starts_with("https://") || url.starts_with("http://"))
}

// ---------------------------------------------------------------------------
// Where the session is
// ---------------------------------------------------------------------------

/// Read `dir`'s repo slug and branch straight out of the git directory.
pub fn checkout(dir: &Path) -> Option<Checkout> {
    let (git_dir, common_dir) = git_dirs(dir)?;
    let branch = head_branch(&fs::read_to_string(git_dir.join("HEAD")).ok()?)?;
    let url = origin_url(&fs::read_to_string(common_dir.join("config")).ok()?)?;
    Some(Checkout {
        slug: slug_from_url(&url)?,
        branch,
    })
}

/// The git directory for `dir`, and the *common* directory its config lives in.
///
/// Those differ in a worktree, which is not an exotic case here: `dl` hands
/// every branch its own checkout and agents work in `git worktree` trees, where
/// `.git` is a file pointing at `…/.git/worktrees/<name>`. HEAD is per-worktree
/// and config is shared, so a worktree that read config from its own git dir
/// would find no remote at all.
fn git_dirs(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let dot = d.join(".git");
        if dot.is_dir() {
            return Some((dot.clone(), dot));
        }
        if dot.is_file() {
            let text = fs::read_to_string(&dot).ok()?;
            let target = text.strip_prefix("gitdir:")?.trim();
            // Relative gitdirs are relative to the directory holding the file.
            let git_dir = match Path::new(target).is_absolute() {
                true => PathBuf::from(target),
                false => d.join(target),
            };
            let common = match fs::read_to_string(git_dir.join("commondir")) {
                Ok(rel) => git_dir.join(rel.trim()),
                Err(_) => git_dir.clone(),
            };
            return Some((git_dir, common));
        }
        cur = d.parent();
    }
    None
}

/// The branch name in a `HEAD` file, or `None` for a detached HEAD.
///
/// Detached is deliberately nothing rather than a guess: there is no branch, so
/// there is no PR to be on, and a raw sha in the line would only be noise.
fn head_branch(head: &str) -> Option<String> {
    let r = head.trim().strip_prefix("ref:")?.trim();
    let branch = r.strip_prefix("refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

/// `url` from the `[remote "origin"]` section of a git config.
///
/// Enough INI to read git's own writing: section headers, `key = value`, `#`
/// and `;` comments. Not a general parser — it never has to be, because the
/// file it reads is one git wrote.
fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for raw in config.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // `[remote "origin"]`, and the subsection name is case-sensitive
            // where the section name is not.
            let header = header.trim();
            in_origin = header
                .split_once(char::is_whitespace)
                .is_some_and(|(section, name)| {
                    section.eq_ignore_ascii_case("remote") && name.trim() == "\"origin\""
                });
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim().eq_ignore_ascii_case("url") {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// `owner/repo` out of any remote URL git will have written.
///
/// `git@host:owner/repo.git`, `https://host/owner/repo.git`,
/// `ssh://git@host/owner/repo` — the host is dropped on purpose. It is only
/// ever handed to `gh --repo`, which resolves it against whichever host the
/// user is authenticated to, and the URL that ends up in the line comes back
/// from `gh` rather than being rebuilt from this.
fn slug_from_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let path = match url.split_once("://") {
        // scheme://[user@]host/owner/repo
        Some((_, rest)) => rest.split_once('/')?.1,
        // scp-like: [user@]host:owner/repo
        None => url.rsplit_once(':')?.1,
    };
    let path = path.trim_start_matches('/').trim_end_matches(".git");
    let mut parts = path.rsplitn(3, '/');
    let repo = parts.next().filter(|s| !s.is_empty())?;
    let owner = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

// ---------------------------------------------------------------------------
// Cache location
// ---------------------------------------------------------------------------

fn cache_path(checkout: &Checkout) -> Option<PathBuf> {
    let root = std::env::var_os("CLAUDE_STATUSLINE_CACHE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .filter(|p| !p.as_os_str().is_empty())?;
    // Readable stem plus a hash of the exact pair: the stem is for whoever
    // opens the cache directory wondering what is in it, and the hash is what
    // actually keeps `feat/a` and `feat-a` apart once slashes are flattened.
    let key = format!("{}\n{}", checkout.slug, checkout.branch);
    let stem: String = key
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' => c,
            _ => '-',
        })
        .take(60)
        .collect();
    Some(
        root.join("claude-statusline")
            .join(format!("{stem}-{:016x}.json", fnv1a(key.as_bytes()))),
    )
}

/// FNV-1a, 64-bit. A cache file name needs distinctness, not cryptography, and
/// this is eight lines against a dependency.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// The `workspace.current_dir` a payload names, if it names one.
pub fn payload_dir(input: &[u8]) -> Option<PathBuf> {
    let v: Value = serde_json::from_slice(input).ok()?;
    let dir = v.get("workspace")?.get("current_dir")?.as_str()?;
    (!dir.is_empty()).then(|| PathBuf::from(OsStr::new(dir)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_names_the_branch_unless_it_is_detached() {
        assert_eq!(
            head_branch("ref: refs/heads/ags/statusline-pr-link\n").as_deref(),
            Some("ags/statusline-pr-link")
        );
        assert_eq!(head_branch("ref: refs/heads/main").as_deref(), Some("main"));
        // detached: a bare sha, no ref
        assert_eq!(head_branch("df8ca6178a1c4c0c9d2f\n"), None);
        // a ref that is not a branch
        assert_eq!(head_branch("ref: refs/tags/v0.1.0\n"), None);
        assert_eq!(head_branch("ref: refs/heads/\n"), None);
        assert_eq!(head_branch(""), None);
    }

    #[test]
    fn origin_is_picked_out_of_a_real_git_config() {
        let config = "\
[core]
\trepositoryformatversion = 0
[remote \"upstream\"]
\turl = git@github.com:someone/else.git
\tfetch = +refs/heads/*:refs/remotes/upstream/*
[remote \"origin\"]
\turl = git@github.com:kinisi-robotics/kinisi_ros.git
\tfetch = +refs/heads/*:refs/remotes/origin/*
[branch \"main\"]
\tremote = origin
";
        assert_eq!(
            origin_url(config).as_deref(),
            Some("git@github.com:kinisi-robotics/kinisi_ros.git")
        );
        // upstream-only: not origin, so nothing
        assert_eq!(origin_url("[remote \"upstream\"]\n\turl = x\n"), None);
        assert_eq!(origin_url("[core]\n\turl = not-a-remote\n"), None);
        // a commented-out origin is not an origin
        assert_eq!(origin_url("# [remote \"origin\"]\n#\turl = x\n"), None);
    }

    #[test]
    fn every_remote_url_git_writes_reduces_to_owner_slash_repo() {
        let s = |u: &str| slug_from_url(u);
        assert_eq!(
            s("git@github.com:blooop/claude-statusline.git").as_deref(),
            Some("blooop/claude-statusline")
        );
        assert_eq!(
            s("git@github.com:blooop/claude-statusline").as_deref(),
            Some("blooop/claude-statusline")
        );
        assert_eq!(
            s("https://github.com/blooop/claude-statusline.git").as_deref(),
            Some("blooop/claude-statusline")
        );
        assert_eq!(
            s("https://github.com/blooop/claude-statusline/").as_deref(),
            Some("blooop/claude-statusline")
        );
        assert_eq!(
            s("ssh://git@github.com/blooop/claude-statusline.git").as_deref(),
            Some("blooop/claude-statusline")
        );
        // a self-hosted host with a path prefix still ends in owner/repo
        assert_eq!(
            s("https://git.example.com/gh/blooop/claude-statusline.git").as_deref(),
            Some("blooop/claude-statusline")
        );
        assert_eq!(s("/home/kinisi-ci/some/local/clone"), None);
        assert_eq!(s(""), None);
    }

    #[test]
    fn a_state_is_whatever_gh_called_it_with_draft_on_top() {
        assert_eq!(State::parse("OPEN", false), State::Open);
        assert_eq!(State::parse("OPEN", true), State::Draft);
        assert_eq!(State::parse("MERGED", false), State::Merged);
        // gh reports a merged PR that was opened as a draft with isDraft
        // false, but be explicit: merged and closed outrank draft either way.
        assert_eq!(State::parse("MERGED", true), State::Merged);
        assert_eq!(State::parse("CLOSED", true), State::Closed);
    }

    #[test]
    fn gh_output_reduces_to_the_first_pr_or_null() {
        let one = br#"[{"isDraft":false,"number":11573,"state":"OPEN","url":"https://github.com/o/r/pull/11573"}]"#;
        let pr = parse_cache(&first_pr_json(one)).expect("a pr");
        assert_eq!(pr.number, 11573);
        assert_eq!(pr.state, State::Open);
        assert_eq!(pr.url, "https://github.com/o/r/pull/11573");

        assert_eq!(first_pr_json(b"[]"), b"null");
        assert_eq!(parse_cache(b"null"), None);
        // a half-written or truncated cache is nothing, never a panic
        assert_eq!(parse_cache(b""), None);
        assert_eq!(parse_cache(br#"{"number":1}"#), None);
        assert_eq!(first_pr_json(b"not json"), b"null");
    }

    #[test]
    fn a_url_that_could_break_the_escape_sequence_is_refused() {
        assert!(is_safe_url("https://github.com/o/r/pull/1"));
        assert!(!is_safe_url("https://github.com/o/r\x1b]8;;evil\x1b\\"));
        assert!(!is_safe_url("https://github.com/o/r\npull/1"));
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url(""));
        assert!(!is_safe_url(&format!("https://x/{}", "a".repeat(600))));
    }

    #[test]
    fn the_cache_key_separates_branches_that_flatten_alike() {
        let p = |branch: &str| {
            std::env::set_var("CLAUDE_STATUSLINE_CACHE", "/tmp/cs-test");
            cache_path(&Checkout {
                slug: "o/r".into(),
                branch: branch.into(),
            })
            .unwrap()
        };
        assert_ne!(p("feat/a"), p("feat-a"));
        assert!(p("feat/a").starts_with("/tmp/cs-test/claude-statusline"));
    }

    #[test]
    fn a_payload_without_a_workspace_names_no_directory() {
        assert_eq!(
            payload_dir(br#"{"workspace":{"current_dir":"/home/x"}}"#),
            Some(PathBuf::from("/home/x"))
        );
        assert_eq!(payload_dir(br#"{"workspace":{"current_dir":""}}"#), None);
        assert_eq!(payload_dir(br#"{"model":{"display_name":"Opus 5"}}"#), None);
        assert_eq!(payload_dir(b"[]"), None);
        assert_eq!(payload_dir(b""), None);
    }
}
