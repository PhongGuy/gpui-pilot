//! `gpui-pilot.toml`: how to build and launch an app's pilot binary.

use crate::error::ToolError;
use gpui_pilot_protocol::{ErrorCode, REFRESH_RATES_HZ};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::FileTypeExt;
use std::path::{Component, Path, PathBuf};

pub(crate) const CONFIG_FILE: &str = "gpui-pilot.toml";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Headless,
    Session,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    headless: Option<ModeConfig>,
    session: Option<ModeConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModeConfig {
    #[serde(default)]
    pub build: Vec<String>,
    pub binary: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub binds: Vec<String>,
    /// Session only: the network a launch gets when it names none.
    #[serde(default)]
    pub network: Option<crate::params::Network>,
    /// The refresh rate a launch gets when it names none, one of `REFRESH_RATES_HZ`.
    #[serde(default)]
    pub refresh_hz: Option<u32>,
}

impl Config {
    /// Reads and validates `root/gpui-pilot.toml`; every failure is `InvalidConfig`.
    pub fn load(root: &Path) -> Result<Config, ToolError> {
        let path = root.join(CONFIG_FILE);
        let invalid = |why: String| ToolError::new(ErrorCode::InvalidConfig, why);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| invalid(format!("cannot read {}: {e}", path.display())))?;
        let mut config: Config = toml::from_str(&text)
            .map_err(|e| invalid(format!("{} is invalid: {e}", path.display())))?;
        let sections = [
            (Mode::Headless, &config.headless),
            (Mode::Session, &config.session),
        ];
        for (mode, section) in sections.iter().filter_map(|(m, s)| Some((*m, s.as_ref()?))) {
            // Headless always shares the host network; session never does.
            match (mode, section.network) {
                (Mode::Headless, Some(_)) => {
                    return Err(invalid(
                        "[headless] network is not configurable; headless runs on the host network"
                            .into(),
                    ));
                }
                (Mode::Session, Some(crate::params::Network::Host)) => {
                    return Err(invalid(
                        "[session] network must be `none` or `pasta`, never `host`".into(),
                    ));
                }
                _ => {}
            }
            if let Some(hz) = section.refresh_hz
                && !REFRESH_RATES_HZ.contains(&hz)
            {
                let name = match mode {
                    Mode::Headless => "headless",
                    Mode::Session => "session",
                };
                return Err(invalid(format!(
                    "[{name}] refresh_hz must be one of {REFRESH_RATES_HZ:?}, not {hz}"
                )));
            }
            // A release build would not carry the pilot code path, so refuse it by any component.
            let is_release = section
                .binary
                .components()
                .any(|c| c == Component::Normal("release".as_ref()));
            if is_release {
                return Err(invalid(format!(
                    "binary {} is a release build; the pilot needs a debug build",
                    section.binary.display()
                )));
            }
        }
        // Relative binds become absolute here, so every later check and the sandbox see one form.
        for spec in [&mut config.headless, &mut config.session]
            .into_iter()
            .flatten()
            .flat_map(|section| &mut section.binds)
        {
            let absolute = absolute_bind(root, spec)
                .and_then(|absolute| check_bind(&absolute).map(|()| absolute))
                .map_err(|why| invalid(format!("bind {spec}: {why}")))?;
            *spec = absolute;
        }
        Ok(config)
    }

    /// The section for `mode`; `InvalidConfig` when the file has none.
    pub fn section(&self, mode: Mode) -> Result<&ModeConfig, ToolError> {
        let (section, name) = match mode {
            Mode::Headless => (&self.headless, "headless"),
            Mode::Session => (&self.session, "session"),
        };
        section.as_ref().ok_or_else(|| {
            ToolError::new(
                ErrorCode::InvalidConfig,
                format!("{CONFIG_FILE} has no [{name}] section"),
            )
        })
    }
}

/// Mounted over the sandbox's tmpfs, a bind at or under one of these would hand the app the
/// user's desktop sockets (bus, Wayland, X11, ICE) or another launch's private files.
const SHADOWED: [&str; 4] = ["/run", "/var/run", "/tmp/.X11-unix", "/tmp/.ICE-unix"];
/// Binding one of these whole would replace the sandbox's own view of it.
const WHOLE: [&str; 5] = ["/", "/tmp", "/proc", "/dev", "/sys"];

/// Refuses a bind (`path` or `path:rw`, mounted at the same place) that is relative, holds
/// `..`, or reaches the desktop session, lexically or through a symlink in the source.
fn check_bind(spec: &str) -> Result<(), String> {
    let path = Path::new(spec.strip_suffix(":rw").unwrap_or(spec));
    if !path.is_absolute() {
        return Err("the path must be absolute".into());
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err("the path must not hold `..`".into());
    }
    // `components` drops `.`, repeated and trailing slashes, which is the lexical normal form.
    let lexical: PathBuf = path.components().collect();
    let pilot_root = format!("/tmp/gpui-pilot-{}", rustix::process::getuid().as_raw());
    let reaches = |path: &Path| {
        WHOLE.iter().any(|whole| path == Path::new(whole))
            || SHADOWED
                .iter()
                .chain([&pilot_root.as_str()])
                .any(|root| path.starts_with(root))
    };
    if reaches(&lexical) || resolved(&lexical).is_some_and(|real| reaches(&real)) {
        return Err("it would expose the desktop session or the sandbox's own files".into());
    }
    Ok(())
}

/// `spec` with a relative path resolved against the project folder `root`; absolute specs pass
/// through. Err when a relative path holds `..` or its real path leaves the project, through a
/// symlink anywhere in it, dangling ones included.
fn absolute_bind(root: &Path, spec: &str) -> Result<String, String> {
    let (path, rw) = match spec.strip_suffix(":rw") {
        Some(path) => (Path::new(path), ":rw"),
        None => (Path::new(spec), ""),
    };
    if path.is_absolute() {
        return Ok(spec.to_owned());
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err("the path must not hold `..`".into());
    }
    let project = std::fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve the project folder: {e}"))?;
    let joined: PathBuf = project.join(path).components().collect();
    // `resolved` skips a dangling symlink as missing, so the shallowest missing ancestor must
    // not be one.
    let dangling = joined
        .ancestors()
        .take_while(|a| !a.exists())
        .last()
        .is_some_and(|missing| missing.symlink_metadata().is_ok());
    if dangling || !resolved(&joined).is_some_and(|real| real.starts_with(&project)) {
        return Err("it leaves the project through a symlink".into());
    }
    let joined = joined
        .to_str()
        .ok_or("the project folder's path is not UTF-8")?;
    Ok(format!("{joined}{rw}"))
}

/// Bind specs (`<arg>:rw`) for each arg that is an absolute path to an existing file or
/// directory; other args yield none. Err names the arg and why when `check_bind` or the
/// stricter agent-arg rules (`check_arg_path`) refuse it.
pub(crate) fn arg_binds(args: &[String]) -> Result<Vec<String>, String> {
    args.iter()
        .filter(|arg| Path::new(arg).is_absolute() && Path::new(arg).exists())
        .map(|arg| {
            let spec = format!("{arg}:rw");
            check_bind(&spec)
                .and_then(|()| check_arg_path(Path::new(arg)))
                .map(|()| spec)
                .map_err(|why| format!("launch arg {arg} cannot be bound: {why}"))
        })
        .collect()
}

/// Kernel interfaces an agent-chosen arg must never bind, lexically or once resolved.
const KERNEL: [&str; 3] = ["/dev", "/proc", "/sys"];

/// Agent args are stricter than config binds: no kernel interfaces, no Unix socket, and no
/// folder holding one directly (`/tmp/tmux-<uid>`, `/tmp/ssh-*`); nested sockets are not scanned.
fn check_arg_path(path: &Path) -> Result<(), String> {
    let kernel = |p: &Path| KERNEL.iter().any(|root| p.starts_with(root));
    let lexical: PathBuf = path.components().collect();
    let real = std::fs::canonicalize(path).map_err(|e| format!("cannot resolve it: {e}"))?;
    if kernel(&lexical) || kernel(&real) {
        return Err("it is a device or kernel interface".into());
    }
    let socket = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.file_type().is_socket());
    if socket(&real) {
        return Err("it is a Unix socket".into());
    }
    if real.is_dir() {
        let entries = std::fs::read_dir(&real).map_err(|e| format!("cannot list it: {e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("cannot list it: {e}"))?;
            if entry.file_type().is_ok_and(|t| t.is_socket()) {
                return Err("it holds a Unix socket".into());
            }
        }
    }
    Ok(())
}

/// `path` with its deepest existing ancestor canonicalized, so a symlink anywhere in it is
/// followed even when the leaf does not exist yet.
fn resolved(path: &Path) -> Option<PathBuf> {
    let existing = path.ancestors().find(|a| a.exists())?;
    let rest = path.strip_prefix(existing).ok()?;
    Some(std::fs::canonicalize(existing).ok()?.join(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> Result<Config, ToolError> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), text).unwrap();
        Config::load(dir.path())
    }

    #[test]
    fn reads_both_sections_with_defaults() {
        let config = load(concat!(
            "[headless]\n",
            "build = [\"cargo\", \"build\", \"--manifest-path\", \"tools/demo-pilot/Cargo.toml\"]\n",
            "binary = \"tools/demo-pilot/target/debug/demo-pilot\"\n",
            "[session]\n",
            "binary = \"target/debug/demo\"\n",
            "binds = [\"/srv/fixtures:rw\"]\n",
        ))
        .unwrap();
        let headless = config.section(Mode::Headless).unwrap();
        assert_eq!(headless.build.len(), 4);
        assert_eq!(
            headless.binary,
            PathBuf::from("tools/demo-pilot/target/debug/demo-pilot")
        );
        assert!(headless.args.is_empty());
        let session = config.section(Mode::Session).unwrap();
        assert!(session.build.is_empty());
        assert_eq!(session.binds, ["/srv/fixtures:rw"]);
    }

    #[test]
    fn session_network_defaults_and_headless_network_is_refused() {
        let config = load("[session]\nbinary = \"b\"\nnetwork = \"pasta\"\n").unwrap();
        assert_eq!(
            config.section(Mode::Session).unwrap().network,
            Some(crate::params::Network::Pasta)
        );
        for text in [
            "[headless]\nbinary = \"b\"\nnetwork = \"none\"\n",
            "[session]\nbinary = \"b\"\nnetwork = \"host\"\n",
        ] {
            assert_eq!(
                load(text).unwrap_err().code(),
                ErrorCode::InvalidConfig,
                "{text}"
            );
        }
    }

    #[test]
    fn refresh_hz_is_one_of_the_measured_rates_per_mode() {
        let config = load("[headless]\nbinary = \"b\"\nrefresh_hz = 60\n[session]\nbinary = \"b\"\nrefresh_hz = 240\n").unwrap();
        assert_eq!(config.section(Mode::Headless).unwrap().refresh_hz, Some(60));
        assert_eq!(config.section(Mode::Session).unwrap().refresh_hz, Some(240));
        let plain = load("[session]\nbinary = \"b\"\n").unwrap();
        assert_eq!(plain.section(Mode::Session).unwrap().refresh_hz, None);
        for bad in [0, 59, 75, 144_000] {
            let error =
                load(&format!("[headless]\nbinary = \"b\"\nrefresh_hz = {bad}\n")).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidConfig, "{bad}");
            assert!(
                error.error.message.contains("refresh_hz"),
                "{bad}: {}",
                error.error.message
            );
        }
    }

    #[test]
    fn a_missing_file_section_or_unknown_key_is_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(dir.path()).unwrap_err().code(),
            ErrorCode::InvalidConfig
        );
        let only_session = load("[session]\nbinary = \"target/debug/demo\"\n").unwrap();
        assert_eq!(
            only_session.section(Mode::Headless).unwrap_err().code(),
            ErrorCode::InvalidConfig
        );
        let typo = load("[headless]\nbinary = \"x\"\nbuidl = []\n");
        assert_eq!(typo.unwrap_err().code(), ErrorCode::InvalidConfig);
    }

    #[test]
    fn a_release_binary_is_refused() {
        for binary in [
            "target/release/demo",
            "/abs/target/x86_64-unknown-linux-gnu/release/demo",
        ] {
            let error = load(&format!("[headless]\nbinary = \"{binary}\"\n")).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidConfig, "{binary}");
            assert!(
                error.error.message.contains("release"),
                "{}",
                error.error.message
            );
        }
        assert!(load("[headless]\nbinary = \"target/debug/release-notes\"\n").is_ok());
    }

    fn bind(spec: &str) -> Result<Config, ToolError> {
        load(&format!(
            "[session]\nbinary = \"target/debug/demo\"\nbinds = [{spec:?}]\n"
        ))
    }

    #[test]
    fn binds_that_reach_the_desktop_session_are_refused_by_name() {
        let uid = rustix::process::getuid().as_raw();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("bus");
        std::os::unix::fs::symlink("/run", &link).unwrap();
        let refused = [
            "/srv/../run/user".to_owned(),
            "/".to_owned(),
            "/tmp".to_owned(),
            "/tmp/".to_owned(),
            "/proc".to_owned(),
            "/dev".to_owned(),
            "/sys".to_owned(),
            "/run".to_owned(),
            format!("/run/user/{uid}"),
            format!("/run/user/{uid}/wayland-0:rw"),
            "/var/run".to_owned(),
            "/var/run/dbus/system_bus_socket".to_owned(),
            "/tmp/.X11-unix".to_owned(),
            "/tmp/.X11-unix/X0".to_owned(),
            "//tmp/./.X11-unix/X0".to_owned(),
            "/tmp/.ICE-unix/1234".to_owned(),
            format!("/tmp/gpui-pilot-{uid}"),
            format!("/tmp/gpui-pilot-{uid}/abc/run"),
            link.display().to_string(),
            format!("{}/user", link.display()),
        ];
        for spec in refused {
            let error = bind(&spec).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidConfig, "{spec}");
            assert!(
                error.error.message.contains(&spec),
                "{}",
                error.error.message
            );
        }
        let config = bind("/srv/fixtures:rw").unwrap();
        assert_eq!(
            config.section(Mode::Session).unwrap().binds,
            ["/srv/fixtures:rw"]
        );
        assert!(bind("/tmp/fixtures").is_ok());
    }

    /// Loads `binds` from a config in a fresh project dir; returns the dir's real path too.
    fn project_binds(
        setup: impl FnOnce(&Path),
        binds: &[&str],
    ) -> (PathBuf, Result<Vec<String>, ToolError>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path());
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            format!("[session]\nbinary = \"b\"\nbinds = {binds:?}\n"),
        )
        .unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let binds =
            Config::load(dir.path()).map(|c| c.section(Mode::Session).unwrap().binds.clone());
        (real, binds, dir)
    }

    #[test]
    fn relative_binds_resolve_against_the_project() {
        let (real, binds, _dir) = project_binds(
            |dir| {
                std::fs::create_dir(dir.join("assets")).unwrap();
                std::os::unix::fs::symlink("assets", dir.join("inside")).unwrap();
            },
            &["assets", "./fixtures/db:rw", "inside"],
        );
        assert_eq!(
            binds.unwrap(),
            [
                format!("{}/assets", real.display()),
                format!("{}/fixtures/db:rw", real.display()),
                format!("{}/inside", real.display()),
            ]
        );
    }

    #[test]
    fn relative_binds_with_parent_dirs_are_refused() {
        for spec in ["../outside", "assets/../../outside", "assets/..:rw"] {
            let (_, binds, _dir) = project_binds(|_| {}, &[spec]);
            let error = binds.unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidConfig, "{spec}");
            assert!(
                error.error.message.contains(spec) && error.error.message.contains("`..`"),
                "{}",
                error.error.message
            );
        }
    }

    #[test]
    fn relative_binds_that_leave_the_project_through_a_symlink_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().to_path_buf();
        for spec in ["out", "out/sub:rw", "run", "run/user", "gone/sub"] {
            let target = target.clone();
            let (_, binds, _dir) = project_binds(
                |dir| {
                    std::os::unix::fs::symlink(&target, dir.join("out")).unwrap();
                    std::os::unix::fs::symlink("/run", dir.join("run")).unwrap();
                    std::os::unix::fs::symlink("/run/gpui-pilot-none", dir.join("gone")).unwrap();
                },
                &[spec],
            );
            let error = binds.unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidConfig, "{spec}");
            assert!(
                error.error.message.contains(spec) && error.error.message.contains("project"),
                "{}",
                error.error.message
            );
        }
    }

    #[test]
    fn arg_binds_take_existing_absolute_paths_and_refuse_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.yml");
        std::fs::write(&file, "x").unwrap();
        let args: Vec<String> = [
            "--flag".to_owned(),
            "relative/path".to_owned(),
            "http://127.0.0.1:1/".to_owned(),
            "/nonexistent/gpui-pilot-p8".to_owned(),
            dir.path().display().to_string(),
            file.display().to_string(),
        ]
        .into();
        assert_eq!(
            arg_binds(&args).unwrap(),
            vec![
                format!("{}:rw", dir.path().display()),
                format!("{}:rw", file.display())
            ]
        );
        let uid = rustix::process::getuid().as_raw();
        let mut refused_paths = vec!["/".to_owned(), "/tmp".to_owned(), "/run".to_owned()];
        let user_run = format!("/run/user/{uid}");
        if Path::new(&user_run).exists() {
            refused_paths.push(user_run);
        }
        for refused in refused_paths {
            let error = arg_binds(std::slice::from_ref(&refused)).unwrap_err();
            assert!(error.contains(&refused), "{error}");
        }
    }

    #[test]
    fn arg_binds_refuse_devices_proc_and_unix_sockets() {
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("null");
        std::os::unix::fs::symlink("/dev/null", &link).unwrap();
        let sockets = tempfile::tempdir().unwrap();
        let socket = sockets.path().join("agent.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        for refused in [
            "/dev/null".to_owned(),
            "/proc/self".to_owned(),
            link.display().to_string(),
            socket.display().to_string(),
            sockets.path().display().to_string(),
        ] {
            let error = arg_binds(std::slice::from_ref(&refused)).unwrap_err();
            assert!(error.contains(&refused), "{error}");
        }
        let plain = tempfile::tempdir().unwrap();
        std::fs::write(plain.path().join("a.yml"), "x").unwrap();
        let arg = plain.path().display().to_string();
        assert_eq!(
            arg_binds(std::slice::from_ref(&arg)).unwrap(),
            vec![format!("{arg}:rw")]
        );
    }
}
