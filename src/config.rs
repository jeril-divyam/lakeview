//! Config file handling: `~/.config/lakeview.toml`.
//!
//! The file holds any number of named profiles, each pointing at a lakeFS
//! server. Secrets may be written inline or, preferably, referenced from the
//! environment with `${VAR}` syntax.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Profile used when `--profile` is not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,

    #[serde(default)]
    pub ui: UiConfig,

    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,

    /// Notes about the file itself — keys that no longer do anything, say.
    /// Shown once on start-up so a stale setting isn't ignored in silence.
    #[serde(skip)]
    pub warnings: Vec<String>,

    /// The file this was read from, so a dragged pane border can be written back
    /// to it. Empty for a config that came from no file — there is nowhere to
    /// write, so nothing is written.
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    /// Base URL of the lakeFS server, e.g. `https://lakefs.example.com`.
    /// A trailing `/api/v1` is optional and stripped if present.
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,

    /// Repository to open on start-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_repo: Option<String>,
    /// Ref (branch, tag or commit) to open on start-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ref: Option<String>,

    /// Set to false to accept self-signed certificates.
    #[serde(default = "default_true")]
    pub verify_tls: bool,
    /// Seconds before a connect or a stalled read gives up. Bounds every stall
    /// rather than the whole request, so a large download can stream for as
    /// long as data keeps arriving.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Human-readable label shown in the header; defaults to the profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UiConfig {
    /// Width of the repositories pane, in columns; `0` folds it down to a rail of
    /// its row markers. `column_width` is accepted as an alias so configs written
    /// for the old Miller-column layout load.
    #[serde(default = "default_repos_width", alias = "column_width")]
    pub repos_width: u16,
    /// The tree's share of the width the repositories pane leaves over,
    /// weighed against `preview_ratio`.
    #[serde(default = "default_ratio")]
    pub tree_ratio: u16,
    /// The preview's share of that same width; `0` hides the pane entirely.
    #[serde(default = "default_ratio")]
    pub preview_ratio: u16,
    /// Maximum number of bytes fetched when previewing an object.
    #[serde(default = "default_preview_bytes")]
    pub preview_bytes: u64,
    /// Entries fetched per API page (lakeFS caps this at 1000).
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Directory listings a recursive `/` search may spend before giving up.
    /// Caps the work a single search can do on a wide tree.
    #[serde(default = "default_search_budget")]
    pub search_max_requests: usize,
    /// Show tags alongside branches under an expanded repository.
    #[serde(default = "default_true")]
    pub show_tags: bool,
    /// Capture the mouse for scrolling and clicking. Turning this off restores
    /// the terminal's own click-drag text selection.
    #[serde(default = "default_true")]
    pub mouse: bool,
}

fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    30
}
fn default_repos_width() -> u16 {
    28
}
fn default_ratio() -> u16 {
    1
}
fn default_preview_bytes() -> u64 {
    64 * 1024
}
fn default_page_size() -> u32 {
    500
}
fn default_search_budget() -> usize {
    300
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            repos_width: default_repos_width(),
            tree_ratio: default_ratio(),
            preview_ratio: default_ratio(),
            preview_bytes: default_preview_bytes(),
            page_size: default_page_size(),
            search_max_requests: default_search_budget(),
            show_tags: true,
            mouse: true,
        }
    }
}

impl Config {
    /// `~/.config/lakeview.toml`, honouring `$XDG_CONFIG_HOME`.
    pub fn default_path() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .ok_or_else(|| anyhow!("could not determine the user config directory"))?;
        Ok(dir.join("lakeview.toml"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        cfg.path = path.to_path_buf();

        // Unknown keys parse fine and are dropped, so a setting that has been
        // replaced would otherwise just stop working with no explanation.
        let table: toml::Value =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if table
            .get("ui")
            .and_then(|ui| ui.get("preview_percent"))
            .is_some()
        {
            cfg.warnings.push(
                "ui.preview_percent is gone — use preview_ratio (0 hides the pane)".into(),
            );
        }

        // Resolve ${ENV_VAR} references in credentials.
        for (name, profile) in cfg.profiles.iter_mut() {
            profile.access_key_id = expand_env(&profile.access_key_id)
                .with_context(|| format!("profile `{name}`: access_key_id"))?;
            profile.secret_access_key = expand_env(&profile.secret_access_key)
                .with_context(|| format!("profile `{name}`: secret_access_key"))?;
            profile.endpoint = expand_env(&profile.endpoint)
                .with_context(|| format!("profile `{name}`: endpoint"))?;
        }
        Ok(cfg)
    }

    /// Pick a profile by name, else the configured default, else the only one.
    pub fn select(&self, requested: Option<&str>) -> Result<(String, Profile)> {
        if self.profiles.is_empty() {
            bail!("no profiles defined — run `lakeview init` to create one");
        }
        let name = match requested.or(self.default_profile.as_deref()) {
            Some(n) => n.to_string(),
            None if self.profiles.len() == 1 => self.profiles.keys().next().unwrap().clone(),
            None => {
                let names: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
                bail!(
                    "multiple profiles defined ({}) — pass --profile NAME or set default_profile",
                    names.join(", ")
                );
            }
        };
        let profile = self
            .profiles
            .get(&name)
            .ok_or_else(|| {
                let names: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
                anyhow!("unknown profile `{name}` (known: {})", names.join(", "))
            })?
            .clone();
        Ok((name, profile))
    }

    /// Write the three layout keys back to the file this config came from, so a
    /// dragged pane border is still where it was left next time.
    ///
    /// The file is re-read here rather than remembered from start-up: whatever
    /// else has changed in it meanwhile is the user's to keep. Nothing is written
    /// for a config that came from no file, and nothing is written when the file
    /// already says what we would say.
    ///
    /// Note the deliberate absence of a `set_permissions` call, unlike
    /// `lakeview init`, which creates the file and so owns its mode. `fs::write`
    /// truncates in place, leaving mode and owner as the user set them;
    /// re-imposing `0600` here would override a choice that isn't ours. Writing
    /// through a temporary file and renaming would be worse still — `rename`
    /// replaces the inode, so the config would come back with whatever
    /// `File::create` and the umask decided, typically `0644`, which on a file
    /// that may hold an inline secret is a downgrade.
    pub fn save_layout(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        let Some(patched) = patch_ui_layout(&raw, &self.ui) else {
            bail!(
                "{} isn't in a shape lakeview can edit — set the pane sizes by hand",
                self.path.display()
            );
        };
        if patched == raw {
            return Ok(());
        }
        std::fs::write(&self.path, patched)
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

/// Replace `${VAR}` with the environment value. A literal string without any
/// `${` is returned unchanged, so plain inline secrets keep working.
fn expand_env(value: &str) -> Result<String> {
    if !value.contains("${") {
        return Ok(value.to_string());
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let end = tail
            .find('}')
            .ok_or_else(|| anyhow!("unterminated `${{` in config value"))?;
        let var = &tail[..end];
        let val = std::env::var(var)
            .with_context(|| format!("environment variable `{var}` referenced but not set"))?;
        out.push_str(&val);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

// ── remembering the layout ───────────────────────────────────────────────

/// Rewrite the three layout keys inside `[ui]`, leaving every other byte of the
/// file as it was — comments, spacing, key order, line endings and all.
///
/// Deliberately a line-patcher rather than a TOML-editing dependency, for the
/// same reason [`read_lakectl`] is a line-scanner: the file shape is the one
/// `lakeview init` writes, and the only thing that ever changes is the digits of
/// three integers. It is emphatically not `toml::to_string(&cfg)` — the config
/// held in memory has had its `${VAR}` credentials expanded, so serialising it
/// would write the user's secret into the file in place of the reference it was
/// read from, and drop every comment besides.
///
/// `None` means the file is not in a shape this is sure of, and nothing should be
/// written: `ui` given as a dotted key or an inline table, a key spelled some
/// other way, a value that is not a plain integer, or a patch that doesn't read
/// back as what was meant. Declining costs a remembered layout; guessing could
/// cost a config file that no longer parses.
pub fn patch_ui_layout(text: &str, ui: &UiConfig) -> Option<String> {
    let mut out = text.to_string();
    for (names, value) in [
        // The old Miller-column name is rewritten where it stands: both spellings
        // load as the same field, and serde rejects seeing a field twice, so the
        // canonical name added beside it would stop the file loading at all.
        (&["repos_width", "column_width"][..], ui.repos_width),
        (&["tree_ratio"][..], ui.tree_ratio),
        (&["preview_ratio"][..], ui.preview_ratio),
    ] {
        out = set_ui_key(&out, names, value)?;
    }

    // Read the patch back with the real parser before it can reach the disk: a
    // line-scanner is only as good as its assumptions about the file, and this is
    // the one way to check them — a key landing in the wrong table parses fine
    // but leaves the value at its default, which this catches. It also declines a
    // file broken in an editor since start-up, not ours to rewrite either.
    let check: Config = toml::from_str(&out).ok()?;
    let wanted = (ui.repos_width, ui.tree_ratio, ui.preview_ratio);
    let got = (
        check.ui.repos_width,
        check.ui.tree_ratio,
        check.ui.preview_ratio,
    );
    (got == wanted).then_some(out)
}

/// One line of the file. Patching between `content` and `end` is what keeps a
/// CRLF file a CRLF file: the terminator is spliced around, never rebuilt.
struct Line<'a> {
    /// Byte offset the line starts at.
    start: usize,
    /// Byte offset just past its terminator — where the next line starts.
    end: usize,
    /// The line without its `\n` or `\r\n`.
    content: &'a str,
}

impl Line<'_> {
    /// Byte offset just past the content, before any terminator.
    fn content_end(&self) -> usize {
        self.start + self.content.len()
    }
}

fn lines_of(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    for piece in text.split_inclusive('\n') {
        out.push(Line {
            start,
            end: start + piece.len(),
            content: piece.trim_end_matches(['\r', '\n']),
        });
        start += piece.len();
    }
    out
}

/// The line ending the file already uses, for a line that has to be added.
fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// A line with its comment cut off. A `#` inside a string would fool this; no key
/// `[ui]` holds has a string value.
fn code(line: &str) -> &str {
    line.split('#').next().unwrap_or("").trim_end()
}

/// The bare key a line assigns to. A quoted or dotted key is not one — nothing in
/// `[ui]` is written that way, and a patcher unsure what it is looking at should
/// leave the line alone.
fn key_of(line: &str) -> Option<&str> {
    let key = code(line).split_once('=')?.0.trim();
    (!key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then_some(key)
}

/// Set one key inside `[ui]`: rewriting the line that holds it, else adding the
/// key to the table, else adding the table to the file.
fn set_ui_key(text: &str, names: &[&str], value: u16) -> Option<String> {
    let lines = lines_of(text);
    let eol = eol_of(text);

    let Some(header) = lines.iter().position(|l| code(l.content).trim() == "[ui]") else {
        // No table to patch. `ui` written as a dotted key or an inline table is
        // still `ui`, and a `[ui]` header beside one would define it twice — a
        // parse error, so the file would stop loading altogether. Dotted keys can
        // only appear above the first header, so that is as far as this looks.
        let spelled_otherwise = lines
            .iter()
            .take_while(|l| !code(l.content).trim_start().starts_with('['))
            .filter_map(|l| code(l.content).split_once('='))
            .any(|(k, _)| {
                let k = k.trim();
                k == "ui" || k.starts_with("ui.") || k.contains("\"ui\"")
            });
        if spelled_otherwise {
            return None;
        }

        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        let blank_already = out.ends_with("\n\n") || out.ends_with("\r\n\r\n");
        if !out.is_empty() && !blank_already {
            out.push_str(eol);
        }
        out.push_str(&format!("[ui]{eol}{} = {value}{eol}", names[0]));
        return Some(out);
    };

    // The table runs to the next header, or to the end of the file — so `[ui]`
    // coming last is the ordinary case, and a `repos_width` under some later
    // `[profiles.x]` is out of reach by construction.
    let end = lines[header + 1..]
        .iter()
        .position(|l| code(l.content).trim_start().starts_with('['))
        .map_or(lines.len(), |i| header + 1 + i);
    let body = &lines[header + 1..end];

    if let Some(line) = body
        .iter()
        .find(|l| key_of(l.content).is_some_and(|k| names.contains(&k)))
    {
        let patched = replace_int(line.content, value)?;
        let mut out = String::with_capacity(text.len() + 8);
        out.push_str(&text[..line.start]);
        out.push_str(&patched);
        out.push_str(&text[line.content_end()..]);
        return Some(out);
    }

    // Quoted or dotted, the same key is the same key to TOML, so ours alongside it
    // would leave the file unparseable. Recognising every spelling isn't worth the
    // code; noticing that there might be one is.
    let spelled_otherwise = body.iter().any(|l| {
        code(l.content)
            .split_once('=')
            .is_some_and(|(k, _)| names.iter().any(|n| k.contains(*n)))
    });
    if spelled_otherwise {
        return None;
    }

    // After the table's last setting, so the key lands inside the table rather
    // than under the blank line that ends it.
    let at = body
        .iter()
        .rposition(|l| !code(l.content).trim().is_empty())
        .map_or(lines[header].end, |i| body[i].end);

    let mut out = String::with_capacity(text.len() + 32);
    out.push_str(&text[..at]);
    if !out.ends_with('\n') {
        out.push_str(eol);
    }
    out.push_str(&format!("{} = {value}{eol}", names[0]));
    out.push_str(&text[at..]);
    Some(out)
}

/// Swap the integer on the right of `=` for `value`. A trailing comment stays,
/// and stays in the column it was in: the spaces in front of it give or take what
/// the digits do, down to the one space that keeps them apart.
fn replace_int(line: &str, value: u16) -> Option<String> {
    let eq = line.find('=')?;
    let (head, rest) = line.split_at(eq + 1);
    let (slot, comment) = match rest.find('#') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };

    let old = slot.trim();
    if old.is_empty() || !old.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
        return None; // not a plain integer, so not ours to rewrite
    }
    let lead = &slot[..slot.len() - slot.trim_start().len()];
    let gap = &slot[slot.trim_end().len()..];
    let new = value.to_string();

    let gap = if comment.is_empty() || gap.is_empty() {
        gap.to_string()
    } else {
        let width = gap.len() as isize + old.len() as isize - new.len() as isize;
        " ".repeat(width.max(1) as usize)
    };
    Some(format!("{head}{lead}{new}{gap}{comment}"))
}

/// The starter file written by `lakeview init`.
pub const TEMPLATE: &str = r#"# lakeview configuration — https://docs.lakefs.io
#
# Credentials may be inlined or pulled from the environment with ${VAR}.

default_profile = "local"

[ui]
repos_width = 28        # columns for the repositories pane; 0 folds it to its marks
tree_ratio = 1          # the tree and the preview divide the rest by these
preview_ratio = 1       # two ratios; set preview_ratio = 0 to hide the pane
preview_bytes = 65536   # max bytes fetched when previewing a file
page_size = 500         # entries fetched per API request
search_max_requests = 300  # listings a recursive `/` search may spend
show_tags = true        # list tags alongside branches
mouse = true            # set false to restore terminal text selection

[profiles.local]
endpoint = "http://localhost:8000"
access_key_id = "AKIAIOSFOLQUICKSTART"
secret_access_key = "${LAKEFS_SECRET_ACCESS_KEY}"
# default_repo = "quickstart"
# default_ref  = "main"

# [profiles.prod]
# endpoint = "https://lakefs.example.com"
# access_key_id = "${LAKEFS_PROD_KEY_ID}"
# secret_access_key = "${LAKEFS_PROD_SECRET}"
# description = "production cluster"
# verify_tls = true
# timeout_secs = 30
"#;

/// Best-effort extraction of credentials from `~/.lakectl.yaml` so that
/// `lakeview init` can seed a working profile. Deliberately a small
/// line-scanner rather than a YAML dependency: the file shape is fixed.
pub fn read_lakectl() -> Option<(String, String, String)> {
    let path = dirs::home_dir()?.join(".lakectl.yaml");
    let text = std::fs::read_to_string(path).ok()?;
    let mut endpoint = None;
    let mut key = None;
    let mut secret = None;
    for line in text.lines() {
        let trimmed = line.trim();
        let Some((k, v)) = trimmed.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches(['"', '\'']).to_string();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "endpoint_url" => endpoint = Some(v),
            "access_key_id" => key = Some(v),
            "secret_access_key" => secret = Some(v),
            _ => {}
        }
    }
    Some((endpoint?, key?, secret?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui(repos_width: u16, tree_ratio: u16, preview_ratio: u16) -> UiConfig {
        UiConfig {
            repos_width,
            tree_ratio,
            preview_ratio,
            ..UiConfig::default()
        }
    }

    /// Patch with the defaults, so only the case under test differs.
    fn patch(text: &str) -> Option<String> {
        patch_ui_layout(text, &ui(28, 1, 1))
    }

    // ── rewriting a key in place ─────────────────────────────────────────

    #[test]
    fn a_key_with_a_comment_keeps_it() {
        let out = patch_ui_layout("[ui]\nrepos_width = 28  # columns\n", &ui(40, 1, 1)).unwrap();
        assert!(out.contains("# columns"), "{out}");
    }

    #[test]
    fn a_wider_number_borrows_a_space_from_the_comment_column() {
        // The `#` must not move: the digits and the gap pay for each other.
        let before = "[ui]\nrepos_width = 28        # columns\ntree_ratio = 1\npreview_ratio = 1\n";
        let out = patch_ui_layout(before, &ui(100, 1, 1)).unwrap();
        assert_eq!(comment_column(&out), comment_column(before), "{out}");
        assert!(out.contains("repos_width = 100"), "{out}");
    }

    #[test]
    fn a_narrower_number_gives_a_space_back() {
        let before = "[ui]\nrepos_width = 100      # columns\ntree_ratio = 1\npreview_ratio = 1\n";
        let out = patch_ui_layout(before, &ui(8, 1, 1)).unwrap();
        assert_eq!(comment_column(&out), comment_column(before), "{out}");
        assert!(out.contains("repos_width = 8"), "{out}");
    }

    /// Which column the `repos_width` line's comment starts in.
    fn comment_column(text: &str) -> Option<usize> {
        text.lines()
            .find(|l| l.trim_start().starts_with("repos_width"))
            .and_then(|l| l.find('#'))
    }

    #[test]
    fn a_number_too_wide_to_pay_for_still_leaves_one_space() {
        // One space of gap, five digits wanted: the comment has to give way, but
        // never so far that it runs into the value.
        let out = patch_ui_layout("[ui]\nrepos_width = 1 # c\n", &ui(65535, 1, 1)).unwrap();
        assert!(out.contains("repos_width = 65535 # c"), "{out}");
    }

    #[test]
    fn a_value_with_no_space_before_its_comment_gains_none() {
        let out = patch_ui_layout("[ui]\nrepos_width = 28# c\n", &ui(30, 1, 1)).unwrap();
        assert!(out.contains("repos_width = 30# c"), "{out}");
    }

    #[test]
    fn preview_ratio_zero_is_written_as_zero() {
        // The hide-the-pane case, and the value most likely to be mistaken for
        // "nothing to do".
        let out = patch_ui_layout("[ui]\npreview_ratio = 1\n", &ui(28, 1, 0)).unwrap();
        assert!(out.contains("preview_ratio = 0"), "{out}");
    }

    // ── adding what isn't there ──────────────────────────────────────────

    #[test]
    fn a_key_the_table_hasnt_got_is_added_to_it() {
        let out = patch(TEMPLATE_WITHOUT_RATIOS).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.ui.tree_ratio, 1);
        assert_eq!(cfg.ui.preview_ratio, 1);
        assert_eq!(cfg.ui.repos_width, 28);
    }

    #[test]
    fn a_key_is_added_above_the_blank_line_that_ends_the_table() {
        let out = patch("[ui]\nrepos_width = 28\n\n[profiles.a]\nendpoint = \"x\"\naccess_key_id = \"k\"\nsecret_access_key = \"s\"\n").unwrap();
        let ratio = out.lines().position(|l| l.starts_with("tree_ratio")).unwrap();
        let table = out.lines().position(|l| l == "[profiles.a]").unwrap();
        assert!(ratio < table, "the key landed outside the table:\n{out}");
    }

    #[test]
    fn a_file_with_no_ui_table_gets_one() {
        let out = patch("default_profile = \"local\"\n").unwrap();
        assert!(out.starts_with("default_profile = \"local\"\n"), "{out}");
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.ui.repos_width, 28);
    }

    #[test]
    fn a_file_that_does_not_end_in_a_newline_gains_one_first() {
        let out = patch("default_profile = \"local\"").unwrap();
        assert!(out.contains("\"local\"\n"), "{out}");
        assert!(toml::from_str::<Config>(&out).is_ok(), "{out}");
    }

    #[test]
    fn ui_as_the_last_table_takes_the_new_key_before_the_end() {
        let out = patch("[profiles.a]\nendpoint = \"x\"\naccess_key_id = \"k\"\nsecret_access_key = \"s\"\n\n[ui]\nrepos_width = 28\n").unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!((cfg.ui.tree_ratio, cfg.ui.preview_ratio), (1, 1));
    }

    // ── things it must not touch, and shapes it must decline ─────────────

    #[test]
    fn the_old_column_width_name_is_rewritten_where_it_stands() {
        // Both spellings load as the same field, and serde rejects a field it sees
        // twice — so adding the canonical name beside this one would stop the file
        // loading at all.
        let out = patch_ui_layout("[ui]\ncolumn_width = 28\n", &ui(40, 1, 1)).unwrap();
        assert!(out.contains("column_width = 40"), "{out}");
        assert!(!out.contains("repos_width"), "both spellings present:\n{out}");
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.ui.repos_width, 40);
    }

    #[test]
    fn a_repos_width_in_a_later_table_is_left_alone() {
        let before = "[ui]\nrepos_width = 28\ntree_ratio = 1\npreview_ratio = 1\n\n[other]\nrepos_width = 99\n";
        let out = patch_ui_layout(before, &ui(40, 1, 1)).unwrap();
        assert!(out.contains("repos_width = 99"), "{out}");
        assert!(out.contains("repos_width = 40"), "{out}");
    }

    #[test]
    fn crlf_line_endings_survive() {
        let before = "[ui]\r\nrepos_width = 28\r\ntree_ratio = 1\r\npreview_ratio = 1\r\n";
        let out = patch_ui_layout(before, &ui(40, 1, 1)).unwrap();
        assert!(
            out.match_indices('\n').all(|(i, _)| out[..i].ends_with('\r')),
            "a bare newline crept in:\n{out:?}"
        );
    }

    #[test]
    fn a_ui_given_as_an_inline_table_is_declined() {
        // A `[ui]` header beside this would define `ui` twice, which is a parse
        // error — the file would stop loading rather than merely look odd.
        assert!(patch("ui = { repos_width = 28 }\n").is_none());
    }

    #[test]
    fn a_dotted_ui_key_is_declined() {
        assert!(patch("ui.repos_width = 28\n").is_none());
    }

    #[test]
    fn a_quoted_key_is_declined_rather_than_duplicated() {
        assert!(patch("[ui]\n\"repos_width\" = 28\n").is_none());
    }

    #[test]
    fn a_value_that_is_not_a_plain_integer_is_declined() {
        assert!(patch("[ui]\nrepos_width = \"wide\"\n").is_none());
    }

    // ── the two that pin the whole design ────────────────────────────────

    #[test]
    fn the_template_written_with_its_own_values_comes_back_unchanged() {
        // Ties the starter file to the patcher: this fails the day either is
        // edited out of step with the other.
        assert_eq!(
            patch_ui_layout(TEMPLATE, &UiConfig::default()).as_deref(),
            Some(TEMPLATE)
        );
    }

    #[test]
    fn a_secret_reference_is_still_a_reference_afterwards() {
        // The regression test for the hazard this whole approach exists to avoid:
        // the config in memory holds expanded credentials, so anything that
        // serialised it would write the secret here in place of the reference.
        // Asserted on the whole line, since a patcher that rewrote the key or the
        // quoting would be just as wrong as one that expanded the value.
        let out = patch_ui_layout(TEMPLATE, &ui(40, 76, 44)).unwrap();
        let line = "secret_access_key = \"${LAKEFS_SECRET_ACCESS_KEY}\"";
        assert!(TEMPLATE.contains(line), "the fixture has moved on");
        assert!(out.contains(line), "the credential line changed:\n{out}");
    }

    #[test]
    fn a_patched_file_still_loads() {
        let out = patch_ui_layout(TEMPLATE, &ui(41, 76, 44)).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.ui.repos_width, 41);
        assert_eq!(cfg.ui.tree_ratio, 76);
        assert_eq!(cfg.ui.preview_ratio, 44);
        // Everything else in the table came through untouched.
        assert_eq!(cfg.ui.preview_bytes, 65536);
        assert_eq!(cfg.ui.page_size, 500);
    }

    // ── saving ───────────────────────────────────────────────────────────

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lakeview-cfg-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("lakeview.toml")
    }

    /// The starter file with its `${VAR}` secret written inline, so `load` needs
    /// no environment. Setting one would race the other tests in this process.
    fn loadable() -> String {
        TEMPLATE.replace("${LAKEFS_SECRET_ACCESS_KEY}", "inline-secret")
    }

    #[test]
    fn an_empty_path_writes_nothing() {
        // What keeps the test suite from rewriting the developer's own config.
        let cfg = Config::default();
        assert!(cfg.path.as_os_str().is_empty());
        assert!(cfg.save_layout().is_ok());
    }

    #[test]
    fn a_saved_layout_survives_a_reload() {
        let path = scratch("reload");
        std::fs::write(&path, loadable()).unwrap();

        let mut cfg = Config::load(&path).unwrap();
        cfg.ui.repos_width = 41;
        cfg.ui.tree_ratio = 76;
        cfg.ui.preview_ratio = 44;
        cfg.save_layout().unwrap();

        let again = Config::load(&path).unwrap();
        assert_eq!(again.ui.repos_width, 41);
        assert_eq!(again.ui.tree_ratio, 76);
        assert_eq!(again.ui.preview_ratio, 44);
        // And the comment the user is meant to keep is still there — taken from the
        // template rather than spelled out, so editing the template can't make this
        // test wrong about what it is checking.
        let raw = std::fs::read_to_string(&path).unwrap();
        let comment = TEMPLATE
            .lines()
            .find(|l| l.starts_with("repos_width"))
            .and_then(|l| l.split_once('#'))
            .map(|(_, c)| c.trim_end().to_string())
            .expect("the template's repos_width comment");
        assert!(raw.contains(&comment), "lost `#{comment}`:\n{raw}");
    }

    #[test]
    fn saving_a_layout_that_hasnt_changed_leaves_the_file_alone() {
        let path = scratch("unchanged");
        std::fs::write(&path, loadable()).unwrap();
        let cfg = Config::load(&path).unwrap();
        cfg.save_layout().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), loadable());
    }

    #[test]
    #[cfg(unix)]
    fn saving_does_not_change_the_files_mode() {
        use std::os::unix::fs::PermissionsExt;

        let path = scratch("mode");
        std::fs::write(&path, loadable()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let mut cfg = Config::load(&path).unwrap();
        cfg.ui.repos_width = 41;
        cfg.save_layout().unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "the write re-imposed a mode of its own");
    }

    /// The starter file with the two ratio keys taken out, for the add path.
    const TEMPLATE_WITHOUT_RATIOS: &str = "[ui]\nrepos_width = 28        # columns\npage_size = 500\n\n[profiles.local]\nendpoint = \"http://localhost:8000\"\naccess_key_id = \"k\"\nsecret_access_key = \"s\"\n";
}
