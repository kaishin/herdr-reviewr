//! Branch-and-cwd comment files.
//!
//! Comments are JSON on disk for one worktree and one checked-out branch. Send and copy
//! leave the file in place; only `clear` deletes it. A missing file, a detached `HEAD`,
//! or a file this version cannot read restores nothing.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::git;
use crate::model::{Comment, CommentStore, CommitPick, Rev, Side};

const FILE_NAME: &str = "comments.json";

/// Where this review's comments live, or `None` when `HEAD` is detached.
pub fn path_for(repo: &Path) -> Result<Option<PathBuf>> {
    let Some(branch) = git::checked_out_branch(repo).map_err(|e| anyhow::anyhow!(e.0))? else {
        return Ok(None);
    };
    Ok(Some(file_path(repo, &branch)))
}

fn file_path(repo: &Path, branch: &str) -> PathBuf {
    comments_dir().join(key(repo, branch)).join(FILE_NAME)
}

fn comments_dir() -> PathBuf {
    crate::search::cache_dir().join("comments")
}

fn key(repo: &Path, branch: &str) -> String {
    format!("{:016x}", hash(&(repo.to_path_buf(), canonical(repo), branch)))
}

fn canonical(repo: &Path) -> PathBuf {
    repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf())
}

/// FNV-1a over the checkout path, its canonical path, and the branch name. The unresolved
/// path keeps two temp checkouts apart when they canonicalize to the same directory.
fn hash(input: &(PathBuf, PathBuf, &str)) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let unresolved = input.0.to_string_lossy().into_owned();
    let resolved = input.1.to_string_lossy().into_owned();
    for byte in unresolved.bytes().chain(resolved.bytes()).chain(input.2.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Read the comment file for the checked-out branch. Missing, detached, or unreadable
/// is an empty store — a bad file must not block the pane.
pub fn load(repo: &Path) -> CommentStore {
    let Ok(Some(path)) = path_for(repo) else {
        return CommentStore::new();
    };
    let Ok(bytes) = fs::read(&path) else {
        return CommentStore::new();
    };
    serde_json::from_slice::<File>(&bytes).map(File::into_store).unwrap_or_default()
}

/// Write `store` to the checked-out branch's file. A detached `HEAD` has no file.
pub fn save(repo: &Path, store: &CommentStore) -> Result<()> {
    let Some(path) = path_for(repo)? else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create comment dir {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(&File::from_store(store))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Delete the checked-out branch's comment file. Missing is success.
pub fn clear(repo: &Path) -> Result<()> {
    let Some(path) = path_for(repo)? else {
        return Ok(());
    };
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("delete {}", path.display())),
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct File {
    comments: Vec<StoredComment>,
}

impl File {
    fn from_store(store: &CommentStore) -> Self {
        Self { comments: store.iter().cloned().map(StoredComment::from).collect() }
    }

    fn into_store(self) -> CommentStore {
        let mut store = CommentStore::new();
        for comment in self.comments {
            store.add(comment.into());
        }
        store
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredComment {
    file: String,
    side: String,
    start: u32,
    end: u32,
    lines: String,
    text: String,
    diff_anchored: bool,
    rev: StoredRev,
}

impl From<Comment> for StoredComment {
    fn from(comment: Comment) -> Self {
        Self {
            file: comment.file,
            side: match comment.side {
                Side::New => "new",
                Side::Old => "old",
            }
            .into(),
            start: comment.start,
            end: comment.end,
            lines: comment.lines,
            text: comment.text,
            diff_anchored: comment.diff_anchored,
            rev: comment.rev.into(),
        }
    }
}

impl From<StoredComment> for Comment {
    fn from(comment: StoredComment) -> Self {
        Self {
            file: comment.file,
            side: if comment.side == "old" { Side::Old } else { Side::New },
            start: comment.start,
            end: comment.end,
            lines: comment.lines,
            text: comment.text,
            diff_anchored: comment.diff_anchored,
            rev: comment.rev.into(),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum StoredRev {
    Worktree,
    Commit { oldest: String, newest: String },
}

impl From<Rev> for StoredRev {
    fn from(rev: Rev) -> Self {
        match rev {
            Rev::Worktree => Self::Worktree,
            Rev::Commit(pick) => Self::Commit { oldest: pick.oldest, newest: pick.newest },
        }
    }
}

impl From<StoredRev> for Rev {
    fn from(rev: StoredRev) -> Self {
        match rev {
            StoredRev::Worktree => Self::Worktree,
            StoredRev::Commit { oldest, newest } => Self::Commit(CommitPick { oldest, newest }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical, file_path, hash, key};
    use crate::model::{Comment, CommentStore, CommitPick, Rev, Side};
    use std::path::Path;

    fn comment(text: &str) -> Comment {
        Comment {
            file: "src/lib.rs".into(),
            side: Side::Old,
            start: 4,
            end: 8,
            lines: "-old".into(),
            text: text.into(),
            diff_anchored: true,
            rev: Rev::Commit(CommitPick::single("abc")),
        }
    }

    #[test]
    fn the_same_branch_in_two_cwds_is_two_files() {
        let a = Path::new("/work/a");
        let b = Path::new("/work/b");
        assert_ne!(key(a, "main"), key(b, "main"));
        assert_ne!(file_path(a, "main"), file_path(b, "main"));
        assert_eq!(key(a, "main"), key(a, "main"));
        assert_ne!(key(a, "main"), key(a, "topic"));
    }

    #[test]
    fn hash_changes_when_the_cwd_or_branch_changes() {
        let repo = Path::new("/work/reviewr");
        let cwd = canonical(repo);
        let other = Path::new("/work/other").to_path_buf();
        assert_ne!(
            hash(&(repo.to_path_buf(), cwd.clone(), "main")),
            hash(&(repo.to_path_buf(), cwd.clone(), "topic"))
        );
        assert_ne!(
            hash(&(repo.to_path_buf(), cwd, "main")),
            hash(&(other.clone(), canonical(&other), "main"))
        );
    }

    fn git(repo: &Path, args: &[&str]) {
        let status =
            std::process::Command::new("git").arg("-C").arg(repo).args(args).status().unwrap();
        assert!(status.success(), "{args:?}");
    }

    #[test]
    fn a_round_trip_keeps_the_comment_until_clear() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "topic"]);
        git(repo, &["config", "user.email", "reviewr@example.com"]);
        git(repo, &["config", "user.name", "reviewr"]);
        std::fs::write(repo.join("f"), "x\n").unwrap();
        git(repo, &["add", "f"]);
        git(repo, &["commit", "-q", "-m", "init"]);

        let mut store = CommentStore::new();
        store.add(comment("keep me"));
        super::save(repo, &store).unwrap();

        let loaded = super::load(repo);
        let got = loaded.get(0).unwrap();
        assert_eq!(got.text, "keep me");
        assert_eq!(got.side, Side::Old);
        assert_eq!(got.rev, Rev::Commit(CommitPick::single("abc")));
        assert!(super::path_for(repo).unwrap().unwrap().is_file());

        super::clear(repo).unwrap();
        assert!(super::load(repo).is_empty());
        assert!(!super::path_for(repo).unwrap().unwrap().exists());
        super::clear(repo).unwrap();
    }
}
