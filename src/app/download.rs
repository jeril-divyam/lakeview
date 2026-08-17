//! Downloading the selected object into the working directory.

use std::path::Path;

use anyhow::{Result, bail};

use super::{App, Focus, Msg, fmt_err};

impl App {
    /// `d` — download the selected object into the working directory.
    ///
    /// The whole object, not the `preview_bytes` the preview settles for, and
    /// streamed to disk rather than held in memory. Nothing is dropped for being
    /// stale: the fetch is a side effect that outlives the selection that started
    /// it, and reports where it landed whenever it finishes.
    pub fn download_selected(&mut self) {
        let Some((repo, reference)) = self.context().map(|(r, f)| (r.to_string(), f.to_string()))
        else {
            self.set_status("open a repository and ref first", true);
            return;
        };
        // The preview is reading whatever the tree is on, so `d` there means the
        // same file it does one pane to the left.
        if !matches!(self.focus, Focus::Tree | Focus::Preview) {
            self.set_status("select a file in the tree to download", true);
            return;
        }
        let Some(node) = self.tree.selected() else {
            self.set_status("nothing selected", true);
            return;
        };
        // A prefix is not one object; fetching a whole subtree is its own
        // feature, and silently downloading nothing would be worse than saying so.
        if node.is_dir() {
            self.set_status(
                format!("{}/ is a directory — d downloads a file", node.name),
                true,
            );
            return;
        }
        let (path, name) = (node.stat.path.clone(), node.name.clone());

        self.inflight += 1;
        self.set_status(format!("downloading {name}…"), false);

        let (tx, client) = (self.tx.clone(), self.client.clone());
        tokio::spawn(async move {
            let res = async {
                let (mut file, dest) = create_download_file(Path::new("."), &name).await?;
                let bytes = client
                    .download_object(&repo, &reference, &path, &mut file)
                    .await?;
                Ok((dest, bytes))
            }
            .await
            .map_err(fmt_err);
            let _ = tx.send(Msg::Download(res));
        });
    }
}

/// A lakeFS object name reduced to something safe to create in the working
/// directory. The name is already a single path segment, so this is insurance
/// against a server that answers with something stranger — never a path.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    // `.`, `..` and the empty string all name something other than a new file.
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
        "download".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Create a file in `dir` to download into, never clobbering one already there:
/// `report.csv`, then `report (1).csv`, and so on.
///
/// Creation is exclusive, so the name is claimed by the same call that tests it
/// — two downloads racing for one name land on different files rather than
/// interleaving into a single corrupt one.
async fn create_download_file(dir: &Path, name: &str) -> Result<(tokio::fs::File, String)> {
    let name = safe_name(name);
    let path = Path::new(&name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.clone());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for n in 0..1000 {
        let candidate = if n == 0 {
            name.clone()
        } else {
            format!("{stem} ({n}){ext}")
        };
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&candidate))
            .await
        {
            Ok(file) => return Ok((file, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("creating {candidate}")));
            }
        }
    }
    bail!("{name}: too many files by that name here already")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{stat, test_app};

    // ── downloads ────────────────────────────────────────────────────────

    /// A scratch directory of its own per test, so the collision cases can't
    /// see each other's files.
    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lakeview-test-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_name_that_is_already_a_plain_filename_is_left_alone() {
        assert_eq!(safe_name("daily_rollup.json"), "daily_rollup.json");
        assert_eq!(safe_name("a b.tar.gz"), "a b.tar.gz");
    }

    #[test]
    fn a_name_that_could_escape_the_directory_cannot() {
        // lakeFS hands back a single segment, so these mean a server doing
        // something strange — none of them may name a path.
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_name(".."), "download");
        assert_eq!(safe_name("."), "download");
        assert_eq!(safe_name("   "), "download");
        assert_eq!(safe_name(""), "download");
    }

    #[tokio::test]
    async fn the_first_download_of_a_name_gets_the_name() {
        let dir = scratch("first");
        let (_file, name) = create_download_file(&dir, "report.csv").await.unwrap();
        assert_eq!(name, "report.csv");
        assert!(dir.join("report.csv").exists());
    }

    #[tokio::test]
    async fn a_second_download_never_overwrites_the_first() {
        let dir = scratch("collide");
        let names = [
            create_download_file(&dir, "report.csv").await.unwrap().1,
            create_download_file(&dir, "report.csv").await.unwrap().1,
            create_download_file(&dir, "report.csv").await.unwrap().1,
        ];
        assert_eq!(names, ["report.csv", "report (1).csv", "report (2).csv"]);
    }

    #[tokio::test]
    async fn an_extensionless_name_still_gets_a_suffix() {
        let dir = scratch("bare");
        assert_eq!(
            create_download_file(&dir, "LICENSE").await.unwrap().1,
            "LICENSE"
        );
        assert_eq!(
            create_download_file(&dir, "LICENSE").await.unwrap().1,
            "LICENSE (1)"
        );
    }

    /// `d` in the preview means the file the preview is reading, not "you are in
    /// the wrong pane". Checked by the complaint it does not make: the fetch
    /// itself writes to the working directory, which is no business of a test.
    #[tokio::test]
    async fn download_is_not_refused_in_the_preview() {
        let mut app = test_app();
        app.tree.key = Some(("repo".into(), "main".into()));
        app.on_msg(Msg::Children {
            generation: app.tree.generation,
            prefix: String::new(),
            res: Ok(vec![stat("data/", true)]),
        });
        app.tree.state.select(Some(0));

        // A directory is refused for being a directory, which is the check just
        // past the focus one — so reaching it is the focus guard letting us by.
        app.focus = Focus::Preview;
        app.download_selected();
        let refused = &app.status.as_ref().expect("`d` said nothing").text;
        assert!(refused.contains("directory"), "{refused}");

        // Pane one has no file selected at all, and is still turned away.
        app.focus = Focus::Repos;
        app.download_selected();
        let refused = &app.status.as_ref().unwrap().text;
        assert!(refused.contains("select a file in the tree"), "{refused}");
    }
}
