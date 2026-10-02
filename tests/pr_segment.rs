//! End to end for the pull-request segment: a checkout on disk, a stub `gh`,
//! and the binary asked to render twice.
//!
//! The unit tests in `src/pr.rs` cover the pieces — HEAD, config, slug, cache
//! key. What only a whole-process test can show is the shape of the mechanism:
//! that the first render is *empty and non-blocking*, that it leaves a refresh
//! running behind it, and that the second render picks up what that refresh
//! wrote. A status line that fetched inline would pass every unit test here and
//! still be the wrong program.
//!
//! No network: `gh` is a shell script on a PATH this test controls, so what
//! comes back is whatever the case wants, immediately.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use support::run_binary_env;

const PR_URL: &str = "https://github.com/kinisi-robotics/kinisi_ros/pull/11573";
const GH_ONE_OPEN_PR: &str = r#"[{"number":11573,"url":"https://github.com/kinisi-robotics/kinisi_ros/pull/11573","state":"OPEN","isDraft":false,"headRepositoryOwner":{"login":"kinisi-robotics"}}]"#;

/// What a terminal actually shows: the line with its escape sequences removed.
///
/// Enough of an ANSI stripper for a status line — SGR (`ESC [ … m`) and OSC
/// (`ESC ] … ST`), which is all this program emits.
fn visible(line: &str) -> String {
    let b: Vec<char> = line.chars().collect();
    let (mut out, mut i) = (String::new(), 0);
    while i < b.len() {
        if b[i] != '\u{1b}' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        match b.get(i + 1) {
            // CSI: ends at the first byte in @..~
            Some('[') => {
                i += 2;
                while i < b.len() && !('@'..='~').contains(&b[i]) {
                    i += 1;
                }
                i += 1;
            }
            // OSC: ends at ST (ESC \) or BEL
            Some(']') => {
                i += 2;
                while i < b.len() {
                    if b[i] == '\u{7}' {
                        i += 1;
                        break;
                    }
                    if b[i] == '\u{1b}' && b.get(i + 1) == Some(&'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    out.trim_end().to_string()
}

/// A scratch directory that takes itself away again.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("claude-statusline-{name}-{nanos}"));
        fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A directory that looks enough like a checkout to be read as one.
fn plant_checkout(root: &Path, branch: &str, origin: &str) -> PathBuf {
    plant_checkout_with(
        root,
        branch,
        &format!("[remote \"origin\"]\n\turl = {origin}\n"),
    )
}

/// A checkout whose config, past `[core]`, is `extra` verbatim.
fn plant_checkout_with(root: &Path, branch: &str, extra: &str) -> PathBuf {
    let work = root.join("work");
    let git = work.join(".git");
    fs::create_dir_all(&git).expect("git dir");
    fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}\n")).expect("HEAD");
    fs::write(
        git.join("config"),
        format!("[core]\n\tbare = false\n{extra}"),
    )
    .expect("config");
    work
}

/// A `gh` on PATH that prints `stdout` and exits 0, or exits `code` if given.
///
/// Every call appends its argv to `<root>/gh-args`, one call per line, so a
/// test can see which repo and branch were asked about.
fn plant_gh(root: &Path, stdout: &str, code: i32) -> PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let gh = bin.join("gh");
    let log = root.join("gh-args");
    fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nprintf '%s' '{stdout}'\nexit {code}\n",
            log.display()
        ),
    )
    .expect("gh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    bin
}

/// The one cache entry under `cache`, insisting there is exactly one.
fn cache_entry(cache: &Path) -> PathBuf {
    let mut found: Vec<_> = fs::read_dir(cache.join("claude-statusline"))
        .expect("cache dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    assert_eq!(found.len(), 1, "expected one cache entry: {found:?}");
    found.remove(0)
}

/// Every `gh` call the stub has seen, one argv per line.
fn gh_calls(root: &Path) -> String {
    fs::read_to_string(root.join("gh-args")).unwrap_or_default()
}

fn payload(dir: &Path) -> Vec<u8> {
    format!(
        r#"{{"model":{{"display_name":"Opus 5"}},"workspace":{{"current_dir":"{}"}}}}"#,
        dir.display()
    )
    .into_bytes()
}

/// Render until the segment turns up, or give up. The refresh is a detached
/// process, so "it has not landed yet" is a legitimate intermediate state.
fn render_until_pr(input: &[u8], envs: &[(&str, &str)], within: Duration) -> String {
    let deadline = Instant::now() + within;
    loop {
        let last = String::from_utf8_lossy(&run_binary_env(input, envs).stdout).into_owned();
        if last.contains("\u{1b}]8;;") || Instant::now() > deadline {
            return last;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn the_first_render_is_empty_and_the_refresh_it_starts_fills_the_second() {
    let scratch = Scratch::new("hit");
    let work = plant_checkout(
        scratch.path(),
        "ags/build_summary_mins",
        "git@github.com:kinisi-robotics/kinisi_ros.git",
    );
    let bin = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];
    let input = payload(&work);

    // Nothing is cached, so the first line carries no segment — and, crucially,
    // does not wait for one.
    let first = String::from_utf8_lossy(&run_binary_env(&input, &envs).stdout).into_owned();
    assert!(
        !first.contains("11573"),
        "the first render should not have waited for gh: {first:?}"
    );
    assert!(
        first.starts_with('\u{1b}'),
        "still a status line: {first:?}"
    );

    let line = render_until_pr(&input, &envs, Duration::from_secs(10));
    assert!(
        line.contains(&format!("\u{1b}]8;;{PR_URL}\u{1b}\\")),
        "expected an OSC 8 link to the PR: {line:?}"
    );

    // The URL has to be in the *visible text*, not only in the OSC 8 target.
    // A keyboard URL picker — kitty's hints kitten on ctrl+shift+e, and the
    // tmux and Vim equivalents — scans what is on screen and never reads the
    // escape, so a tidy `#11573` leaves the link reachable by mouse only. This
    // is the whole reason the segment prints the URL in full.
    let text = visible(&line);
    assert!(
        text.contains(PR_URL),
        "the URL must be on screen, not just in the escape: {text:?}"
    );
    assert!(text.contains(&format!("● {PR_URL}")), "open PR: {text:?}");

    // Last on the line, with nothing after it for a greedy match to swallow.
    assert!(text.ends_with(PR_URL), "the PR goes last: {text:?}");
}

#[test]
fn a_branch_with_no_pr_renders_exactly_the_line_it_would_have_without_one() {
    let scratch = Scratch::new("miss");
    let work = plant_checkout(
        scratch.path(),
        "ags/no-pr-here",
        "git@github.com:kinisi-robotics/kinisi_ros.git",
    );
    let bin = plant_gh(scratch.path(), "[]", 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];
    let input = payload(&work);

    let line = render_until_pr(&input, &envs, Duration::from_secs(3));
    assert!(!line.contains("\u{1b}]8;;"), "no link at all: {line:?}");

    // The empty answer really was cached — the refresh ran and wrote `null` —
    // so this is "no PR", not "never asked".
    assert_eq!(fs::read_to_string(cache_entry(&cache)).unwrap(), "null");
}

#[test]
fn a_gh_that_fails_leaves_the_last_good_answer_standing() {
    let scratch = Scratch::new("fail");
    let work = plant_checkout(
        scratch.path(),
        "ags/build_summary_mins",
        "git@github.com:kinisi-robotics/kinisi_ros.git",
    );
    let cache = scratch.path().join("cache");
    let cache_s = cache.to_str().unwrap().to_string();
    let good = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let input = payload(&work);

    // Prime the cache with a real answer.
    let primed = render_until_pr(
        &input,
        &[
            ("PATH", good.to_str().unwrap()),
            ("CLAUDE_STATUSLINE_CACHE", &cache_s),
        ],
        Duration::from_secs(10),
    );
    assert!(primed.contains(PR_URL), "primed: {primed:?}");

    // Now break `gh` — no auth, no network, whatever — and make every entry
    // instantly stale so the next render is guaranteed to try a refresh.
    // Losing the link on a blip would be worse than showing one a minute old,
    // so the old answer has to survive the failure.
    let broken = plant_gh(scratch.path(), "", 1);
    let envs = [
        ("PATH", broken.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", &cache_s[..]),
        ("CLAUDE_STATUSLINE_PR_TTL", "0"),
    ];
    let line = String::from_utf8_lossy(&run_binary_env(&input, &envs).stdout).into_owned();
    assert!(line.contains(PR_URL), "stale but still linked: {line:?}");

    // Let the failed refresh finish, then confirm it erased nothing and, on the
    // render after it, that the link is still there.
    std::thread::sleep(Duration::from_millis(500));
    let entry = cache_entry(&cache);
    assert!(
        fs::read_to_string(&entry).unwrap().contains("11573"),
        "a failed gh must not overwrite a known PR"
    );
    let again = String::from_utf8_lossy(&run_binary_env(&input, &envs).stdout).into_owned();
    assert!(again.contains(PR_URL), "still linked: {again:?}");
}

#[test]
fn a_full_page_of_other_owners_prs_leaves_the_last_good_answer_standing() {
    // `--head` matches by name alone, so a popular branch name can fill the
    // whole `--limit` with forks' PRs and push ours off the page. That page
    // says nothing about our PR, so it must not erase the one we know.
    let scratch = Scratch::new("full-page");
    let work = plant_checkout(
        scratch.path(),
        "ags/build_summary_mins",
        "git@github.com:kinisi-robotics/kinisi_ros.git",
    );
    let cache = scratch.path().join("cache");
    let cache_s = cache.to_str().unwrap().to_string();
    let good = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let input = payload(&work);
    let primed = render_until_pr(
        &input,
        &[
            ("PATH", good.to_str().unwrap()),
            ("CLAUDE_STATUSLINE_CACHE", &cache_s),
        ],
        Duration::from_secs(10),
    );
    assert!(primed.contains(PR_URL), "primed: {primed:?}");

    let rows: Vec<String> = (1..=20)
        .map(|n| {
            format!(
                r#"{{"number":{n},"url":"https://github.com/kinisi-robotics/kinisi_ros/pull/{n}","state":"OPEN","isDraft":false,"headRepositoryOwner":{{"login":"fork{n}"}}}}"#
            )
        })
        .collect();
    let crowded = plant_gh(scratch.path(), &format!("[{}]", rows.join(",")), 0);
    let envs = [
        ("PATH", crowded.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", &cache_s[..]),
        ("CLAUDE_STATUSLINE_PR_TTL", "0"),
    ];
    let line = String::from_utf8_lossy(&run_binary_env(&input, &envs).stdout).into_owned();
    assert!(line.contains(PR_URL), "stale but still linked: {line:?}");

    std::thread::sleep(Duration::from_millis(500));
    assert!(
        fs::read_to_string(cache_entry(&cache))
            .unwrap()
            .contains("11573"),
        "a page with no room for our PR must not overwrite it"
    );
    let again = String::from_utf8_lossy(&run_binary_env(&input, &envs).stdout).into_owned();
    assert!(again.contains(PR_URL), "still linked: {again:?}");
}

#[test]
fn a_directory_that_is_not_a_checkout_asks_nothing_and_shows_nothing() {
    let scratch = Scratch::new("norepo");
    let plain = scratch.path().join("plain");
    fs::create_dir_all(&plain).expect("plain dir");
    // A `gh` that would fail the test loudly if it were ever reached.
    let bin = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];

    let line = render_until_pr(&payload(&plain), &envs, Duration::from_secs(2));
    assert!(!line.contains("\u{1b}]8;;"), "no link: {line:?}");
    assert!(
        !cache.join("claude-statusline").exists(),
        "nothing should have been looked up at all"
    );
}

#[test]
fn the_segment_can_be_switched_off_entirely() {
    let scratch = Scratch::new("off");
    let work = plant_checkout(
        scratch.path(),
        "ags/build_summary_mins",
        "git@github.com:kinisi-robotics/kinisi_ros.git",
    );
    let bin = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_NO_PR", "1"),
    ];

    let line = render_until_pr(&payload(&work), &envs, Duration::from_secs(2));
    assert!(!line.contains("\u{1b}]8;;"), "no link: {line:?}");
    assert!(
        !cache.join("claude-statusline").exists(),
        "and no background work either"
    );
}

#[test]
fn the_default_branch_asks_nothing_and_shows_nothing() {
    // `gh pr list --head main` matches every PR ever opened *from* a branch
    // called main — in kinisi_ros, a years-old PR that merged main into ci.
    // The default branch is not a PR's head, so it gets no segment at all.
    let scratch = Scratch::new("default");
    let work = plant_checkout(
        scratch.path(),
        "main",
        "git@github.com:kinisi-robotics/kinisi_ros.git",
    );
    let bin = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];

    let line = render_until_pr(&payload(&work), &envs, Duration::from_secs(2));
    assert!(!line.contains("\u{1b}]8;;"), "no link on main: {line:?}");
    assert_eq!(gh_calls(scratch.path()), "", "and gh never asked");
}

#[test]
fn a_default_branch_that_origin_head_names_is_skipped_too() {
    let scratch = Scratch::new("origin-head");
    let work = plant_checkout(
        scratch.path(),
        "develop",
        "git@github.com:kinisi-robotics/kinisi_ros.git",
    );
    let refs = work.join(".git/refs/remotes/origin");
    fs::create_dir_all(&refs).expect("refs");
    fs::write(refs.join("HEAD"), "ref: refs/remotes/origin/develop\n").expect("origin/HEAD");
    let bin = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];

    let line = render_until_pr(&payload(&work), &envs, Duration::from_secs(2));
    assert!(!line.contains("\u{1b}]8;;"), "no link on develop: {line:?}");
    assert_eq!(gh_calls(scratch.path()), "");
}

#[test]
fn a_pr_from_someone_elses_branch_of_the_same_name_is_passed_over() {
    // `--head` matches the branch name alone, so a fork's `fix-ci` and ours
    // come back in one list. Only the one whose head lives on our origin is
    // this branch's PR.
    let scratch = Scratch::new("owner");
    let work = plant_checkout(
        scratch.path(),
        "fix-ci",
        "git@github.com:kinisi-robotics/kinisi_ros.git",
    );
    let both = r#"[{"number":900,"url":"https://github.com/kinisi-robotics/kinisi_ros/pull/900","state":"OPEN","isDraft":false,"headRepositoryOwner":{"login":"someone-else"}},{"number":11573,"url":"https://github.com/kinisi-robotics/kinisi_ros/pull/11573","state":"OPEN","isDraft":false,"headRepositoryOwner":{"login":"Kinisi-Robotics"}}]"#;
    let bin = plant_gh(scratch.path(), both, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];

    let line = render_until_pr(&payload(&work), &envs, Duration::from_secs(10));
    assert!(line.contains(PR_URL), "our PR, not the fork's: {line:?}");
    assert!(!line.contains("/pull/900"), "{line:?}");
}

#[test]
fn a_fork_asks_upstream_about_the_branch_it_pushed_to() {
    // A fork clone: origin is the fork, the PR lives on upstream, and with
    // `push.default = upstream` the local branch pushes to the branch it
    // merges from, which has another name.
    let scratch = Scratch::new("fork");
    let work = plant_checkout_with(
        scratch.path(),
        "local-name",
        "[remote \"origin\"]\n\turl = git@github.com:blooop/kinisi_ros.git\n\
         [remote \"upstream\"]\n\turl = https://github.com/kinisi-robotics/kinisi_ros.git\n\
         [branch \"local-name\"]\n\tremote = origin\n\tmerge = refs/heads/pushed/name\n\
         [push]\n\tdefault = upstream\n",
    );
    let from_fork = r#"[{"number":11573,"url":"https://github.com/kinisi-robotics/kinisi_ros/pull/11573","state":"OPEN","isDraft":false,"headRepositoryOwner":{"login":"blooop"}}]"#;
    let bin = plant_gh(scratch.path(), from_fork, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];

    let line = render_until_pr(&payload(&work), &envs, Duration::from_secs(10));
    assert!(line.contains(PR_URL), "the upstream PR: {line:?}");
    let calls = gh_calls(scratch.path());
    assert!(
        calls.contains("--repo kinisi-robotics/kinisi_ros --head pushed/name"),
        "asked upstream about the pushed branch: {calls:?}"
    );
}

#[test]
fn a_stacked_branch_asks_about_itself_not_the_branch_it_was_cut_from() {
    // `git checkout -b my-fix origin/base-feature` records base-feature as the
    // upstream. That is where the branch pulls from; under the default
    // `push.default` it still pushes to `my-fix`, and that is its PR's head.
    let scratch = Scratch::new("stacked");
    let work = plant_checkout_with(
        scratch.path(),
        "my-fix",
        "[remote \"origin\"]\n\turl = git@github.com:kinisi-robotics/kinisi_ros.git\n\
         [branch \"my-fix\"]\n\tremote = origin\n\tmerge = refs/heads/base-feature\n",
    );
    let bin = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];

    let line = render_until_pr(&payload(&work), &envs, Duration::from_secs(10));
    assert!(line.contains(PR_URL), "{line:?}");
    let calls = gh_calls(scratch.path());
    assert!(calls.contains("--head my-fix "), "{calls:?}");
    assert!(!calls.contains("base-feature"), "{calls:?}");
}

#[test]
fn a_branch_cut_from_main_is_still_asked_about() {
    // `git checkout -b feat origin/main` tracks main. The default branch is
    // nobody's head, but `feat` is, and it pushes to `feat`.
    let scratch = Scratch::new("from-main");
    let work = plant_checkout_with(
        scratch.path(),
        "feat",
        "[remote \"origin\"]\n\turl = git@github.com:kinisi-robotics/kinisi_ros.git\n\
         [branch \"feat\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
    );
    let bin = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];

    let line = render_until_pr(&payload(&work), &envs, Duration::from_secs(10));
    assert!(line.contains(PR_URL), "{line:?}");
    let calls = gh_calls(scratch.path());
    assert!(calls.contains("--head feat "), "{calls:?}");
}

/// A checkout that fetches from origin but pushes to a `fork` remote.
fn plant_push_remote_checkout(root: &Path) -> PathBuf {
    plant_checkout_with(
        root,
        "fix-ci",
        "[remote \"origin\"]\n\turl = git@github.com:kinisi-robotics/kinisi_ros.git\n\
         [remote \"fork\"]\n\turl = git@github.com:blooop/kinisi_ros.git\n\
         [branch \"fix-ci\"]\n\tremote = origin\n\tpushRemote = fork\n\tmerge = refs/heads/fix-ci\n",
    )
}

#[test]
fn a_push_remote_names_the_head_owner() {
    // The triangular workflow: fetch from origin, push to a fork. The PR's head
    // is on the fork, so its owner is the one that counts.
    let scratch = Scratch::new("push-remote");
    let work = plant_push_remote_checkout(scratch.path());
    let from_fork = r#"[{"number":11573,"url":"https://github.com/kinisi-robotics/kinisi_ros/pull/11573","state":"OPEN","isDraft":false,"headRepositoryOwner":{"login":"blooop"}}]"#;
    let bin = plant_gh(scratch.path(), from_fork, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];

    let line = render_until_pr(&payload(&work), &envs, Duration::from_secs(10));
    assert!(line.contains(PR_URL), "the fork's PR: {line:?}");
}

#[test]
fn a_push_remote_passes_over_the_fetch_remotes_pr() {
    let scratch = Scratch::new("push-remote-miss");
    let work = plant_push_remote_checkout(scratch.path());
    let bin = plant_gh(scratch.path(), GH_ONE_OPEN_PR, 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];

    let line = render_until_pr(&payload(&work), &envs, Duration::from_secs(3));
    assert!(
        !line.contains("\u{1b}]8;;"),
        "origin's head is not ours: {line:?}"
    );
    assert_eq!(fs::read_to_string(cache_entry(&cache)).unwrap(), "null");
}

#[test]
fn the_pr_claude_code_sends_is_shown_without_asking_gh() {
    // Claude Code puts its own lookup in the payload. It already knows the
    // base repo and the head owner, so when it is there it wins, and the
    // render costs no refresh at all.
    let scratch = Scratch::new("payload");
    let work = plant_checkout(
        scratch.path(),
        "ags/build_summary_mins",
        "git@github.com:kinisi-robotics/kinisi_ros.git",
    );
    let bin = plant_gh(scratch.path(), "[]", 0);
    let cache = scratch.path().join("cache");
    let envs = [
        ("PATH", bin.to_str().unwrap()),
        ("CLAUDE_STATUSLINE_CACHE", cache.to_str().unwrap()),
    ];
    let input = format!(
        r#"{{"model":{{"display_name":"Opus 5"}},"workspace":{{"current_dir":"{}"}},"pr":{{"number":11573,"url":"{PR_URL}","review_state":"draft"}}}}"#,
        work.display()
    );

    let out = run_binary_env(input.as_bytes(), &envs);
    let text = visible(&String::from_utf8_lossy(&out.stdout));
    assert!(
        text.contains(&format!("○ {PR_URL}")),
        "a draft, linked: {text:?}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(gh_calls(scratch.path()), "", "gh never asked");
}
