# lakeview

A terminal browser for [lakeFS](https://lakefs.io), written in Rust with
[ratatui](https://ratatui.rs).

Columns open to the right as you descend — repositories → refs → object
prefixes — with a live detail/preview pane pinned to the right edge.

```
╭────────────────────────────────────────────────────────────────────────────────╮
│  1 Browse   2 Commits   3 Help                        mock · http://lakefs:8000│
╰────────────────────────────────────────────────────────────────────────────────╯
 lakefs://quickstart/main/data/curated
╭‹ main ───────── 4 ╮╭ data ────────── 3 ╮╭ curated ──── 1 ╮╭ daily_rollup.json ╮
│ ▸ data/           ││ ▸ curated/        ││▌  daily_ro… 56B││ size        56 B   │
│ ▸ images/         ││ ▸ raw/            ││                ││ modified    10:59  │
│   README.md  120 B││   lakes.pa… 2.05kB││                ││ type        json   │
│   lakes.sou…  47 B││                   ││                ││ ─────────────────  │
│                   ││                   ││                ││  1 {               │
│                   ││                   ││                ││  2   "clicks": 2,  │
╰───────────────────╯╰───────────────────╯╰────────────────╯╰────────────────────╯
        ↑↓/jk  move   →/l  open   ←/h  back   /  filter   y  copy   q  quit
```

A repository with only one ref skips the branch column entirely — there is
nothing to choose, so opening it lands straight in the object listing.

## Install

```sh
cargo build --release
install -m755 target/release/lakeview ~/.local/bin/
```

## Getting started

```sh
lakeview init        # writes ~/.config/lakeview.toml (seeded from ~/.lakectl.yaml if present)
lakeview check       # verify the profile can reach its server
lakeview             # browse
```

## Configuration

Profiles live in `~/.config/lakeview.toml` (override with `--config`). Any
number of profiles may be defined; pick one with `--profile NAME`, or switch
between them at runtime with `p`.

```toml
default_profile = "local"

[ui]
column_width = 28       # min width of a Miller column before older ones collapse
preview_percent = 38    # share of the screen given to the preview pane (0 disables)
preview_bytes = 65536   # max bytes fetched when previewing a file
page_size = 500         # entries fetched per API request
show_tags = true        # list tags alongside branches
mouse = true            # set false to restore terminal text selection

[profiles.local]
endpoint = "http://localhost:8000"
access_key_id = "AKIAIOSFOLQUICKSTART"
secret_access_key = "${LAKEFS_SECRET_ACCESS_KEY}"

[profiles.prod]
endpoint = "https://lakefs.example.com"
access_key_id = "${LAKEFS_PROD_KEY_ID}"
secret_access_key = "${LAKEFS_PROD_SECRET}"
description = "production cluster"
default_repo = "analytics-lake"
default_ref = "main"
verify_tls = true
timeout_secs = 30
```

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

Any value written as `${VAR}` is read from the environment at start-up, so
secrets need not be stored on disk. `lakeview init` writes the file with mode
`600`.

## Keys

| Key | Action |
|---|---|
| `j` `k` / `↓` `↑` | move |
| `l` `→` `Enter` | open — a file opens the zoomed preview |
| `h` `←` `Backspace` | close the rightmost column |
| `g` / `G` | first / last entry |
| `Ctrl-d` / `Ctrl-u` | half-page down / up |
| `/` | filter the focused column (`Esc` clears) |
| `y` | copy the selected `lakefs://` URI (OSC 52, works over SSH) |
| `r` | reload the focused column |
| `p` | switch profile |
| `1` `2` `3` / `Tab` | switch tab |
| `q` / `Ctrl-c` | quit |

Pressing open on a column that is still loading is remembered and replayed once
the data arrives, so fast drill-downs don't lose keystrokes.

## Mouse

Mouse support is on by default.

| Action | Effect |
|---|---|
| click a row | select it; clicking an earlier column closes the columns to its right |
| double-click a row | open it |
| right-click | close the rightmost column |
| wheel over the focused column | move the selection (the preview follows) |
| wheel over an earlier column | scroll that column's view only — no focus or selection change |
| wheel over the preview | scroll the preview |
| click a tab | switch tab |

Capturing the mouse takes over your terminal's click-drag text selection; most
terminals still let you select with **Shift** held. If you'd rather keep native
selection, set `mouse = false` under `[ui]` and everything stays keyboard-driven.

## Commands

| Command | Purpose |
|---|---|
| `lakeview` | browse (`--repo`, `--ref` jump straight to a path) |
| `lakeview init [--force]` | write a starter config |
| `lakeview profiles` | list configured profiles |
| `lakeview check` | verify connectivity and credentials |

## Notes

- Listings are paginated transparently and sorted directories-first.
- Text previews are capped at `preview_bytes`; binary content falls back to a
  hex dump.
- JSON is re-indented and syntax-coloured, preserving the file's key order. A
  file too large to fetch whole won't parse, so it renders as plain text.
- Everything is read-only — lakeview never writes to your lakeFS server.
