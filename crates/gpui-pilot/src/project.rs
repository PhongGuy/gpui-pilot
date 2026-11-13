//! `launch`'s `project_dir`: the server root, or a git worktree of the root's repository.

use std::path::{Path, PathBuf};

use gpui_pilot_protocol::ErrorCode;

use crate::config::CONFIG_FILE;
use crate::error::ToolError;

/// The project folder `candidate` names: `root` itself (returned as given), or the canonical
/// path of a git worktree of the root's repository holding a `gpui-pilot.toml`. Any other
/// folder is `InvalidArgument` naming why. Reads git's files directly and never runs git.
pub(crate) fn resolve(root: &Path, candidate: &str) -> Result<PathBuf, ToolError> {
    let invalid = |why: String| {
        ToolError::new(
            ErrorCode::InvalidArgument,
            format!("`project_dir` {candidate}: {why}"),
        )
        .with("field", "project_dir")
    };
    if !Path::new(candidate).is_absolute() {
        return Err(invalid("the path must be absolute".into()));
    }
    let dir = std::fs::canonicalize(candidate)
        .map_err(|e| invalid(format!("cannot resolve the folder: {e}")))?;
    if !dir.join(CONFIG_FILE).is_file() {
        return Err(invalid(format!("the folder holds no {CONFIG_FILE}")));
    }
    let real_root = std::fs::canonicalize(root)
        .map_err(|e| invalid(format!("cannot resolve the server's folder: {e}")))?;
    if dir == real_root {
        return Ok(root.to_path_buf());
    }
    let root_common = common_dir(&real_root).ok_or_else(|| {
        invalid("the server's folder is not in a git repository, so only it is accepted".into())
    })?;
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return Err(invalid(
            "the folder has its own repository; pass a git worktree of the server's project".into(),
        ));
    }
    let not_worktree =
        || invalid("the folder is not a git worktree of the server's project".into());
    let git_dir = link(&dir, &dot_git).ok_or_else(not_worktree)?;
    // A worktree's git dir names the shared one; a submodule's or a whole repository's does not.
    let common = link(&git_dir, &git_dir.join("commondir")).ok_or_else(|| {
        invalid("its git dir is a submodule's or a repository's own, not a worktree".into())
    })?;
    if common != root_common {
        return Err(invalid(
            "it is a worktree of another repository, not the server's project".into(),
        ));
    }
    // A forged `.git` file may name a real worktree's git dir; that dir names only its own.
    let back = link(&git_dir, &git_dir.join("gitdir"));
    if back.as_deref() != Some(dot_git.as_path()) {
        return Err(invalid(
            "its git dir does not point back at this folder's `.git`".into(),
        ));
    }
    Ok(dir)
}

/// The repository's common git dir for a work tree at `dir`: its `.git` folder, or for a
/// worktree (or submodule) the dir its `.git` file names, followed through `commondir`.
fn common_dir(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return std::fs::canonicalize(dot_git).ok();
    }
    let git_dir = link(dir, &dot_git)?;
    link(&git_dir, &git_dir.join("commondir")).or(Some(git_dir))
}

/// The canonical path a git link file names, relative to `base` when relative. A `.git` file
/// reads `gitdir: <path>`; `commondir` and `gitdir` inside a git dir hold the bare path.
fn link(base: &Path, file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let line = text.lines().next()?;
    let path = line.strip_prefix("gitdir: ").unwrap_or(line).trim_end();
    if path.is_empty() {
        return None;
    }
    std::fs::canonicalize(base.join(path)).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    /// Runs git in `dir` without the user's or the system's config.
    pub(crate) fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c"])
            .arg("protocol.file.allow=always")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} in {}", dir.display());
    }

    /// A repository at `dir` with one commit.
    pub(crate) fn repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
    }

    /// A worktree of the repository at `repo` added at `at`, holding a config file.
    pub(crate) fn worktree(repo: &Path, at: &Path, branch: &str) {
        git(
            repo,
            &["worktree", "add", "-q", "-b", branch, at.to_str().unwrap()],
        );
        std::fs::write(at.join(CONFIG_FILE), "").unwrap();
    }

    /// A temp dir holding `root`, a repository with a config file.
    fn rooted() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("root");
        repo(&root);
        std::fs::write(root.join(CONFIG_FILE), "").unwrap();
        (temp, root)
    }

    fn refused(root: &Path, candidate: &Path, reason: &str) {
        let error = resolve(root, candidate.to_str().unwrap()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.details["field"], serde_json::json!("project_dir"));
        assert!(
            error.error.message.contains(reason),
            "{reason:?} not in {:?}",
            error.error.message
        );
    }

    #[test]
    fn the_root_itself_is_accepted_as_the_root_path() {
        let (_temp, root) = rooted();
        let spelled = format!("{}/./", root.display());
        assert_eq!(resolve(&root, &spelled).unwrap(), root);
    }

    #[test]
    fn a_worktree_of_the_root_repository_is_accepted_canonical() {
        let (temp, root) = rooted();
        let tree = temp.path().canonicalize().unwrap().join("wt");
        worktree(&root, &tree, "wt");
        assert_eq!(resolve(&root, tree.to_str().unwrap()).unwrap(), tree);
    }

    #[test]
    fn a_sibling_worktree_is_accepted_when_the_root_is_itself_a_worktree() {
        let (temp, main) = rooted();
        let base = temp.path().canonicalize().unwrap();
        let (root, sibling) = (base.join("a"), base.join("b"));
        worktree(&main, &root, "a");
        worktree(&main, &sibling, "b");
        assert_eq!(resolve(&root, sibling.to_str().unwrap()).unwrap(), sibling);
    }

    #[test]
    fn a_worktree_with_relative_git_links_is_accepted() {
        let (temp, root) = rooted();
        let tree = temp.path().canonicalize().unwrap().join("rel");
        git(
            &root,
            &[
                "-c",
                "worktree.useRelativePaths=true",
                "worktree",
                "add",
                "-q",
                "-b",
                "rel",
                tree.to_str().unwrap(),
            ],
        );
        std::fs::write(tree.join(CONFIG_FILE), "").unwrap();
        assert_eq!(resolve(&root, tree.to_str().unwrap()).unwrap(), tree);
    }

    #[test]
    fn a_relative_path_is_refused() {
        let (_temp, root) = rooted();
        refused(&root, Path::new("root"), "absolute");
    }

    #[test]
    fn a_missing_folder_is_refused() {
        let (temp, root) = rooted();
        refused(&root, &temp.path().join("nowhere"), "cannot resolve");
    }

    #[test]
    fn a_worktree_without_a_config_file_is_refused() {
        let (temp, root) = rooted();
        let tree = temp.path().canonicalize().unwrap().join("wt");
        worktree(&root, &tree, "wt");
        std::fs::remove_file(tree.join(CONFIG_FILE)).unwrap();
        refused(&root, &tree, CONFIG_FILE);
    }

    #[test]
    fn a_folder_that_is_not_a_worktree_is_refused() {
        let (temp, root) = rooted();
        let plain = temp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join(CONFIG_FILE), "").unwrap();
        refused(&root, &plain, "not a git worktree");
        // A subfolder of the root is in the repository but is not a worktree of it.
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join(CONFIG_FILE), "").unwrap();
        refused(&root, &sub, "not a git worktree");
    }

    #[test]
    fn a_separate_repository_is_refused() {
        let (temp, root) = rooted();
        let other = temp.path().join("other");
        repo(&other);
        std::fs::write(other.join(CONFIG_FILE), "").unwrap();
        refused(&root, &other, "own repository");
    }

    #[test]
    fn another_repositorys_worktree_is_refused() {
        let (temp, root) = rooted();
        let other = temp.path().join("other");
        repo(&other);
        let tree = temp.path().join("other-wt");
        worktree(&other, &tree, "wt");
        refused(&root, &tree, "another repository");
    }

    #[test]
    fn a_submodule_is_refused() {
        let (temp, root) = rooted();
        let other = temp.path().join("other");
        repo(&other);
        git(
            &root,
            &["submodule", "add", "-q", other.to_str().unwrap(), "sub"],
        );
        let sub = root.join("sub");
        std::fs::write(sub.join(CONFIG_FILE), "").unwrap();
        refused(&root, &sub, "not a worktree");
    }

    #[test]
    fn a_forged_git_file_naming_a_real_worktree_is_refused() {
        let (temp, root) = rooted();
        let tree = temp.path().canonicalize().unwrap().join("wt");
        worktree(&root, &tree, "wt");
        let forged = temp.path().join("forged");
        std::fs::create_dir(&forged).unwrap();
        std::fs::write(forged.join(CONFIG_FILE), "").unwrap();
        let link = std::fs::read_to_string(tree.join(".git")).unwrap();
        std::fs::write(forged.join(".git"), link).unwrap();
        refused(&root, &forged, "does not point back");
    }

    #[test]
    fn a_non_root_folder_is_refused_when_the_root_is_not_in_git() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let (_other_temp, other_root) = rooted();
        let tree = other_root.parent().unwrap().join("wt");
        worktree(&other_root, &tree, "wt");
        refused(&root, &tree, "not in a git repository");
    }
}
