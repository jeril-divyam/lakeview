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
│ ▾ quickstart   main││ ▾ data/                 ││  1 {                        │
│     ● main  a3f01b2││   ▾ curated/            ││  2   "name": "daily_rollup",│
│     ○ dev   77c19ee││    ▌  daily_ro…    56 B ││  3   "clicks": 2,           │
│     ◇ v1.0  1b2c3d4││   ▸ raw/                ││  4   "tags": [              │
│ ● analytics    main││ ▸ images/               ││  5     "a",                 │
│                    ││   README.md       120 B ││  6     "b"                  │
╰────────────────────╯╰─────────────────────────╯╰─────────────────────────────╯
    ↑↓/jk move  →/l expand  ⏎ open  ←/h back  space toggle  / search  q quit
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
brew install jeril-divyam/tap/lakeview
```

Or take a binary from the [latest release](https://github.com/jeril-divyam/lakeview/releases/latest)
and drop it somewhere on your `PATH`:

```sh
tar xzf lakeview_*_linux_amd64.tar.gz
install -m755 lakeview ~/.local/bin/
```

Builds are published for macOS on Apple silicon and for Linux on x86_64 and
arm64. The Linux binaries are statically linked against musl, so they do not
care which distro or glibc version you are on. Anything else builds from
source:

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
| `l` `→` | expand; at a pane's edge, move focus right; in a zoomed `.json` or `.jsonl`, unfold a row |
| `Enter` | everything `→` does, and on a file it zooms the preview full-screen |
| `h` `←` | collapse, else go to the parent; at the top level, move focus left; in a zoom, fold back up |
| `Esc` `Backspace` | leave a zoom. Outside one, `Esc` clears the search and `Backspace` acts as `←` |
| `space` | expand / collapse in place, without moving focus |
| `g` / `G` | first / last entry |
| `Ctrl-d` / `Ctrl-u` | half-page down / up |
| `/` | search the focused pane |
| `a` / `c` | in a zoom, unfold it all — or to the level you are reading at / fold it all back up |
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

Zooming a `.json` file (`Enter` on it in the tree) opens it unfolded all the way
down: every object and array inside it on show, marked `▾`. A file is one value,
and reading it is reading the whole of it — `c` folds it back to the root's own
members when the shape is what you are after.

```
╭ daily_rollup.json ─────────────────────────── zoom  ╮
│                                                     │
│  1 {                                                │
│  2   "name": "daily_rollup",                        │
│  3   "clicks": 2,                                   │
│  4 ▾ "tags": [                                      │
│  5     "a",                                         │
│  6     "b"                                          │
│  7   ],                                             │
│  8 ▾ "meta": {                                      │
│  9     "pid": 11,                                   │
│ 10   ▾ "nested": {                                  │
│ 11       "deep": true                               │
│ 12     }                                            │
│ 13   },                                             │
│ 14   "ok": null                                     │
│ 15 }                                                │
╰─────────────────────────────────────────────────────╯
```

`→`, `Enter` or `space` unfolds the selected row a level at a time. `space` folds
it back up as well, from its opening row or its closing bracket either way; `→`
only ever opens, so pressing it on something already open leaves it alone. `←`
winds back out the way it does in the tree, and stops once nothing is left to
close; `Esc` or `Backspace` leaves the zoom. The file's own brackets don't fold —
collapsing a whole file to `{…}` says nothing.

Line numbers count the rows on show, so folding a block renumbers what is under
it. The document is re-indented from the parsed value, so there is no original
line number to keep.

The side pane leaves the folding alone and shows the whole file laid flat: at
that width there is no room to fold anything usefully. It does wrap, though — a
file is one value read top to bottom, so a long string in the middle of it is
worth the lines it takes, and continuations hang under the nesting so the shape
still reads down the pane.

## JSONL

Zooming a `.jsonl` or `.ndjson` file (`Enter` on it in the tree) gives every record
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

`space` folds a block back up, from its opening row or its closing bracket either
way, and what was open inside it is remembered. `→` and `Enter` only unfold —
descending is one direction, as it is in the tree.

`←` winds back out the way it does in the tree: it folds what is open, steps out
to the enclosing block when there is nothing there to fold, and stops once the
whole record is folded again. `Esc` and `Backspace` are what leave the zoom — a key
you hold down to fold your way up a record should not be able to lose you the file,
which is why `←` isn't one of them.

### All at once

`a` reads the record the cursor is in. On a **folded** record there is no level
to copy, so it opens **all of it** — every record, all the way down. Inside an
**open** one it means "the rest like this one", and brings every record to the
level that record is open to: having unfolded one record's `meta`, `a` gives you
the whole file that way. Anywhere inside the record does, the folded rows within
it included — what counts is how deep the record is open, not which of its rows
you are resting on.

Levelling off folds as well as unfolds — a record somebody had opened deeper than
the level goes back to it, so "every record at level 2" is what you get rather
than "at least level 2". The footer names the level it settled on, since a row
folded at level 2 looks no different from one folded at level 5.

`c` folds everything back up, however deep any of it was open, and forgets what
was open inside — unlike `←` and `space`, which remember. In a `.jsonl` it is the
state the file was zoomed in at.

In a zoomed `.json` the two are simpler: the file is one value rather than a shape
repeated, so there is nothing to level it against. `a` opens the whole of it
wherever the cursor sits — which is how it was zoomed in — and `c` shuts it back
to level 1, the root's own members, since the root brackets don't fold.

A folded row is truncated rather than wrapped — unfolding it is how you see the
rest, and re-flowing one record over ten lines would bury the records under it.
Everything else wraps, so the long values an unfolded record exposes are
readable at full width.

The side pane doesn't fold — at that width there is no room to usefully — but it
gives each record a line, syntax-coloured like the JSON preview beside it, and
lets a long one overflow rather than wrapping it: a record is a row of its own
here, and re-flowing one over ten lines would bury the records under it. The
records are re-spaced to be coloured at all, so what it shows is the JSON each
line holds rather than its exact bytes; the zoom is the same. A record that isn't
valid JSON keeps its raw text, in red with the parse error beside it, and unfolds
to show the message and the text. Records lost to the `preview_bytes` cap are
marked `truncated` in the pane's title rather than shown half-read.

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

The menu and the record you are reading keep the same shape. `F` opens it
unfolded exactly as far as that record is, so it lists the keys the record is
showing and no more, and `←`/`→` unfold and fold that key in the record from then
on — the key under the cursor is always one whose values are on show behind it.
An array is no level of naming here, so unfolding `spans` where the records hold
`"spans": [{…}]` opens the list and its entries along with it. The rest of the
file stays as you left it; opening the whole of it is what `a` is for.

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
| drag the border between two panes | resize them — see below |
| click `«` / `»` under the repositories pane | fold it down to its marks, or back |
| wheel over the focused pane | scroll the view; the selection stays where it is and only comes along once the view would leave it behind |
| wheel over the other pane | scroll that pane's view only — no focus or selection change |
| wheel over the preview | scroll the preview; in a zoomed `.json` / `.jsonl` the selection stays where it is and only comes along once the view would leave it behind |
| double-click a zoomed `.json` / `.jsonl` row | unfold or fold it |
| click a key in the `F` menu | switch it on or off |
| click away from that menu | close it, keeping the switches |
| click a tab | switch tab |

Capturing the mouse takes over your terminal's click-drag text selection; most
terminals still let you select with **Shift** held. If you'd rather keep native
selection, set `mouse = false` under `[ui]` and everything stays keyboard-driven.

### Resizing the panes

Drag the border between two panes and they follow the pointer. The border wears
the amber accent while you hold it, and a press on a border is only ever a grab —
it never moves the selection behind it.

No pane can be dragged away to nothing: each stops at its own floor. Two of them
give way at the ends instead. Shove the preview's border off the right-hand edge
and it closes — the same thing `preview_ratio = 0` says. Shove the repositories
pane's border off the left and it folds down to a rail of its marks, which is
`repos_width = 0`. Both borders stop well short of their edge first, so the last
stretch is deliberate rather than a slip, and dragging back far enough for a whole
pane brings it back.

### Folding the repositories pane

The `«` under the repositories pane folds it to a rail showing nothing but the mark
on each row — `▸` and `▾` for repositories, `●` `○` `◇` for the branches and tags
under an open one, indented a column so the shape still reads down it. `»` unfolds
it again, to the width it was folded from.

```
╭ Repositories ───────── 1 ╮      ╭────╮
│                          │      │    │
│▌▸ quickstart        main │  «   │▌▸  │
│                          │      │    │
╰─────────── « ────────────╯      ╰ » ─╯
```

The rail keeps the same rows the full pane had, so folding never moves your
selection, and the columns it gives up go to the tree. It also survives a terminal
too narrow to have held the list at all, where the full pane would have dropped
out.

Where you leave a border is written to your config, so the layout survives a
restart — a folded pane included, since being folded is just `repos_width = 0`.
The width to unfold *to* is only remembered for the session, so a pane that starts
folded unfolds to the floor. Only `repos_width`, `tree_ratio` and `preview_ratio`
are touched, and only
those three lines: comments, spacing and key order all stay as they were, and a
`${VAR}` credential is never read, rewritten or expanded on disk. The ratios are
written as the column counts you dragged to, which is why they come back as
something like `76` and `44` rather than `1` and `1` — they still mean the same
thing, and a wider terminal still divides itself in the same proportion. If the
file turns out not to be in a shape lakeview is sure it can edit, it says so in
the footer and leaves the file alone rather than guessing.

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
- A selected file's pane is its contents and nothing else. Its size is already
  on its row in the tree, and the rest of what lakeFS knows about the object is
  not what you opened the pane to read. A prefix still names itself, since it
  has no contents to show.
- Text previews are capped at `preview_bytes`; binary content falls back to a
  hex dump.
- Long lines wrap in both the zoom and the side pane, hanging continuations
  under the content so the line numbers stay readable. A folded `.jsonl` record
  is the exception: it holds to its own row and overflows — see below.
- JSON is re-indented and syntax-coloured, preserving the file's key order, and
  folds a level at a time in the zoom — see below. A file too large to fetch
  whole won't parse, so it renders as plain text.
- `.jsonl` and `.ndjson` files unfold record by record, and `F` switches off the
  keys you don't want to read — see below.
- Everything is read-only — lakeview never writes to your lakeFS server. `d` is
  the only thing it writes anywhere, and only to the working directory.

## License

MIT — see [LICENSE](LICENSE).
