//! Host path staging for session picks: paths the sandboxed app can open, copied or redirected.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use gpui_pilot_protocol::ErrorCode;
use gpui_pilot_session::sandbox::Bind;
use rustix::fs::OFlags;

use crate::error::ToolError;
use crate::launch::LaunchDir;

/// Symlinks followed while resolving one outside path, as the kernel's own limit.
const MAX_LINKS: usize = 40;

/// Maps a session pick's host paths to paths the sandboxed app can open, copying or
/// redirecting where needed.
pub(crate) struct Stage {
    pick_in: PathBuf,
    pick_out: PathBuf,
    /// Paths visible inside, in mount order, and whether the app may write there. Each root
    /// appears as named and, when that differs, canonical, so a symlinked bind is caught too.
    roots: Vec<(PathBuf, bool)>,
    /// Folders a copy may not contain: every root and the pick dirs' parent.
    guarded: Vec<PathBuf>,
    calls: u64,
}

/// What one requested path becomes before anything is copied or created.
enum Plan {
    Pass(PathBuf),
    Copy(PathBuf, OsString),
    Redirect(OsString),
}

impl Stage {
    /// `binds` are the session section's specs in config syntax (`path` or `path:rw`).
    pub fn new(dir: &LaunchDir, binds: &[String]) -> Stage {
        let both = |path: PathBuf| {
            let canonical = fs::canonicalize(&path).ok().filter(|real| *real != path);
            [Some(path), canonical].into_iter().flatten()
        };
        let mounts = [(dir.path().to_owned(), true)]
            .into_iter()
            .chain(binds.iter().map(|spec| {
                let bind = Bind::parse(spec);
                (bind.dest, bind.writable)
            }))
            .chain([(dir.pick_in(), false), (dir.pick_out(), true)]);
        let roots: Vec<(PathBuf, bool)> = mounts
            .flat_map(|(path, writable)| both(path).map(move |p| (p, writable)))
            .collect();
        let pick_parent = dir.pick_in().parent().map(Path::to_owned);
        let guarded = roots
            .iter()
            .map(|(root, _)| root.clone())
            .chain(pick_parent.into_iter().flat_map(both))
            .collect();
        Stage {
            pick_in: dir.pick_in(),
            pick_out: dir.pick_out(),
            roots,
            guarded,
            calls: 0,
        }
    }

    /// Continues `kept`'s pick numbering, so a relaunch on kept data never reuses `in/<n>`.
    pub fn continue_from(mut self, kept: &Stage) -> Stage {
        self.calls = kept.calls;
        self
    }

    /// One pick call: every path mapped. Each call takes the next number `<n>` (from 1), even
    /// one that copies nothing. `InvalidArgument` for a relative path or one holding `..`, a
    /// missing parent, an outside path that resolves through a path the sandbox sees, a
    /// folder holding such a path or the pick dirs, a symlink or special file inside a copied
    /// directory, or two inputs with the same name.
    pub fn map(&mut self, paths: &[PathBuf]) -> Result<Vec<PathBuf>, ToolError> {
        self.calls += 1;
        let n = self.calls.to_string();
        // Plan every path first, so a bad path or a name clash fails before anything is copied.
        let plans = paths
            .iter()
            .map(|path| self.plan(path))
            .collect::<Result<Vec<_>, _>>()?;
        let mut names = BTreeSet::new();
        for plan in &plans {
            if let Plan::Copy(_, name) | Plan::Redirect(name) = plan {
                let into_in = matches!(plan, Plan::Copy(..));
                if !names.insert((into_in, name.clone())) {
                    return Err(invalid(format!(
                        "two picked paths share the file name {}",
                        name.to_string_lossy()
                    )));
                }
            }
        }
        // pick/out is writable inside, so its per-call dir is made with mkdir, which fails
        // rather than following anything the app planted there.
        let mut made_in = once_dir(self.pick_in.join(&n));
        let mut made_out = once_dir(self.pick_out.join(&n));
        plans
            .into_iter()
            .map(|plan| match plan {
                Plan::Pass(path) => Ok(path),
                Plan::Copy(source, name) => {
                    let dest = made_in()?.join(name);
                    copy_tree(&source, &dest)?;
                    Ok(dest)
                }
                Plan::Redirect(name) => Ok(made_out()?.join(name)),
            })
            .collect()
    }

    /// A path under a root the sandbox sees passes through as given, unresolved, since it
    /// resolves inside; a save target there is redirected when its root is read-only. Any
    /// other path is resolved on the host: an existing one is copied, a missing one redirected.
    fn plan(&self, path: &Path) -> Result<Plan, ToolError> {
        if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
            return Err(invalid(format!(
                "{} must be absolute without `..`",
                path.display()
            )));
        }
        let name = path
            .file_name()
            .map(OsString::from)
            .ok_or_else(|| invalid(format!("{} has no file name", path.display())))?;
        match self.root_of(path) {
            Some(true) => return Ok(Plan::Pass(path.to_owned())),
            // A read-only root's contents are out of the app's reach, so stat them on the host.
            Some(false) if fs::symlink_metadata(path).is_ok() => {
                return Ok(Plan::Pass(path.to_owned()));
            }
            Some(false) => return Ok(Plan::Redirect(name)),
            None => {}
        }
        if let Some(real) = self.resolve(path)? {
            let meta = fs::symlink_metadata(&real).map_err(|e| io_error(&real, e))?;
            if !meta.is_file() && !meta.is_dir() {
                return Err(invalid(format!(
                    "{} is neither a file nor a directory",
                    path.display()
                )));
            }
            // Copying a folder that holds a root would walk dirs the app can change, and
            // one that holds the pick dirs would copy into itself without end.
            if let Some(inner) = self.guarded.iter().find(|g| g.starts_with(&real)) {
                return Err(invalid(format!(
                    "{} holds {}, which cannot be copied",
                    path.display(),
                    inner.display()
                )));
            }
            return Ok(Plan::Copy(real, name));
        }
        match path
            .parent()
            .map(|parent| self.resolve(parent))
            .transpose()?
        {
            Some(Some(parent)) if parent.is_dir() => Ok(Plan::Redirect(name)),
            _ => Err(invalid(format!(
                "the folder of {} does not exist",
                path.display()
            ))),
        }
    }

    /// Whether the root holding `path` is writable, or `None` when no root does. The last
    /// mount over a path is the one the app sees, so the last matching root decides.
    fn root_of(&self, path: &Path) -> Option<bool> {
        self.roots
            .iter()
            .rev()
            .find(|(root, _)| path.starts_with(root))
            .map(|(_, writable)| *writable)
    }

    /// Resolves an outside path one component at a time, `None` when it does not exist.
    /// Stepping into a root is refused: the app controls what is there, so following on
    /// the host could read a file the app pointed anywhere.
    fn resolve(&self, path: &Path) -> Result<Option<PathBuf>, ToolError> {
        let mut pending: Vec<PathBuf> = path
            .components()
            .rev()
            .map(|c| PathBuf::from(c.as_os_str()))
            .collect();
        let mut real = PathBuf::from("/");
        let mut links = 0;
        while let Some(part) = pending.pop() {
            match part.components().next() {
                Some(Component::RootDir) => real = PathBuf::from("/"),
                Some(Component::ParentDir) => {
                    real.pop();
                }
                Some(Component::Normal(step)) => {
                    let next = real.join(step);
                    if self.root_of(&next).is_some() {
                        return Err(invalid(format!(
                            "{} resolves through {}, which the app can change",
                            path.display(),
                            next.display()
                        )));
                    }
                    match fs::symlink_metadata(&next) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            links += 1;
                            if links > MAX_LINKS {
                                return Err(invalid(format!(
                                    "{} has too many symlinks",
                                    path.display()
                                )));
                            }
                            let target = fs::read_link(&next).map_err(|e| io_error(&next, e))?;
                            pending.extend(
                                target
                                    .components()
                                    .rev()
                                    .map(|c| PathBuf::from(c.as_os_str())),
                            );
                        }
                        Ok(_) => real = next,
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::NotFound | ErrorKind::NotADirectory
                            ) =>
                        {
                            return Ok(None);
                        }
                        Err(e) => return Err(io_error(&next, e)),
                    }
                }
                _ => {}
            }
        }
        Ok(Some(real))
    }
}

/// A getter that makes `dir` with a plain mkdir on first use and returns it after.
fn once_dir(dir: PathBuf) -> impl FnMut() -> Result<PathBuf, ToolError> {
    let mut made = false;
    move || {
        if !made {
            fs::create_dir(&dir).map_err(|e| io_error(&dir, e))?;
            made = true;
        }
        Ok(dir.clone())
    }
}

/// Copies a regular file, or a directory holding only directories and regular files.
/// Nothing is followed: entries are inspected without following links, sources are opened
/// with `O_NOFOLLOW`, and every destination must not exist yet.
fn copy_tree(source: &Path, dest: &Path) -> Result<(), ToolError> {
    let meta = fs::symlink_metadata(source).map_err(|e| io_error(source, e))?;
    let nofollow = OFlags::NOFOLLOW.bits() as i32;
    if meta.is_file() {
        let mut from = OpenOptions::new()
            .read(true)
            .custom_flags(nofollow)
            .open(source)
            .map_err(|e| io_error(source, e))?;
        let mut to: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(nofollow)
            .open(dest)
            .map_err(|e| io_error(dest, e))?;
        std::io::copy(&mut from, &mut to).map_err(|e| io_error(source, e))?;
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(invalid(format!(
            "{} is a symlink or special file; only files and directories are copied",
            source.display()
        )));
    }
    fs::create_dir(dest).map_err(|e| io_error(dest, e))?;
    for entry in fs::read_dir(source).map_err(|e| io_error(source, e))? {
        let entry = entry.map_err(|e| io_error(source, e))?;
        copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
    }
    Ok(())
}

fn invalid(message: String) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, message)
}

fn io_error(path: &Path, error: std::io::Error) -> ToolError {
    ToolError::new(
        ErrorCode::Internal,
        format!("staging {}: {error}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::LaunchDir;
    use crate::launch::tests::short_temp;
    use gpui_pilot_protocol::ErrorCode;

    fn stage(binds: &[&str]) -> (Stage, LaunchDir, tempfile::TempDir) {
        let parent = short_temp();
        let dir = LaunchDir::create(parent.path()).unwrap();
        std::fs::create_dir_all(dir.pick_in()).unwrap();
        std::fs::create_dir_all(dir.pick_out()).unwrap();
        let binds: Vec<String> = binds.iter().map(|b| b.to_string()).collect();
        (Stage::new(&dir, &binds), dir, parent)
    }

    #[test]
    fn an_outside_file_is_copied_into_the_input_dir_once_per_call() {
        let (mut stage, dir, _parent) = stage(&[]);
        let outside = short_temp();
        let file = outside.path().join("notes.txt");
        std::fs::write(&file, "hello").unwrap();
        let mapped = stage.map(std::slice::from_ref(&file)).unwrap();
        assert_eq!(mapped, vec![dir.pick_in().join("1/notes.txt")]);
        assert_eq!(std::fs::read_to_string(&mapped[0]).unwrap(), "hello");
        assert_eq!(
            stage.map(&[file]).unwrap(),
            vec![dir.pick_in().join("2/notes.txt")]
        );
    }

    #[test]
    fn a_directory_is_copied_whole_and_a_symlink_inside_is_refused() {
        let (mut stage, dir, _parent) = stage(&[]);
        let outside = short_temp();
        let collection = outside.path().join("demo");
        std::fs::create_dir_all(collection.join("users")).unwrap();
        std::fs::write(collection.join("opencollection.yml"), "x").unwrap();
        std::fs::write(collection.join("users/folder.yml"), "y").unwrap();
        let mapped = stage.map(std::slice::from_ref(&collection)).unwrap();
        assert_eq!(mapped, vec![dir.pick_in().join("1/demo")]);
        assert_eq!(
            std::fs::read_to_string(mapped[0].join("users/folder.yml")).unwrap(),
            "y"
        );
        std::os::unix::fs::symlink("/etc/hostname", collection.join("link")).unwrap();
        let error = stage.map(&[collection]).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(
            error.error.message.contains("link"),
            "{}",
            error.error.message
        );
    }

    #[test]
    fn paths_the_sandbox_already_sees_pass_through() {
        let shared = short_temp();
        let spec = shared.path().display().to_string();
        let (mut stage, dir, _parent) = stage(&[spec.as_str()]);
        let in_data = dir.data().join("a.txt");
        std::fs::write(&in_data, "a").unwrap();
        let in_bind = shared.path().join("b.txt");
        std::fs::write(&in_bind, "b").unwrap();
        assert_eq!(
            stage.map(&[in_data.clone(), in_bind.clone()]).unwrap(),
            vec![in_data, in_bind]
        );
    }

    #[test]
    fn a_save_target_is_redirected_unless_its_dir_is_writable_inside() {
        let rw = short_temp();
        let ro = short_temp();
        let specs = [
            format!("{}:rw", rw.path().display()),
            ro.path().display().to_string(),
        ];
        let (mut stage, dir, _parent) = stage(&[specs[0].as_str(), specs[1].as_str()]);
        let in_rw = rw.path().join("out.json");
        assert_eq!(
            stage.map(std::slice::from_ref(&in_rw)).unwrap(),
            vec![in_rw]
        );
        let in_data = dir.data().join("out.json");
        assert_eq!(
            stage.map(std::slice::from_ref(&in_data)).unwrap(),
            vec![in_data]
        );
        assert_eq!(
            stage.map(&[ro.path().join("out.json")]).unwrap(),
            vec![dir.pick_out().join("3/out.json")]
        );
        let outside = short_temp();
        assert_eq!(
            stage.map(&[outside.path().join("x.json")]).unwrap(),
            vec![dir.pick_out().join("4/x.json")]
        );
        assert!(
            dir.pick_out().join("4").is_dir(),
            "the app can write into the redirect's dir"
        );
        assert_eq!(
            stage.map(&[dir.pick_in().join("new.txt")]).unwrap(),
            vec![dir.pick_out().join("5/new.txt")],
            "the input dir is read-only inside"
        );
    }

    #[test]
    fn the_pick_dirs_live_outside_the_writable_launch_dir() {
        let (_stage, dir, _parent) = stage(&[]);
        for pick in [dir.pick_in(), dir.pick_out()] {
            assert!(!pick.starts_with(dir.path()), "{}", pick.display());
            assert!(pick.is_dir(), "{}", pick.display());
        }
    }

    #[test]
    fn a_link_planted_under_a_visible_root_passes_through_unresolved() {
        let (mut stage, dir, _parent) = stage(&[]);
        let outside = short_temp();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "s").unwrap();
        let planted = dir.data().join("foo");
        std::os::unix::fs::symlink(&secret, &planted).unwrap();
        let parent_link = dir.data().join("up");
        std::os::unix::fs::symlink(outside.path(), &parent_link).unwrap();
        let through_parent = parent_link.join("secret.txt");
        assert_eq!(
            stage
                .map(&[planted.clone(), through_parent.clone()])
                .unwrap(),
            vec![planted, through_parent]
        );
        assert!(!dir.pick_in().join("1").exists(), "nothing is copied");
    }

    #[test]
    fn an_outside_path_that_resolves_through_a_root_is_refused() {
        let (mut stage, dir, _parent) = stage(&[]);
        std::fs::write(dir.data().join("app.txt"), "a").unwrap();
        let outside = short_temp();
        let link = outside.path().join("into-launch");
        std::os::unix::fs::symlink(dir.data(), &link).unwrap();
        for path in [link.join("app.txt"), link.join("new.txt")] {
            let error = stage.map(&[path]).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
        }
        let error = stage.map(&[dir.data().join("../owner")]).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn the_server_never_writes_through_a_link_planted_in_the_output_dir() {
        let (mut stage, dir, _parent) = stage(&[]);
        let victim = short_temp();
        std::os::unix::fs::symlink(victim.path(), dir.pick_out().join("1")).unwrap();
        let outside = short_temp();
        assert!(stage.map(&[outside.path().join("x.json")]).is_err());
        assert_eq!(std::fs::read_dir(victim.path()).unwrap().count(), 0);
        assert!(
            dir.pick_out()
                .join("1")
                .symlink_metadata()
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn a_bind_named_through_a_symlink_is_seen_at_its_real_path_too() {
        let real = short_temp();
        let aliases = short_temp();
        let alias = aliases.path().join("alias");
        std::os::unix::fs::symlink(real.path(), &alias).unwrap();
        let spec = format!("{}:rw", alias.display());
        let (mut stage, dir, _parent) = stage(&[spec.as_str()]);
        let outside = short_temp();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "s").unwrap();
        let planted = real.path().join("foo");
        std::os::unix::fs::symlink(&secret, &planted).unwrap();
        assert_eq!(
            stage.map(std::slice::from_ref(&planted)).unwrap(),
            vec![planted]
        );
        assert!(!dir.pick_in().join("1").exists(), "nothing is copied");
        let link = outside.path().join("into-real");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let error = stage.map(&[link.join("foo")]).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn a_directory_holding_a_root_or_the_pick_dirs_is_refused() {
        let (mut stage, dir, parent) = stage(&[]);
        let pick_parent = dir.pick_in().parent().unwrap().to_owned();
        let pilot_root = dir.path().parent().unwrap().to_owned();
        for ancestor in [parent.path().to_owned(), pilot_root, pick_parent] {
            let error = stage.map(std::slice::from_ref(&ancestor)).unwrap_err();
            assert_eq!(
                error.code(),
                ErrorCode::InvalidArgument,
                "{}",
                ancestor.display()
            );
        }
        assert_eq!(std::fs::read_dir(dir.pick_in()).unwrap().count(), 0);
    }

    #[test]
    fn a_missing_parent_and_clashing_names_are_invalid() {
        let (mut stage, _dir, _parent) = stage(&[]);
        let error = stage
            .map(&[PathBuf::from("/nonexistent-gp/dir/x.txt")])
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        let (a, b) = (short_temp(), short_temp());
        for d in [&a, &b] {
            std::fs::write(d.path().join("same.txt"), "s").unwrap();
        }
        let error = stage
            .map(&[a.path().join("same.txt"), b.path().join("same.txt")])
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(
            error.error.message.contains("same.txt"),
            "{}",
            error.error.message
        );
    }

    #[test]
    fn a_continued_stage_takes_the_next_pick_number() {
        let (mut first, dir, _parent) = stage(&[]);
        let outside = short_temp();
        let file = outside.path().join("notes.txt");
        std::fs::write(&file, "hello").unwrap();
        first.map(std::slice::from_ref(&file)).unwrap();
        let mut second = Stage::new(&dir, &[]).continue_from(&first);
        assert_eq!(
            second.map(&[file]).unwrap(),
            vec![dir.pick_in().join("2/notes.txt")]
        );
    }
}
