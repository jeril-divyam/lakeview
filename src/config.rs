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
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Human-readable label shown in the header; defaults to the profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UiConfig {
    /// Minimum width of a Miller column before older columns get dropped.
    #[serde(default = "default_column_width")]
    pub column_width: u16,
    /// Percentage of the screen given to the preview pane (0 disables it).
    #[serde(default = "default_preview_pct")]
    pub preview_percent: u16,
    /// Maximum number of bytes fetched when previewing an object.
    #[serde(default = "default_preview_bytes")]
    pub preview_bytes: u64,
    /// Entries fetched per API page (lakeFS caps this at 1000).
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Show tags alongside branches in the refs column.
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
fn default_column_width() -> u16 {
    28
}
fn default_preview_pct() -> u16 {
    38
}
fn default_preview_bytes() -> u64 {
    64 * 1024
}
fn default_page_size() -> u32 {
    500
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            column_width: default_column_width(),
            preview_percent: default_preview_pct(),
            preview_bytes: default_preview_bytes(),
            page_size: default_page_size(),
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

/// The starter file written by `lakeview init`.
pub const TEMPLATE: &str = r#"# lakeview configuration — https://docs.lakefs.io
#
# Credentials may be inlined or pulled from the environment with ${VAR}.

default_profile = "local"

[ui]
column_width = 28       # min width of a Miller column
preview_percent = 38    # share of the screen given to the preview pane
preview_bytes = 65536   # max bytes fetched when previewing a file
page_size = 500         # entries fetched per API request
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
