# lakeview

A terminal browser for [lakeFS](https://lakefs.io), written in Rust with
[ratatui](https://ratatui.rs).

Three panes: repositories on the left at a fixed column width, then a tree of
one ref's objects and a live detail/preview pane dividing the rest by a
configurable ratio.

```
╭────────────────────────────────────────────────────────────────────────────────╮
│  1 Browse   2 Commits   3 Help                        mock · http://lakefs:8000│
╰────────────────────────────────────────────────────────────────────────────────╯
╭ Repositories ─── 2 ╮╭ main ──────────────── 6 ╮╭ daily_rollup.json ──────────╮
│                    ││                         ││                             │
│ ▾ quickstart   main││ ▾ data/                 ││ size            56 B        │
│     ● main  a3f01b2││   ▾ curated/            ││ modified        10:59       │
│     ○ dev   77c19ee││    ▌  daily_ro…    56 B ││ type            json        │
│     ◇ v1.0  1b2c3d4││   ▸ raw/                ││ ──────────────────────────  │
│ ● analytics    main││ ▸ images/               ││  1 {                        │
│                    ││   README.md       120 B ││  2   "clicks": 2,           │
╰────────────────────╯╰─────────────────────────╯╰─────────────────────────────╯
    ↑↓/jk move  →/l open  ←/h back  space toggle  / search  d download  q quit
```

Selecting a repository opens its default branch straight away — the listing
already names it, so nothing extra is fetched. Press `→` on a repository to
expand it and pick another branch or tag.

A repository with one branch never lists it: its row already selects it, so a
row of its own would only add a step. Such a repository is marked `●` — the same
mark its lone default branch would have worn a row below — rather than `▸`, and
`→` steps straight into the tree. Tags are always listed, so a single-branch
repository that has them still expands.

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
repos_width = 28        # columns given to the repositories pane
tree_ratio = 1          # the tree and the preview divide the remaining width
preview_ratio = 1       # by these ratios; preview_ratio = 0 hides the pane
preview_bytes = 65536   # max bytes fetched when previewing a file
page_size = 500         # entries fetched per API request
search_max_requests = 300  # listings a recursive `/` search may spend
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
| `l` `→` `Enter` | expand; at a pane's edge, move focus right; on a file, zoom the preview; in a zoomed `.json` or `.jsonl`, unfold a row |
| `h` `←` `Backspace` | collapse, else go to the parent; at the top level, move focus left, or leave a zoom |
| `space` | expand / collapse in place, without moving focus |
| `g` / `G` | first / last entry |
| `Ctrl-d` / `Ctrl-u` | half-page down / up |
| `/` | search the focused pane (`Esc` clears) |
| `F` | in a zoomed `.jsonl`, filter which keys the records show |
| `d` | download the selected file into the working directory |
| `r` | reload the focused pane |
| `p` | switch profile |
| `1` `2` `3` / `Tab` | switch tab |
| `q` / `Ctrl-c` | quit |

Reloading the tree with `r` keeps the directories you had open.

## Search

`/` in the repositories pane filters it by name. `/` in the tree searches
**recursively**: it walks into closed directories and opens the path to every
match, so a file buried three levels down is found without opening anything by
hand. Directories already loaded match as you type; the walk over the rest
starts once you stop typing.

Clearing the search with `Esc` restores exactly the shape you had open — the
search never touches your own expand/collapse state.

A search stops after `search_max_requests` directory listings and says so
rather than quietly returning a partial result. Nothing is fetched twice, so
extending the search term costs nothing.

## Download

`d` on a file in the tree downloads it into the directory you started `lakeview`
in, under its own name. The whole object is fetched — `preview_bytes` caps what
the preview reads, not this — and streamed to disk, so an object larger than
memory still lands. The footer names the file and its size when it finishes.

An existing file is never overwritten: `report.csv` arriving a second time is
written as `report (1).csv`. The download runs in the background, so browsing
carries on while it does, and it reports where it landed even if you have moved
on to something else by then.

`d` works on files, not prefixes — downloading a whole subtree is a different
thing, so a directory says so rather than quietly doing nothing.

## JSON

Zooming a `.json` file (`→` on it in the tree) opens it one level down: the
root's own members, with every object and array inside them folded onto a row
each and marked `▸`. A file's shape is the thing worth seeing first; the values
under it are what you go looking for.

```
╭ daily_rollup.json ─────────────────────────── zoom  ╮
│                                                     │
│  1 {                                                │
│  2   "name": "daily_rollup",                        │
│  3   "clicks": 2,                                   │
│  4 ▸ "tags": ["a", "b"],                            │
│  5 ▾ "meta": {                                      │
│  6     "pid": 11,                                   │
│  7   ▸ "nested": {"deep": true}                     │
│  8   },                                             │
│  9   "ok": null                                     │
│ 10 }                                                │
╰─────────────────────────────────────────────────────╯
```

`→`, `Enter` or `space` unfolds the selected row a level at a time, and folds it
back up from its opening row or its closing bracket either way. `←` winds back
out the way it does in the tree, and leaves the zoom once nothing is left to
close. The file's own brackets don't fold — collapsing a whole file to `{…}`
says nothing.

Line numbers count the rows on show, so folding a block renumbers what is under
it. The document is re-indented from the parsed value, so there is no original
line number to keep.

The side pane leaves the folding alone and shows the whole file laid flat: at
that width there is no room to fold anything usefully.

## JSONL

Zooming a `.jsonl` or `.ndjson` file (`→` on it in the tree) gives every record
a row of its own, folded onto one line. `→`, `Enter` or `space` unfolds the
selected record to its top level; the objects and arrays inside it stay folded
on a row each, marked `▸`, and unfold a level at a time in turn:

```
╭ events.jsonl ───────────────────────────────── zoom  ╮
│                                                      │
│  1 ▸ {"level": "info", "msg": "boot", "meta": {…}}   │
│  2 ▾ {4}                                             │
│    │ {                                               │
│    │   "level": "warn",                              │
│    │   "msg": "retry",                               │
│    │ ▾ "meta": {                                     │
│    │     "pid": 11,                                  │
│    │   ▸ "tags": ["x", "y"]                          │
│    │   }                                             │
│    │ }                                                │
╰──────────────────────────────────────────────────────╯
```

The same keys fold a block back up, from its opening row or its closing bracket
either way, and what was open inside it is remembered.

`←` winds back out the way it does in the tree: it folds what is open, steps out
to the enclosing block when there is nothing there to fold, and only once the
whole record is folded again does it leave the zoom. `Esc` leaves at once.

### Filtering keys

Wide records are mostly noise more often than not. `F` opens a menu of the keys
the file's records use, as a tree that unfolds one level at a time, and each key
can be switched off:

```
╭ Filter keys ─────────────────────────────────────────────╮
│                                                          │
│   [x] level                                              │
│   [x] msg                                                │
│ ▾ [~] meta                                               │
│     [x] pid                                              │
│     [ ] tags                                             │
│                                                          │
╰ space on/off  ←→ fold  a/n all/none  ⏎ apply  esc cancel ╯
```

`space` (or a click) switches the selected key, `←`/`→` fold and unfold a level,
`a`/`n` switch all or none, `Enter` closes the menu and `Esc` puts the old filter
back. The records change behind the menu as you switch keys, so what you are
choosing is on show while you choose it.

Switching a key off takes everything nested under it with it, and switching one
back on clears the way down to it, so a `[x]` always means a key you will
actually see. A `[~]` means the key is shown but is hiding something below it.

Switched-off keys are gone from the folded previews, the unfolded bodies and the
side pane's lines alike, and the pane's title keeps count of how many. Nothing is
re-fetched or re-read to do it, so switching a key back on brings it straight
back. The menu is built from the first 500 records of the preview, so a key that
first appears after those is never hidden.

The filter belongs to the file you are looking at: moving to another object and
back starts clean. Only `.jsonl` has one — a whole `.json` file is a single value
rather than a shape repeated, so there is no set of keys worth switching off.

## Mouse

Mouse support is on by default.

| Action | Effect |
|---|---|
| click a row | focus that pane and select the row |
| double-click a row | expand / collapse it, or open it if there's nothing to expand |
| right-click | collapse, or go back |
| wheel over the focused pane | move the selection (the preview follows) |
| wheel over the other pane | scroll that pane's view only — no focus or selection change |
| wheel over the preview | scroll the preview, or move the selection in a zoomed `.json` / `.jsonl` |
| double-click a zoomed `.json` / `.jsonl` row | unfold or fold it |
| click a key in the `F` menu | switch it on or off |
| click away from that menu | close it, keeping the switches |
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
- Directories are fetched one level at a time, as you open them.
- Text previews are capped at `preview_bytes`; binary content falls back to a
  hex dump.
- The zoomed preview wraps long lines, hanging continuations under the content
  so the line numbers stay readable. The side pane lets them overflow instead —
  at that width, wrapping one long JSON string would bury the rest of the file.
- JSON is re-indented and syntax-coloured, preserving the file's key order, and
  folds a level at a time in the zoom — see below. A file too large to fetch
  whole won't parse, so it renders as plain text.
- `.jsonl` and `.ndjson` files unfold record by record, and `F` switches off the
  keys you don't want to read — see below.
- Everything is read-only — lakeview never writes to your lakeFS server. `d` is
  the only thing it writes anywhere, and only to the working directory.
