use directories;
use failure::{bail, Fallible};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use serde_json;
use signal_hook;
use std::io::prelude::*;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use structopt::StructOpt;

#[derive(Deserialize, Clone, Debug)]
struct PodmanImageInspect {
    id: String,
    names: Vec<String>,
}

static DEFAULT_IMAGE: &str = "registry.fedoraproject.org/f30/fedora-toolbox:30";
/// The label set on toolbox images and containers.
static TOOLBOX_LABEL: &str = "com.coreos.toolbox";
/// The default container name
static DEFAULT_NAME: &str = "coreos-toolbox";

lazy_static! {
    static ref APPDIRS: directories::ProjectDirs =
        directories::ProjectDirs::from("com", "coreos", "toolbox").expect("creating appdirs");
}

static MAX_UID_COUNT: u32 = 65536;

/// Set of statically known paths to files/directories
/// that we redirect inside the container to /host.
static STATIC_HOST_FORWARDS: &[&str] = &["/run/dbus", "/run/libvirt"];
/// Set of devices we forward (if they exist)
static FORWARDED_DEVICES: &[&str] = &["bus", "dri", "kvm", "fuse"];

static PRESERVED_ENV: &[&str] = &[
    "COLORTERM",
    "DBUS_SESSION_BUS_ADDRESS",
    "DESKTOP_SESSION",
    "DISPLAY",
    "USER",
    "LANG",
    "SHELL",
    "SSH_AUTH_SOCK",
    "TERM",
    "VTE_VERSION",
    "XDG_CURRENT_DESKTOP",
    "XDG_DATA_DIRS",
    "XDG_MENU_PREFIX",
    "XDG_RUNTIME_DIR",
    "XDG_SEAT",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
    "XDG_VTNR",
    "WAYLAND_DISPLAY",
];

trait CommandRunExt {
    fn run(&mut self) -> Fallible<()>;
}

impl CommandRunExt for Command {
    fn run(&mut self) -> Fallible<()> {
        let r = self.status()?;
        if !r.success() {
            bail!("Child [{:?}] exited: {}", self, r);
        }
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
struct CreateOpts {
    #[structopt(
        short = "I",
        long = "image",
    )]
    /// Use a different base image
    image: Option<String>,

    #[structopt(short = "n", long = "name")]
    /// Name the container
    name: Option<String>,

    #[structopt(short = "N", long = "nested")]
    /// Allow running inside a container
    nested: bool,

    #[structopt(short = "D", long = "destroy")]
    /// Destroy any existing container
    destroy: bool,
}

#[derive(Debug, StructOpt)]
struct RunOpts {
    #[structopt(short = "n", long = "name")]
    /// Name of container
    name: Option<String>,

    #[structopt(short = "N", long = "nested")]
    /// Allow running inside a container
    nested: bool,
}

#[derive(Debug, StructOpt)]
struct RmOpts {
    #[structopt(short = "n", long = "name", default_value = "coreos-toolbox")]
    /// Name for container
    name: String,
}

#[derive(Debug, StructOpt)]
#[structopt(name = "coretoolbox", about = "Toolbox")]
#[structopt(rename_all = "kebab-case")]
enum Opt {
    /// Create a  toolbox
    Create(CreateOpts),
    /// Enter the toolbox
    Run(RunOpts),
    /// Delete the toolbox container
    Rm(RmOpts),
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab-case")]
enum InternalOpt {
    /// Internal implementation detail; do not use
    RunPid1,
    /// Internal implementation detail; do not use
    Exec,
}

fn cmd_podman() -> Command {
    if let Some(podman) = std::env::var_os("podman") {
        Command::new(podman)
    } else {
        Command::new("podman")
    }
}

#[allow(dead_code)]
enum InspectType {
    Container,
    Image,
}

/// Returns true if an image or container is in the podman
/// storage.
fn podman_has(t: InspectType, name: &str) -> Fallible<bool> {
    let typearg = match t {
        InspectType::Container => "container",
        InspectType::Image => "image",
    };
    Ok(cmd_podman()
        .args(&["inspect", "--type", typearg, name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

fn podman_get_toolbox_images() -> Fallible<Vec<PodmanImageInspect>> {
    let proc = cmd_podman()
        .stdout(std::process::Stdio::piped())
        .args(&["images", "--output", "json", "--filter"])
        .arg(format!("label={}=true", TOOLBOX_LABEL))
        .spawn()?;
    let sout = proc.stdout.expect("stdout piped");
    let mut res = Vec::new();
    for val in serde_json::Deserializer::from_reader(sout).into_iter::<PodmanImageInspect>() {
        let val = val?;
        res.push(val);
    };
    Ok(res)
}

/// Pull a container image if not present
fn ensure_image(name: &str) -> Fallible<()> {
    if !podman_has(InspectType::Image, name)? {
        cmd_podman().args(&["pull", name]).run()?;
    }
    Ok(())
}

/// Parse an extant environment variable as UTF-8
fn getenv_required_utf8(n: &str) -> Fallible<String> {
    if let Some(v) = std::env::var_os(n) {
        Ok(v.to_str()
            .ok_or_else(|| failure::format_err!("{} is invalid UTF-8", n))?
            .to_string())
    } else {
        bail!("{} is unset", n)
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct EntrypointState {
    username: String,
    uid: u32,
    home: String,
}

fn append_preserved_env(c: &mut Command) -> Fallible<()> {
    for n in PRESERVED_ENV.iter() {
        let v = match std::env::var_os(n) {
            Some(v) => v,
            None => continue,
        };
        let v = v
            .to_str()
            .ok_or_else(|| failure::format_err!("{} contains invalid UTF-8", n))?;
        c.arg(format!("--env={}={}", n, v));
    }
    Ok(())
}

fn create(opts: &CreateOpts) -> Fallible<()> {
    if in_container() && !opts.nested {
        bail!("Already inside a container");
    }

    let image =
        if opts.image.is_none() && opts.name.is_none() && !podman_has(InspectType::Container, DEFAULT_NAME)? {
            let toolboxes = podman_get_toolbox_images()?;
            match toolboxes.len() {
                0 => DEFAULT_IMAGE.to_owned(),
                1 => toolboxes[0].names[0].clone(),
                _ => bail!("Multiple toolbox images found, must specify via -I"),
            }
        } else {
            opts.image.as_ref().map(|s|s.as_str()).unwrap_or(DEFAULT_IMAGE).to_owned()
        };

    let name = opts.name.as_ref().map(|s|s.as_str()).unwrap_or(DEFAULT_NAME);

    if opts.destroy {
        rm(&RmOpts {
            name: name.to_owned(),
        })?;
    }

    ensure_image(&image)?;

    // exec ourself as the entrypoint.  In the future this
    // would be better with podman fd passing.
    let self_bin = std::fs::read_link("/proc/self/exe")?;
    let self_bin = self_bin
        .as_path()
        .to_str()
        .ok_or_else(|| failure::err_msg("non-UTF8 self"))?;

    let runtime_dir = getenv_required_utf8("XDG_RUNTIME_DIR")?;
    let statefile = "coreos-toolbox.initdata";
    let real_uid: u32 = nix::unistd::getuid().into();
    let privileged = real_uid == 0;

    let mut podman = cmd_podman();
    // The basic arguments.
    podman.args(&[
        "create",
        "--interactive",
        "--tty",
        "--hostname=toolbox",
        "--network=host",
        // We are not aiming for security isolation here.
        "--privileged",
        "--security-opt=label=disable",
        "--tmpfs=/run:rw",
    ]);
    podman.arg(format!("--label={}=true", TOOLBOX_LABEL));
    podman.arg(format!("--name={}", name));
    // In privileged mode we assume we want to control all host processes by default;
    // we're more about debugging/management and less of a "dev container".
    if privileged {
        podman.arg("--pid=host");
    }
    // We bind ourself in so we can handle recursive invocation.
    podman.arg(format!("--volume={}:/usr/bin/toolbox:ro", self_bin));

    // In true privileged mode we don't use userns
    if !privileged {
        let uid_plus_one = real_uid + 1;
        let max_minus_uid = MAX_UID_COUNT - real_uid;
        podman.args(&[
            format!("--uidmap={}:0:1", real_uid),
            format!("--uidmap=0:1:{}", real_uid),
            format!(
                "--uidmap={}:{}:{}",
                uid_plus_one, uid_plus_one, max_minus_uid
            ),
        ]);
    }

    for p in &["/dev", "/usr", "/var", "/etc", "/run", "/tmp"] {
        podman.arg(format!("--volume={}:/host{}:rslave", p, p));
    }
    if privileged {
        let debugfs = "/sys/kernel/debug";
        if Path::new(debugfs).exists() {
            // Bind debugfs in privileged mode so we can use e.g. bpftrace
            podman.arg(format!("--volume={}:{}:rslave", debugfs, debugfs));
        }
    }
    append_preserved_env(&mut podman)?;
    podman.arg(format!("--env=TOOLBOX_STATEFILE={}", statefile));

    {
        let state = EntrypointState {
            username: getenv_required_utf8("USER")?,
            uid: real_uid,
            home: getenv_required_utf8("HOME")?,
        };
        let w = std::fs::File::create(format!("{}/{}", runtime_dir, statefile))?;
        let mut w = std::io::BufWriter::new(w);
        serde_json::to_writer(&mut w, &state)?;
        w.flush()?;
    }

    podman.arg(&image);
    podman.args(&["/usr/bin/toolbox", "internals", "run-pid1"]);
    podman.stdout(Stdio::null());
    podman.run()?;
    Ok(())
}

fn in_container() -> bool {
    Path::new("/run/.containerenv").exists()
}

fn run(opts: &RunOpts) -> Fallible<()> {
    if in_container() && !opts.nested {
        bail!("Already inside a container");
    }

    let name = opts.name.as_ref().map(|s|s.as_str()).unwrap_or(DEFAULT_NAME);

    cmd_podman()
        .args(&["start", name])
        .stdout(Stdio::null())
        .run()?;

    let mut podman = cmd_podman();
    podman.args(&["exec", "--interactive", "--tty"]);
    append_preserved_env(&mut podman)?;
    podman.args(&[name, "/usr/bin/toolbox", "internals", "exec"]);
    return Err(podman.exec().into());
}

fn rm(opts: &RmOpts) -> Fallible<()> {
    if !podman_has(InspectType::Container, opts.name.as_str())? {
        return Ok(());
    }
    let mut podman = cmd_podman();
    podman
        .args(&["rm", "-f", opts.name.as_str()])
        .stdout(Stdio::null());
    Err(podman.exec().into())
}

fn run_pid1(_opts: InternalOpt) -> Fallible<()> {
    unsafe {
        signal_hook::register(signal_hook::SIGCHLD, waitpid_all)?;
        signal_hook::register(signal_hook::SIGTERM, || std::process::exit(0))?;
    };
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1_000_000));
    }
}

fn waitpid_all() {
    use nix::sys::wait::WaitStatus;
    loop {
        match nix::sys::wait::waitpid(None, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(status) => match status {
                WaitStatus::StillAlive => break,
                _ => {}
            },
            Err(_) => break,
        }
    }
}

mod entrypoint {
    use super::CommandRunExt;
    use super::EntrypointState;
    use failure::{bail, Fallible, ResultExt};
    use fs2::FileExt;
    use rayon::prelude::*;
    use std::io::prelude::*;
    use std::os::unix;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    static CONTAINER_INITIALIZED_LOCK: &str = "/run/coreos-toolbox.lock";
    /// This file is created when we've generated a "container image" (overlayfs layer)
    /// that has things like our modifications to /etc/passwd, and the root `/`.
    static CONTAINER_INITIALIZED_STAMP: &str = "/etc/coreos-toolbox.initialized";
    /// This file is created when we've completed *runtime* state configuration
    /// changes such as bind mounts.
    static CONTAINER_INITIALIZED_RUNTIME_STAMP: &str = "/run/coreos-toolbox.initialized";

    /// Set of directories we explicitly make bind mounts rather than symlinks to /host.
    /// To ensure that paths are the same inside and out.
    static DATADIRS: &[&str] = &["/srv", "/mnt", "/home"];

    fn rbind(src: &str, dest: &str) -> Fallible<()> {
        Command::new("mount").args(&["--rbind", src, dest]).run()?;
        Ok(())
    }

    /// Update /etc/passwd with the same user from the host,
    /// and bind mount the homedir.
    fn adduser(state: &EntrypointState) -> Fallible<()> {
        if state.uid == 0 {
            return Ok(());
        }
        let uidstr = format!("{}", state.uid);
        Command::new("useradd")
            .args(&[
                "--no-create-home",
                "--home-dir",
                &state.home,
                "--uid",
                &uidstr,
                "--groups",
                "wheel",
                state.username.as_str(),
            ])
            .run()?;

        // Bind mount the homedir rather than use symlinks
        // as various software is unhappy if the path isn't canonical.
        std::fs::create_dir_all(&state.home)?;
        let uid = nix::unistd::Uid::from_raw(state.uid);
        let gid = nix::unistd::Gid::from_raw(state.uid);
        nix::unistd::chown(state.home.as_str(), Some(uid), Some(gid))?;
        let host_home = format!("/host{}", state.home);
        rbind(host_home.as_str(), state.home.as_str())?;
        Ok(())
    }

    /// Symlink a path e.g. /run/dbus/system_bus_socket to the
    /// /host equivalent, creating any necessary parent directories.
    fn host_symlink<P: AsRef<Path> + std::fmt::Display>(p: P) -> Fallible<()> {
        let path = p.as_ref();
        std::fs::create_dir_all(path.parent().unwrap())?;
        match std::fs::remove_dir_all(path) {
            Ok(_) => Ok(()),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }?;
        unix::fs::symlink(format!("/host{}", p), path)?;
        Ok(())
    }

    fn init_container_static() -> Fallible<()> {
        let initstamp = Path::new(CONTAINER_INITIALIZED_STAMP);
        if initstamp.exists() {
            return Ok(());
        }

        let lockf = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(CONTAINER_INITIALIZED_LOCK)?;
        lockf.lock_exclusive()?;

        if initstamp.exists() {
            return Ok(());
        }

        let runtime_dir = super::getenv_required_utf8("XDG_RUNTIME_DIR")?;
        let state: EntrypointState = {
            let p = format!("/host/{}/{}", runtime_dir, "coreos-toolbox.initdata");
            let f =
                std::fs::File::open(&p).with_context(|e| format!("Opening statefile: {}", e))?;
            std::fs::remove_file(p)?;
            serde_json::from_reader(std::io::BufReader::new(f))?
        };

        let ostree_based_host = std::path::Path::new("/host/run/ostree-booted").exists();

        // Convert the container to ostree-style layout
        if ostree_based_host {
            DATADIRS.par_iter()
                .try_for_each(|d| -> Fallible<()> {
                    std::fs::remove_dir(d)?;
                    let vard = format!("var{}", d);
                    unix::fs::symlink(&vard, d)?;
                    std::fs::create_dir(&vard)?;
                    Ok(())
                })?;
        }

        // This is another mount point used by udisks
        unix::fs::symlink("/host/run/media", "/run/media")?;

        // Remove anaconda cruft
        std::fs::read_dir("/tmp")?.try_for_each(|e| -> Fallible<()> {
            let e = e?;
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with("ks-script-") {
                    std::fs::remove_file(e.path())?;
                }
            }
            Ok(())
        })?;

        // Generate a unique tempdir in the host's /tmp and /var/tmp
        // so that we don't clash with any well-known dirs.  For example
        // fish creates /tmp/root.fish which will break with `sudo`
        // as the userns uid 0 isn't the same as the real uid 0 of course.
        ["/tmp", "/var/tmp"]
            .par_iter()
            .try_for_each(|d| -> Fallible<()> {
                std::fs::remove_dir(d)?;
                std::fs::create_dir(d)?;
                let hostd = format!("/host{}", d);
                let tmpd = tempfile::TempDir::new_in(&hostd)?.into_path();
                let uid = nix::unistd::Uid::from_raw(state.uid);
                let gid = nix::unistd::Gid::from_raw(state.uid);
                // Chown the dir rather than make it sticky+world-writable
                // like a regular tmpdir so that other users on the host
                // can't write to it too.  We rely on the userns root having
                // CAP_DAC_OVERRIDE for access to it as well.  This does
                // break *other* uids inside the toolbox, but eh.
                nix::unistd::chown(&tmpd, Some(uid), Some(gid))?;
                rbind(tmpd.as_path().as_os_str().to_str().unwrap(), d)?;
                Ok(())
            })
            .with_context(|e| format!("Handling tmpdirs: {}", e))?;

        // And forward the runtime dir
        host_symlink(runtime_dir).with_context(|e| format!("Forwarding runtime dir: {}", e))?;

        // These symlinks into /host are our set of default forwarded APIs/state
        // directories.
        super::STATIC_HOST_FORWARDS
            .par_iter()
            .try_for_each(host_symlink)
            .with_context(|e| format!("Enabling static host forwards: {}", e))?;

        // And these are into /dev
        if state.uid != 0 {
            super::FORWARDED_DEVICES
                .par_iter()
                .try_for_each(|d| -> Fallible<()> {
                    let hostd = format!("/host/dev/{}", d);
                    if Path::new(&hostd).exists() {
                        unix::fs::symlink(hostd, format!("/dev/{}", d))
                            .with_context(|e| format!("symlinking {}: {}", d, e))?;
                    }
                    Ok(())
                })
                .with_context(|e| format!("Forwarding devices: {}", e))?;
        }

        // Allow sudo
        || -> Fallible<()> {
            let f = std::fs::File::create(format!("/etc/sudoers.d/toolbox-{}", state.username))?;
            let mut f = std::io::BufWriter::new(f);
            writeln!(&mut f, "{} ALL=(ALL) NOPASSWD: ALL", state.username)?;
            f.flush()?;
            Ok(())
        }()
        .with_context(|e| format!("Enabling sudo: {}", e))?;

        adduser(&state)?;
        let _ = std::fs::File::create(&initstamp)?;

        Ok(())
    }

    fn init_container_runtime() -> Fallible<()> {
        let initstamp = Path::new(CONTAINER_INITIALIZED_RUNTIME_STAMP);
        if initstamp.exists() {
            return Ok(());
        }

        let lockf = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(CONTAINER_INITIALIZED_LOCK)?;
        lockf.lock_exclusive()?;

        if initstamp.exists() {
            return Ok(());
        }

        // Podman unprivileged mode has a bug where it exposes the host
        // selinuxfs which is bad because it can make e.g. librpm
        // think it can do domain transitions to rpm_exec_t, which
        // isn't actually permitted.
        let sysfs_selinux = "/sys/fs/selinux";
        if Path::new(sysfs_selinux).join("status").exists() {
            rbind("/usr/share/empty", sysfs_selinux)?;
        }

        let ostree_based_host = std::path::Path::new("/host/run/ostree-booted").exists();

        // Propagate standard mount points into the container.
        // We make these bind mounts instead of symlinks as
        // some programs get confused by absolute paths.
        if ostree_based_host {
            DATADIRS.par_iter()
                .try_for_each(|d| -> Fallible<()> {
                    let vard = format!("var{}", d);
                    let hostd = format!("/host/{}", &vard);
                    rbind(&hostd, &vard)?;
                    Ok(())
                })?;
        } else {
            DATADIRS.par_iter()
                .try_for_each(|d| -> Fallible<()> {
                    let hostd = format!("/host/{}", d);
                    rbind(&hostd, d)?;
                    Ok(())
                })?;
        }

        Ok(())
    }


    pub(crate) fn exec() -> Fallible<()> {
        use nix::sys::stat::Mode;
        if !super::in_container() {
            bail!("Not inside a container");
        }
        init_container_static().with_context(|e| format!("Initializing container (static): {}", e))?;
        init_container_runtime().with_context(|e| format!("Initializing container (runtime): {}", e))?;
        let initstamp = Path::new(CONTAINER_INITIALIZED_STAMP);
        if !initstamp.exists() {
            bail!("toolbox not initialized");
        }
        // Set a sane umask (022) by default; something seems to be setting it to 077
        nix::sys::stat::umask(Mode::S_IWGRP | Mode::S_IWOTH);
        let username = super::getenv_required_utf8("USER")?;
        let su_preserved_env_arg =
            format!("--whitelist-environment={}", super::PRESERVED_ENV.join(","));
        Err(Command::new("setpriv")
            .args(&[
                "--inh-caps=-all",
                "su",
                su_preserved_env_arg.as_str(),
                "-",
                &username,
            ])
            .env_remove("TOOLBOX_STATEFILE")
            .exec()
            .into())
    }
}

/// Primary entrypoint
fn main() {
    || -> Fallible<()> {
        let mut args : Vec<String> = std::env::args().collect();
        if let Some("internals") = args.get(1).map(|s| s.as_str()) {
            args.remove(1);
            let opts = InternalOpt::from_iter(args.iter());
            match opts {
                InternalOpt::Exec => entrypoint::exec(),
                InternalOpt::RunPid1 => run_pid1(opts),
            }
        } else {
            let opts = Opt::from_iter(args.iter());
            match opts {
                Opt::Create(ref opts) => create(opts),
                Opt::Run(ref opts) => run(opts),
                Opt::Rm(ref opts) => rm(opts),
            }
        }
    }()
    .unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1)
    })
}
