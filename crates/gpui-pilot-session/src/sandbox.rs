//! The bwrap command around the private session: its argument list, environment allow-list
//! and private files, plus a preflight check of the host tools it needs.

use crate::SessionError;
use crate::cvt::{CvtMode, cvt_reduced};
use rustix::fs::OFlags;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The Wayland socket name KWin listens on inside the sandbox.
pub const WAYLAND_SOCKET: &str = "gpui-pilot";

/// Variables the sandbox owns for its isolation; `prepare` refuses them in `vars`, as it
/// refuses any name starting with one of `RESERVED_PREFIXES`.
pub const RESERVED_NAMES: [&str; 6] = [
    "HOME",
    "TMPDIR",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "AT_SPI_BUS_ADDRESS",
];
pub const RESERVED_PREFIXES: [&str; 2] = ["DBUS_", "XDG_"];

/// Whether `key` is one of the sandbox's isolation variables.
pub fn is_reserved(key: &str) -> bool {
    RESERVED_NAMES.contains(&key) || RESERVED_PREFIXES.iter().any(|p| key.starts_with(p))
}

const SESSION_CONF: &str = "/usr/share/dbus-1/session.conf";
const BROKER_LAUNCH: &str = "/usr/bin/dbus-broker-launch";

/// The sandbox's network: none at all, or a user-mode stack from `pasta` that forwards only
/// `Sandbox::host_ports` to the host.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    None,
    Pasta,
}

/// The virtual output: `width` x `height` logical pixels at `scale`, refreshed at `refresh_hz`.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Output {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub refresh_hz: u32,
}

/// A device width libxcvt would change, with the nearest logical widths whose device width
/// it keeps (`None` when no such width lies in the scanned range).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidthRefused {
    pub device_width: u32,
    pub cvt_width: u32,
    pub below: Option<u32>,
    pub above: Option<u32>,
}

impl Output {
    /// An output at 60 Hz, KWin's built-in virtual mode.
    pub const fn new(width: u32, height: u32, scale: f32) -> Output {
        Output {
            width,
            height,
            scale,
            refresh_hz: 60,
        }
    }

    pub const fn with_refresh_hz(self, refresh_hz: u32) -> Output {
        Output { refresh_hz, ..self }
    }

    /// The scale as written to KWin: rounded to three decimals.
    fn written_scale(&self) -> f64 {
        (f64::from(self.scale) * 1000.0).round() / 1000.0
    }

    fn device(&self, logical: u32) -> u32 {
        (f64::from(logical) * self.written_scale()).round() as u32
    }

    /// Device pixels, rounded exactly as `output_config` rounds them.
    pub fn device_size(&self) -> (u32, u32) {
        (self.device(self.width), self.device(self.height))
    }

    fn custom_mode(&self) -> Option<CvtMode> {
        let (width, height) = self.device_size();
        (self.refresh_hz != 60).then(|| cvt_reduced(width, height, self.refresh_hz))
    }

    /// What KWin will advertise: 60000 at 60 Hz, else `cvt_reduced(device).refresh_mhz`.
    pub fn expected_refresh_mhz(&self) -> u32 {
        self.custom_mode().map_or(60_000, |mode| mode.refresh_mhz)
    }

    /// Ok at 60 Hz, or when libxcvt keeps the device size; else the nearest logical widths
    /// below and above whose device width it keeps.
    pub fn check_custom_mode(&self) -> Result<(), WidthRefused> {
        let Some(mode) = self.custom_mode() else {
            return Ok(());
        };
        let device_width = self.device(self.width);
        if mode.width == device_width {
            return Ok(());
        }
        // Scan logical widths outwards, keeping the device height, for the first that libxcvt
        // leaves untouched at this rate.
        let device_height = self.device(self.height);
        let kept = |logical: &u32| {
            let device = self.device(*logical);
            cvt_reduced(device, device_height, self.refresh_hz).width == device
        };
        Err(WidthRefused {
            device_width,
            cvt_width: mode.width,
            below: (1..self.width).rev().find(kept),
            above: (self.width + 1..=self.width + 64).find(kept),
        })
    }
}

/// A host path made visible inside the sandbox at `dest`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub writable: bool,
}

impl Bind {
    /// `path` at the same place, read-only.
    pub fn ro(path: impl Into<PathBuf>) -> Bind {
        let path = path.into();
        Bind {
            source: path.clone(),
            dest: path,
            writable: false,
        }
    }

    /// `path` at the same place, writable.
    pub fn rw(path: impl Into<PathBuf>) -> Bind {
        Bind {
            writable: true,
            ..Bind::ro(path)
        }
    }

    /// `source` mounted writable at a different `dest`.
    pub fn at(source: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Bind {
        Bind {
            source: source.into(),
            dest: dest.into(),
            writable: true,
        }
    }

    /// The config syntax: `path` is read-only, `path:rw` is writable.
    pub fn parse(spec: &str) -> Bind {
        match spec.strip_suffix(":rw") {
            Some(path) => Bind::rw(path),
            None => Bind::ro(spec),
        }
    }
}

/// One session's sandbox, rooted at the launch directory `dir`, which holds the private
/// HOME, the runtime directory and the bus config.
#[non_exhaustive]
pub struct Sandbox {
    pub dir: PathBuf,
    pub output: Output,
    pub network: Network,
    pub binds: Vec<Bind>,
    pub vars: Vec<(String, String)>,
    /// With pasta: namespace loopback TCP ports forwarded to the same ports on the host's loopback.
    pub host_ports: Vec<u16>,
}

impl Sandbox {
    /// An offline sandbox with no extra binds or variables.
    pub fn new(dir: impl Into<PathBuf>, output: Output) -> Sandbox {
        Sandbox {
            dir: dir.into(),
            output,
            network: Network::None,
            binds: Vec::new(),
            vars: Vec::new(),
            host_ports: Vec::new(),
        }
    }

    pub fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    pub fn runtime(&self) -> PathBuf {
        self.dir.join("run")
    }

    /// The whole environment of the sandboxed command: a fixed allow-list, a few inherited
    /// values, then `vars`. Nothing else from `inherited` gets through.
    pub fn env(
        &self,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Vec<(OsString, OsString)> {
        let inherited: Vec<_> = inherited.into_iter().collect();
        let pick = |key: &str| {
            inherited
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        let mut env = vec![("PATH".into(), "/usr/bin".into())];
        env.push((
            "LANG".into(),
            pick("LANG").unwrap_or_else(|| "C.UTF-8".into()),
        ));
        for key in ["RUST_LOG", "RUST_BACKTRACE"] {
            if let Some(value) = pick(key) {
                env.push((key.into(), value));
            }
        }
        env.push(("HOME".into(), self.home().into()));
        env.push(("XDG_RUNTIME_DIR".into(), self.runtime().into()));
        for (key, value) in [
            ("XDG_CURRENT_DESKTOP", "KDE"),
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", WAYLAND_SOCKET),
            ("QT_QPA_PLATFORM", "wayland"),
            ("TMPDIR", "/tmp"),
        ] {
            env.push((key.into(), value.into()));
        }
        env.extend(self.vars.iter().map(|(k, v)| (k.into(), v.into())));
        env
    }

    /// The full command line: optional `pasta`, then `bwrap` with a fresh /tmp, /run and
    /// /run/user so no desktop socket is visible, then a private `dbus-run-session`.
    pub fn argv(&self, program: &Path, args: &[OsString]) -> Vec<OsString> {
        let mut argv: Vec<OsString> = Vec::new();
        let mut push = |items: &[&OsStr]| argv.extend(items.iter().map(|s| s.to_os_string()));
        let s = |v: &'static str| OsStr::new(v);
        if self.network == Network::Pasta {
            // Only the named ports reach the host, and only from the namespace to the host.
            let ports: Vec<String> = self.host_ports.iter().map(u16::to_string).collect();
            let ports = if ports.is_empty() {
                "none".to_owned()
            } else {
                ports.join(",")
            };
            for arg in ["pasta", "--foreground", "--config-net", "--no-map-gw"] {
                push(&[s(arg)]);
            }
            push(&[s("-t"), s("none"), s("-T"), OsStr::new(&ports)]);
            for arg in ["-u", "none", "-U", "none", "--quiet", "--"] {
                push(&[s(arg)]);
            }
        }
        push(&[s("bwrap"), s("--die-with-parent")]);
        if self.network == Network::None {
            push(&[s("--unshare-net")]);
        }
        push(&[
            s("--unshare-pid"),
            s("--unshare-ipc"),
            s("--ro-bind"),
            s("/usr"),
            s("/usr"),
        ]);
        for (target, link) in [
            ("usr/lib", "/lib"),
            ("usr/lib", "/lib64"),
            ("usr/bin", "/bin"),
            ("usr/bin", "/sbin"),
        ] {
            push(&[s("--symlink"), s(target), s(link)]);
        }
        push(&[
            s("--ro-bind"),
            s("/etc"),
            s("/etc"),
            s("--ro-bind"),
            s("/sys"),
            s("/sys"),
        ]);
        push(&[
            s("--proc"),
            s("/proc"),
            s("--dev"),
            s("/dev"),
            s("--dev-bind-try"),
            s("/dev/dri"),
            s("/dev/dri"),
        ]);
        for device in nvidia_devices(Path::new("/dev")) {
            push(&[s("--dev-bind"), device.as_os_str(), device.as_os_str()]);
        }
        push(&[s("--tmpfs"), s("/tmp"), s("--tmpfs"), s("/run")]);
        push(&[s("--ro-bind-try"), s("/run/dbus"), s("/run/dbus")]);
        push(&[
            s("--tmpfs"),
            s("/run/user"),
            s("--tmpfs"),
            s("/tmp/.X11-unix"),
        ]);
        // Masking the broker launcher makes dbus-run-session fall back to dbus-daemon, which
        // honours the private session.conf.
        if Path::new(BROKER_LAUNCH).exists() {
            push(&[s("--ro-bind"), s("/dev/null"), s(BROKER_LAUNCH)]);
        }
        push(&[s("--bind"), self.dir.as_os_str(), self.dir.as_os_str()]);
        for bind in &self.binds {
            let flag = if bind.writable { "--bind" } else { "--ro-bind" };
            push(&[s(flag), bind.source.as_os_str(), bind.dest.as_os_str()]);
        }
        let mut config = OsString::from("--config-file=");
        config.push(self.dir.join("session.conf"));
        push(&[
            s("dbus-run-session"),
            config.as_os_str(),
            s("--"),
            program.as_os_str(),
        ]);
        argv.extend(args.iter().cloned());
        argv
    }

    /// Writes the session's private files: the bus config listening in the runtime
    /// directory, the AltGr keymap, and the output config that gives the virtual output its
    /// scale. Safe to call again over existing directories. A reserved `vars` key is
    /// `InvalidInput` naming it, before anything is written: `env` appends `vars` last, so it
    /// would override the allow-list.
    pub fn prepare(&self) -> std::io::Result<()> {
        if let Some((key, _)) = self.vars.iter().find(|(key, _)| is_reserved(key)) {
            let why = format!("variable {key} is reserved by the sandbox's isolation");
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, why));
        }
        let config = self.home().join(".config");
        for dir in [self.home(), self.runtime(), config.clone()] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
            // The app can rewrite a kept launch dir, so a directory it swapped for a link is
            // refused before anything below it is written.
            if !std::fs::symlink_metadata(&dir)?.is_dir() {
                let why = format!("{} is not a real directory", dir.display());
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, why));
            }
        }
        let template = std::fs::read_to_string(SESSION_CONF)?;
        let (start, end) = match (template.find("<listen>"), template.find("</listen>")) {
            (Some(start), Some(end)) if start < end => (start, end + "</listen>".len()),
            _ => {
                let why = format!("{SESSION_CONF} has no <listen> element");
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, why));
            }
        };
        let listen = format!("<listen>unix:dir={}</listen>", self.runtime().display());
        let conf = format!("{}{listen}{}", &template[..start], &template[end..]);
        private_file(&self.dir.join("session.conf"))?.write_all(conf.as_bytes())?;
        private_file(&config.join("kxkbrc"))?
            .write_all(b"[Layout]\nLayoutList=us\nVariantList=altgr-intl\nUse=true\n")?;
        private_file(&config.join("kwinoutputconfig.json"))?
            .write_all(self.output_config().as_bytes())?;
        Ok(())
    }

    /// The sandboxed command with a cleared environment, ready to spawn.
    pub fn command(&self, program: &Path, args: &[OsString]) -> Command {
        let argv = self.argv(program, args);
        let mut command = Command::new(&argv[0]);
        command
            .env_clear()
            .envs(self.env(std::env::vars_os()))
            .args(&argv[1..]);
        command
    }

    // KWin ignores `--scale` on a virtual output, so the mode is the device-pixel size and
    // the scale is written here; see docs/research/2026-10-10-session-gate-spike.md.
    fn output_config(&self) -> String {
        let (width, height) = self.output.device_size();
        let mut output = serde_json::json!({
            "connectorName": "Virtual-0", "edidHash": "", "edidIdentifier": "",
            "mode": {"basic": {"width": width, "height": height, "refreshRate": 60000}, "flags": 1},
            "scale": self.output.written_scale(), "transform": "Normal",
        });
        // Any other rate adds a custom mode: KWin 6.8 stores the libxcvt timings themselves, and
        // the current mode must repeat them exactly to be selected.
        if let Some(mode) = self.output.custom_mode() {
            let entry = serde_json::json!({"cvt": {
                "clock": mode.dot_clock_khz, "flags": 9, "hdisplay": mode.width, "hskew": 0,
                "hsyncEnd": mode.hsync_end, "hsyncStart": mode.hsync_start, "htotal": mode.htotal,
                "vdisplay": mode.height, "vscan": 1, "vsyncEnd": mode.vsync_end,
                "vsyncStart": mode.vsync_start, "vtotal": mode.vtotal,
            }, "flags": 10});
            output["customModes"] = serde_json::json!([entry.clone()]);
            output["mode"] = entry;
        }
        let json = serde_json::json!([
            {"name": "outputs", "data": [output]},
            {"name": "setups", "data": [{"lidClosed": false, "outputs": [{
                "enabled": true, "outputIndex": 0, "position": {"x": 0, "y": 0}, "priority": 0,
            }]}]},
        ]);
        json.to_string()
    }
}

/// Opens `path` for writing as a 0600 file, refusing to follow a link planted in its place.
fn private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

/// Checks the host tools with the process PATH.
pub fn preflight(network: Network) -> Result<(), SessionError> {
    preflight_in(&std::env::var_os("PATH").unwrap_or_default(), network)
}

/// Checks that every host tool the session needs is present and that bwrap can create a
/// user namespace. The first failure names its tool.
pub fn preflight_in(path: &OsStr, network: Network) -> Result<(), SessionError> {
    let missing = |tool: &str, why: String| SessionError::EnvironmentMissing {
        tool: tool.to_owned(),
        why,
    };
    let mut tools = vec!["bwrap", "dbus-run-session", "kwin_wayland"];
    if network == Network::Pasta {
        tools.push("pasta");
    }
    let mut bwrap = None;
    for tool in tools {
        let found = std::env::split_paths(path)
            .map(|dir| dir.join(tool))
            .find(|p| is_executable(p));
        let Some(found) = found else {
            return Err(missing(
                tool,
                format!("not found on PATH {}", path.to_string_lossy()),
            ));
        };
        bwrap.get_or_insert(found);
    }
    for file in [
        "/usr/lib/at-spi2-registryd",
        "/usr/lib/at-spi-bus-launcher",
        SESSION_CONF,
    ] {
        if !Path::new(file).exists() {
            return Err(missing(file, "the file does not exist".to_owned()));
        }
    }
    let bwrap = bwrap.expect("bwrap is the first tool checked");
    let probe = Command::new(&bwrap)
        .args([
            "--ro-bind",
            "/",
            "/",
            "--unshare-pid",
            "--die-with-parent",
            "/usr/bin/true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output();
    match probe {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(missing(
                "bwrap",
                format!("user namespaces look disabled: {}", stderr.trim()),
            ))
        }
        Err(error) => Err(missing(
            "bwrap",
            format!("could not run {}: {error}", bwrap.display()),
        )),
    }
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The NVIDIA character devices under `dev` (`nvidia0`, `nvidiactl`, ...), sorted;
/// directories such as `nvidia-caps` are left out.
pub(crate) fn nvidia_devices(dev: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dev) else {
        return Vec::new();
    };
    let mut devices: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("nvidia"))
        .map(|e| e.path())
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.file_type().is_char_device()))
        .collect();
    devices.sort();
    devices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionError;
    use gpui_pilot_protocol::ErrorCode;
    use std::collections::BTreeMap;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn sandbox() -> Sandbox {
        Sandbox::new("/tmp/gp-abc123", Output::new(1280, 800, 1.5))
    }

    fn position(hay: &[OsString], needle: &[&str]) -> Option<usize> {
        let needle = os(needle);
        hay.windows(needle.len())
            .position(|w| w == needle.as_slice())
    }

    #[test]
    fn the_offline_argv_unshares_everything_and_ends_in_dbus_run_session() {
        let argv = sandbox().argv(
            Path::new("/opt/agent"),
            &os(&["session-agent", "--", "/app"]),
        );
        assert_eq!(argv[0], "bwrap");
        for run in [
            &["--die-with-parent"][..],
            &["--unshare-net"],
            &["--unshare-pid"],
            &["--unshare-ipc"],
            &["--ro-bind", "/usr", "/usr"],
            &["--ro-bind", "/etc", "/etc"],
            &["--ro-bind", "/sys", "/sys"],
            &["--proc", "/proc"],
            &["--dev", "/dev"],
            &["--dev-bind-try", "/dev/dri", "/dev/dri"],
            &["--tmpfs", "/run"],
            &["--ro-bind-try", "/run/dbus", "/run/dbus"],
            &["--tmpfs", "/run/user"],
            &["--tmpfs", "/tmp/.X11-unix"],
        ] {
            assert!(
                position(&argv, run).is_some(),
                "{run:?} missing from {argv:?}"
            );
        }
        let tmp = position(&argv, &["--tmpfs", "/tmp"]).expect("tmpfs over /tmp");
        let dir = position(&argv, &["--bind", "/tmp/gp-abc123", "/tmp/gp-abc123"])
            .expect("the launch dir");
        assert!(tmp < dir, "the launch dir is bound over the fresh /tmp");
        let tail = os(&[
            "dbus-run-session",
            "--config-file=/tmp/gp-abc123/session.conf",
            "--",
            "/opt/agent",
            "session-agent",
            "--",
            "/app",
        ]);
        assert!(argv.ends_with(&tail), "{argv:?}");
        let home = std::env::var_os("HOME").unwrap_or_default();
        assert!(
            !argv
                .iter()
                .any(|a| a == &home || a.to_string_lossy().starts_with("/run/user/"))
        );
    }

    #[test]
    fn host_ports_forward_only_those_ports_to_the_host() {
        let mut s = sandbox();
        s.network = Network::Pasta;
        s.host_ports = vec![8080, 9000];
        let argv = s.argv(Path::new("/opt/agent"), &[]);
        let text: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let runs = |run: &[&str]| text.windows(run.len()).any(|w| w == run);
        assert!(runs(&["-T", "8080,9000"]), "{text:?}");
        for flag in ["-t", "-u", "-U"] {
            assert!(runs(&[flag, "none"]), "{flag}: {text:?}");
        }
        assert!(runs(&["--no-map-gw"]));
        assert!(!runs(&["-T", "none"]));
    }

    #[test]
    fn pasta_wraps_bwrap_and_drops_unshare_net() {
        let mut s = sandbox();
        s.network = Network::Pasta;
        let argv = s.argv(Path::new("/opt/agent"), &[]);
        let prefix = os(&[
            "pasta",
            "--foreground",
            "--config-net",
            "--no-map-gw",
            "-t",
            "none",
            "-T",
            "none",
            "-u",
            "none",
            "-U",
            "none",
            "--quiet",
            "--",
            "bwrap",
        ]);
        assert!(argv.starts_with(&prefix), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "--unshare-net"));
    }

    #[test]
    fn nvidia_nodes_are_character_devices_only() {
        for path in nvidia_devices(Path::new("/dev")) {
            let kind = std::fs::metadata(&path).unwrap().file_type();
            assert!(kind.is_char_device(), "{}", path.display());
            assert!(
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("nvidia")
            );
        }
        assert!(!nvidia_devices(Path::new("/dev")).contains(&PathBuf::from("/dev/nvidia-caps")));
    }

    #[test]
    fn binds_follow_the_config_syntax_and_come_after_the_tmpfs() {
        assert_eq!(Bind::parse("/srv/fixtures"), Bind::ro("/srv/fixtures"));
        assert_eq!(Bind::parse("/srv/out:rw"), Bind::rw("/srv/out"));
        let mut s = sandbox();
        s.binds = vec![
            Bind::parse("/srv/fixtures"),
            Bind::parse("/srv/out:rw"),
            Bind::at("/tmp/fake.sock", "/run/user/1000/bus"),
        ];
        let argv = s.argv(Path::new("/opt/agent"), &[]);
        let user = position(&argv, &["--tmpfs", "/run/user"]).unwrap();
        for run in [
            &["--ro-bind", "/srv/fixtures", "/srv/fixtures"][..],
            &["--bind", "/srv/out", "/srv/out"],
            &["--bind", "/tmp/fake.sock", "/run/user/1000/bus"],
        ] {
            let at = position(&argv, run).unwrap_or_else(|| panic!("{run:?} missing"));
            assert!(at > user, "{run:?} must follow the /run/user tmpfs");
        }
    }

    #[test]
    fn the_environment_is_the_allow_list_only() {
        let inherited = [
            ("PATH", "/home/u/bin:/usr/bin"),
            ("LANG", "de_DE.UTF-8"),
            ("RUST_LOG", "debug"),
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HOME", "/home/u"),
            ("SSH_AUTH_SOCK", "/run/user/1000/ssh"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let mut s = sandbox();
        s.vars = vec![(
            "GPUI_PILOT_SOCKET".into(),
            "/tmp/gp-abc123/pilot.sock".into(),
        )];
        let env: BTreeMap<String, String> = s
            .env(inherited)
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect();
        let expected: BTreeMap<String, String> = [
            ("PATH", "/usr/bin"),
            ("LANG", "de_DE.UTF-8"),
            ("RUST_LOG", "debug"),
            ("HOME", "/tmp/gp-abc123/home"),
            ("XDG_RUNTIME_DIR", "/tmp/gp-abc123/run"),
            ("XDG_CURRENT_DESKTOP", "KDE"),
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "gpui-pilot"),
            ("QT_QPA_PLATFORM", "wayland"),
            ("TMPDIR", "/tmp"),
            ("GPUI_PILOT_SOCKET", "/tmp/gp-abc123/pilot.sock"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .into();
        assert_eq!(env, expected);
        let bare = sandbox().env(Vec::<(OsString, OsString)>::new());
        assert!(bare.contains(&("LANG".into(), "C.UTF-8".into())));
    }

    #[test]
    fn prepare_writes_the_bus_config_keymap_and_output_scale() {
        let dir = tempfile::Builder::new()
            .prefix("gp-")
            .tempdir_in("/tmp")
            .unwrap();
        let s = Sandbox::new(dir.path(), Output::new(1280, 800, 1.5));
        s.prepare().unwrap();
        let conf = std::fs::read_to_string(dir.path().join("session.conf")).unwrap();
        let listen = format!("<listen>unix:dir={}</listen>", s.runtime().display());
        assert!(conf.contains(&listen), "{conf}");
        assert_eq!(conf.matches("<listen>").count(), 1);
        for sub in [s.home(), s.runtime()] {
            let mode = std::fs::metadata(&sub).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", sub.display());
        }
        let kxkbrc = std::fs::read_to_string(s.home().join(".config/kxkbrc")).unwrap();
        assert!(kxkbrc.contains("LayoutList=us") && kxkbrc.contains("VariantList=altgr-intl"));
        let text = std::fs::read_to_string(s.home().join(".config/kwinoutputconfig.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        let output = &json[0]["data"][0];
        assert_eq!(json[0]["name"], "outputs");
        assert_eq!(output["connectorName"], "Virtual-0");
        assert_eq!(output["scale"], 1.5);
        assert_eq!(
            (
                output["mode"]["basic"]["width"].as_u64(),
                output["mode"]["basic"]["height"].as_u64()
            ),
            (Some(1920), Some(1200))
        );
        assert_eq!(json[1]["name"], "setups");
        s.prepare()
            .expect("prepare runs again over existing directories");
    }

    #[test]
    fn the_output_scale_is_written_as_a_short_decimal() {
        let dir = tempfile::Builder::new()
            .prefix("gp-")
            .tempdir_in("/tmp")
            .unwrap();
        let s = Sandbox::new(dir.path(), Output::new(1000, 600, 1.1));
        s.prepare().unwrap();
        let text = std::fs::read_to_string(s.home().join(".config/kwinoutputconfig.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json[0]["data"][0]["scale"], 1.1,
            "f32 1.1 is rounded to 3 decimals, not 1.100000023841858"
        );
        assert_eq!(json[0]["data"][0]["mode"]["basic"]["width"], 1100);
    }

    #[test]
    fn preflight_names_the_first_missing_tool() {
        let error = preflight_in(OsStr::new("/nonexistent"), Network::None).unwrap_err();
        assert!(
            matches!(&error, SessionError::EnvironmentMissing { tool, .. } if tool == "bwrap"),
            "{error}"
        );
        assert_eq!(error.code(), ErrorCode::EnvironmentMissing);
    }

    #[test]
    fn prepare_refuses_vars_that_would_undo_the_isolation() {
        let fresh = || {
            tempfile::Builder::new()
                .prefix("gp-")
                .tempdir_in("/tmp")
                .unwrap()
        };
        let keys = [
            "HOME",
            "TMPDIR",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "AT_SPI_BUS_ADDRESS",
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_SYSTEM_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
            "XDG_CONFIG_HOME",
        ];
        for key in keys {
            let dir = fresh();
            let mut s = Sandbox::new(dir.path(), Output::new(1280, 800, 1.5));
            s.vars.push((key.into(), "x".into()));
            let error = s.prepare().unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{key}");
            assert!(error.to_string().contains(key), "{error}");
            assert!(
                !dir.path().join("session.conf").exists(),
                "{key} is refused before any write"
            );
        }
        let dir = fresh();
        let mut s = Sandbox::new(dir.path(), Output::new(1280, 800, 1.5));
        s.vars
            .push(("GPUI_PILOT_SOCKET".into(), "/tmp/gp-x/pilot.sock".into()));
        s.prepare().unwrap();
    }

    #[test]
    fn prepare_writes_through_no_link_the_app_could_plant() {
        let dir = tempfile::Builder::new()
            .prefix("gp-")
            .tempdir_in("/tmp")
            .unwrap();
        let elsewhere = tempfile::Builder::new()
            .prefix("gp-")
            .tempdir_in("/tmp")
            .unwrap();
        let s = Sandbox::new(dir.path(), Output::new(1280, 800, 1.5));
        s.prepare().unwrap();
        let conf = dir.path().join("session.conf");
        std::fs::remove_file(&conf).unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("victim"), &conf).unwrap();
        assert!(s.prepare().is_err(), "a linked session.conf is refused");
        assert!(!elsewhere.path().join("victim").exists());
        std::fs::remove_file(&conf).unwrap();
        let config = s.home().join(".config");
        std::fs::remove_dir_all(&config).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &config).unwrap();
        let error = s.prepare().unwrap_err();
        assert!(error.to_string().contains(".config"), "{error}");
        assert!(!elsewhere.path().join("kxkbrc").exists());
        assert!(!elsewhere.path().join("kwinoutputconfig.json").exists());
    }

    fn written(output: Output) -> serde_json::Value {
        let dir = tempfile::Builder::new()
            .prefix("gp-")
            .tempdir_in("/tmp")
            .unwrap();
        let s = Sandbox::new(dir.path(), output);
        s.prepare().unwrap();
        let text = std::fs::read_to_string(s.home().join(".config/kwinoutputconfig.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        json[0]["data"][0].clone()
    }

    #[test]
    fn sixty_hertz_keeps_kwins_built_in_mode() {
        let output = written(Output::new(1280, 800, 1.0));
        assert_eq!(
            output["mode"],
            serde_json::json!({"basic": {"width": 1280, "height": 800, "refreshRate": 60000}, "flags": 1})
        );
        assert!(output.get("customModes").is_none(), "{output}");
        assert_eq!(Output::new(1280, 800, 1.0).expected_refresh_mhz(), 60_000);
    }

    #[test]
    fn other_rates_write_the_libxcvt_custom_mode() {
        let output = written(Output::new(1280, 800, 1.0).with_refresh_hz(144));
        // The timings KWin 6.8 writes back for a 1280x800 reduced-blanking 144 Hz mode.
        let timings = serde_json::json!({
            "clock": 177500, "flags": 9, "hdisplay": 1280, "hskew": 0, "hsyncEnd": 1360,
            "hsyncStart": 1328, "htotal": 1440, "vdisplay": 800, "vscan": 1, "vsyncEnd": 809,
            "vsyncStart": 803, "vtotal": 857,
        });
        assert_eq!(
            output["customModes"],
            serde_json::json!([{"cvt": timings, "flags": 10}])
        );
        assert_eq!(
            output["mode"],
            serde_json::json!({"cvt": timings, "flags": 10})
        );
        let scaled = Output::new(1280, 800, 1.5).with_refresh_hz(144);
        assert_eq!(scaled.device_size(), (1920, 1200));
        assert_eq!(scaled.expected_refresh_mhz(), 143_838);
        assert_eq!(written(scaled)["mode"]["cvt"]["clock"], 384_750);
        assert_eq!(
            Output::new(1280, 800, 1.0)
                .with_refresh_hz(240)
                .expected_refresh_mhz(),
            239_776
        );
        assert_eq!(
            Output::new(1280, 800, 1.0)
                .with_refresh_hz(120)
                .expected_refresh_mhz(),
            119_909
        );
    }

    #[test]
    fn a_width_libxcvt_would_round_is_refused_with_the_nearest_kept_widths() {
        let refused = Output::new(1100, 600, 1.0)
            .with_refresh_hz(144)
            .check_custom_mode();
        assert_eq!(
            refused,
            Err(WidthRefused {
                device_width: 1100,
                cvt_width: 1096,
                below: Some(1096),
                above: Some(1104)
            })
        );
        let fractional = Output::new(1000, 600, 1.1)
            .with_refresh_hz(144)
            .check_custom_mode()
            .unwrap_err();
        assert_eq!(
            (fractional.device_width, fractional.below, fractional.above),
            (1100, Some(996), Some(1004))
        );
        assert_eq!(
            Output::new(1100, 600, 1.0).check_custom_mode(),
            Ok(()),
            "60 Hz uses the built-in mode"
        );
        // libxcvt turns 1360x768 into 1366x768, so 1360 is refused and never offered.
        let fwxga = Output::new(1360, 768, 1.0)
            .with_refresh_hz(144)
            .check_custom_mode()
            .unwrap_err();
        assert_eq!(
            (fwxga.cvt_width, fwxga.below, fwxga.above),
            (1366, Some(1352), Some(1366))
        );
        let near = Output::new(1361, 768, 1.0)
            .with_refresh_hz(144)
            .check_custom_mode()
            .unwrap_err();
        assert_eq!((near.below, near.above), (Some(1352), Some(1366)));
        for ok in [
            Output::new(1280, 800, 1.0),
            Output::new(1280, 800, 1.5),
            Output::new(1280, 800, 1.25),
            Output::new(1366, 768, 1.0),
            Output::new(683, 384, 2.0),
        ] {
            assert_eq!(
                ok.with_refresh_hz(144).check_custom_mode(),
                Ok(()),
                "{ok:?}"
            );
        }
    }
}
