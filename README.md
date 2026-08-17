# lakeview

A terminal browser for [lakeFS](https://lakefs.io), written in Rust with
[ratatui](https://ratatui.rs). Repositories on the left, a tree of one ref's
objects in the middle, and a live preview on the right.

```
╭────────────────────────────────────────────────────────────────────────────────╮
│  1 Browse   2 Commits   3 Help                        mock · http://lakefs:8000│
╰────────────────────────────────────────────────────────────────────────────────╯
╭ Repositories ─── 2 ╮╭ main ──────────────── 6 ╮╭ daily_rollup.json ──────────╮
│                    ││                         ││                             │
│ ▾ quickstart   main││ ▾ data/                 ││  1 {                        │
│     ● main  a3f01b2││   ▾ curated/            ││  2   "name": "daily_rollup",│
│     ○ dev   77c19ee││    ▌  daily_ro…    56 B ││  3   "clicks": 2,           │
│     ◇ v1.0  1b2c3d4││   ▸ raw/                ││  4   "tags": [              │
│ ● analytics    main││ ▸ images/               ││  5     "a",                 │
│                    ││   README.md       120 B ││  6     "b"                  │
╰────────────────────╯╰─────────────────────────╯╰─────────────────────────────╯
    ↑↓/jk move  →/l expand  ⏎ open  ←/h back  space toggle  / search  q quit
```

Everything is read-only — lakeview never writes to your lakeFS server.

## Install

```sh
brew install jeril-divyam/tap/lakeview
```

Or take a binary from the [latest release](https://github.com/jeril-divyam/lakeview/releases/latest)
and drop it on your `PATH`:

```sh
tar xzf lakeview_*_linux_amd64.tar.gz
install -m755 lakeview ~/.local/bin/
```

Builds are published for macOS on Apple silicon and Linux on x86_64 and arm64.
The Linux binaries are static, so they don't care which distro you're on.
Anything else builds from source with `cargo build --release`.

## Getting started

```sh
lakeview init        # writes ~/.config/lakeview.toml (seeded from ~/.lakectl.yaml if present)
lakeview check       # verify the profile can reach its server
lakeview             # browse
```

Selecting a repository opens its default branch straight away. Press `→` to
expand it and pick another branch or tag instead.

## Configuration

Profiles live in `~/.config/lakeview.toml` (override with `--config`). Pick one
with `--profile NAME`, or switch between them at runtime with `p`.

```toml
default_profile = "local"

[profiles.local]
endpoint = "http://localhost:8000"
access_key_id = "AKIAIOSFOLQUICKSTART"
secret_access_key = "${LAKEFS_SECRET_ACCESS_KEY}"
```

Any value written as `${VAR}` is read from the environment at start-up, so
secrets need not be stored on disk. `lakeview init` writes the file mode `600`.

| Profile key | Required | Meaning |
|---|---|---|
| `endpoint` | yes | Server base URL; a trailing `/api/v1` is optional |
| `access_key_id` | yes | lakeFS access key |
| `secret_access_key` | yes | lakeFS secret |
| `default_repo` | no | Repository to open on start-up |
| `default_ref` | no | Ref to open on start-up |
| `verify_tls` | no | Set `false` to accept self-signed certificates (default `true`) |
| `timeout_secs` | no | HTTP timeout (default `30`) |
| `description` | no | Note shown by `lakeview profiles` |

An optional `[ui]` table tunes the layout and fetch limits — pane widths,
`preview_bytes`, `page_size`, `show_tags`, `mouse`. See
[`lakeview.example.toml`](lakeview.example.toml) for the full set with defaults.

## Keys

| Key | Action |
|---|---|
| `j` `k` / `↓` `↑` | move |
| `l` `→` | expand; at a pane's edge, move focus right — into a previewed `.json`, where it unfolds a row |
| `Enter` | everything `→` does, and on a file it zooms the preview full-screen |
| `h` `←` | collapse, else go to the parent; at the top level, move focus left; folding a document is how it leaves one |
| `Esc` `Backspace` | leave a zoom, or step out of the preview. Outside both, `Esc` clears the search |
| `space` | expand / collapse in place, without moving focus |
| `g` / `G` | first / last entry |
| `Ctrl-d` / `Ctrl-u` | half-page down / up |
| `/` | search the focused pane |
| `a` / `c` | in a document, unfold it all / fold it all back up |
| `F` | in a zoomed `.jsonl`, filter which keys the records show |
| `d` | download the selected file into the working directory |
| `r` | reload the focused pane |
| `p` | switch profile |
| `1` `2` `3` / `Tab` | switch tab |
| `q` / `Ctrl-c` | quit |

Mouse is on by default: click to select, double-click to expand or fold, drag
the pane borders to resize, wheel to scroll. Set `mouse = false` under `[ui]` to
keep your terminal's own text selection.

## What it does

**Search.** `/` in the tree searches recursively — it walks into closed
directories and opens the path to every match. `Esc` restores exactly the shape
you had open.

**Preview.** Text, JSON (re-indented and syntax-coloured) or a hex dump for
binaries, capped at `preview_bytes`. Press `Enter` on a file to zoom it
full-screen.

**JSON and JSONL.** A `.json` folds a level at a time, in the preview pane as
well as zoomed. `→` on the file steps the cursor out of the tree and into the
preview — one more step right, like the one from the repositories into the tree
— and `←` folds its way back out. Both views are one document, so a fold shows
in either, and the shape is kept for the session: leave the file and come back
and it is as you left it. A `.jsonl` or `.ndjson` gives every record its own
row, unfolding one level at a time in the zoom, and `F` switches off the keys
you'd rather not read.

**Download.** `d` fetches the whole object — not just the previewed head — and
streams it to the working directory. An existing file is never overwritten.

**Commits.** Tab `2` lists the commit log for the ref you're browsing.

## Commands

| Command | Purpose |
|---|---|
| `lakeview` | browse (`--repo`, `--ref` jump straight to a path) |
| `lakeview init [--force]` | write a starter config |
| `lakeview profiles` | list configured profiles |
| `lakeview check` | verify connectivity and credentials |

## License

MIT — see [LICENSE](LICENSE).
