# claude-statusline

One binary that renders [Claude Code](https://claude.com/claude-code)'s status
line. It reads the status-line JSON payload on stdin and prints a single
ANSI-coloured line:

```
Opus 5 (1M context)│45.7k/200k│🐢22%/48% ⏳2h35m/5h│🔥63%/53% ⏳3d7h/7d│$3.42│30m45s│+271/-88
└─ model ──────────┘└ context ┘└─ 5h window ──────┘└─ 7d window ──────┘└cost┘└ time ┘└ diff ┘
```

It is a **faithful port** of the stdlib-Python `status-line.py` it replaces —
same output, byte for byte, for the same stdin. The port exists because a Python
script needs a Python, and a devcontainer that only wants `claude` and `gh` has
no reason to carry one. Nothing about the display changed.

## Install

```bash
pixi global install --channel https://prefix.dev/blooop claude-statusline
```

That puts `claude-statusline` on `PATH` via `~/.pixi/bin`. Then point Claude
Code's `statusLine` at it in `~/.claude/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "claude-statusline"
  }
}
```

## How to read the line

* **Model** — one hue per family, on an item-rarity ladder: haiku cyan
  (uncommon), sonnet sky blue (rare), fable violet (epic), opus orange
  (legendary). Anything unrecognised is blue-grey, never grey — grey means
  *absent*, never *unimportant*.
* **Context** — tokens used over window size, on a green→red ramp walked at
  1.6× so it reaches red near 60%, which is where compaction starts to loom
  rather than where the window ends.
* **Rate-limit windows** (`5h`, `7d`) — a pacing gauge, `used%/even%`, where
  `even` is the usage you would have at a perfectly steady burn (i.e. the
  percentage of the window that has elapsed). `20%/87%` is 67 points of
  headroom; 🐢 says you are on pace or banking it, 🔥 says you are ahead of an
  even burn. `⏳42m/5h` is time to reset over window length.

  The colour of the whole block — digits, glyphs and clock together — is the
  **projected finish**: `used/even × 100`, i.e. where the window lands at reset
  if the current rate holds. 100 is break-even and fully green; 200 is twice the
  sustainable rate and fully red. Absolute usage is printed but never hued: an
  even burn projects to 100 at any level, so the number tells you the level and
  the hue tells you the rate.
* **Session** — cost, wall-clock and lines added/removed come last, so a narrow
  terminal clips them first.

A window with no data yet reads `⏳5h --` in grey. A window with usage but no
reset clock drops the pacing pair and colours by absolute usage instead, because
there is no second term to compare against. Nothing on stdin, or unparseable
JSON, prints a bare `⏳`.

## Fidelity

The port is held to the original by two test layers:

* **`tests/golden.rs`** — every fixture under `tests/fixtures/` is a recorded
  stdin payload plus the exact stdout the Python script produced for it. The
  expected files are recordings, not hand-written expectations, so a diff is the
  port drifting from the original. Fixtures whose output depends on the clock
  carry a `now` file — the instant the recording was made — replayed through
  `CLAUDE_STATUSLINE_NOW`.
* **`tests/parity.rs`** — runs the real script live and diffs it against the
  binary, for the case a recording cannot catch: the script itself changing.
  Skipped unless you point it at a copy:

  ```bash
  STATUS_LINE_PY=~/.claude/scripts/status-line.py cargo test --test parity
  ```

`CLAUDE_STATUSLINE_NOW` (epoch seconds) is the only environment variable the
binary reads, and it exists for those tests: `resets_at` only means anything
relative to "now", so a fixture exercising a mid-window block has to pin the
instant it was recorded at.

Two behaviours are worth naming because they are inherited rather than designed:
a top-level JSON value that is not an object prints nothing and exits 1 (Python
raises `AttributeError` there), and wrong-typed values *inside* the object are
the one deliberate divergence — Python raises, this treats them as absent, on
the grounds that a status line that renders beats an exception in the prompt.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Releasing

The conda package is **not** built here. It is built and published by
[blooop/blooop-feedstock](https://github.com/blooop/blooop-feedstock), which owns
the recipe (`recipes/claude-statusline/recipe.yaml`) and the `prefix.dev`
credentials for the `blooop` channel. This repo only produces the tag the recipe
fetches its source from:

```bash
# bump [package] version in Cargo.toml, commit, then:
git tag v0.1.0 && git push origin v0.1.0
```

Then, in the feedstock, bump `version` and the source `sha256` in the recipe and
run its release workflow:

```bash
gh workflow run release-workflow.yml --repo blooop/blooop-feedstock \
  -f package=claude-statusline -f force_build=true
```

The tag has to exist before the feedstock build, because the recipe fetches the
tag's source tarball by URL and checksum.

## Licence

MIT.
