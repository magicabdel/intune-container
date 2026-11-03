//! Pure-Rust rootless container runtime.
//!
//! Creates an unprivileged user namespace (no sudo), plus mount/pid/uts
//! namespaces, sets up `/proc`, `/dev`, `/sys` and `/run`, `pivot_root`s into a
//! rootfs, and boots the rootfs's systemd as PID 1. The host network namespace
//! is kept, so no slirp is needed.
//!
//! ## Where the container's cgroup comes from
//!
//! systemd as PID 1 needs a cgroup-v2 subtree it can write. On a desktop the
//! user's systemd manager hands one over: a transient scope with `Delegate=yes`.
//! Inside a Docker container or a Kubernetes pod there is no user manager at all,
//! so the runtime makes the scope itself, as a child of the cgroup it already
//! runs in — which only works when whoever started *that* (the container's
//! entrypoint, run as root) handed this uid its own cgroup first. See
//! [`CGROUP_ENV`] and [`delegate_cgroup`].
//!
//! ## Security profiles
//!
//! Callers pass `hardened` to select a profile, chosen automatically from
//! whether the host display is attached:
//!
//! - **hardened** (headless): also unshares the IPC namespace and the delegated
//!   scope caps memory as well as tasks.
//! - **compat** (interactive GUI): keeps the host IPC namespace so XWayland's
//!   MIT-SHM works for forwarded GUI apps; the scope caps tasks only.
//!
//! IMPORTANT: `unshare(CLONE_NEWUSER)` requires a single-threaded process, so we
//! `fork()` first and do all namespace work in the (single-threaded) child. A
//! production caller must therefore launch this from a freshly-exec'd process,
//! never directly from the multithreaded GUI/tokio runtime.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::pty::{openpty, Winsize};
use nix::sched::{unshare, CloneFlags};
use nix::sys::signal::{killpg, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{
    chdir, chroot, dup2, execv, fork, getgid, getuid, sethostname, setsid, ForkResult, Pid,
};

/// Run `argv` (argv[0] must be an absolute path inside the rootfs, e.g.
/// `/bin/sh`) inside a fresh rootless container built from `rootfs`. Returns the
/// command's exit code.
pub fn run_in_rootfs(rootfs: &Path, argv: &[&str]) -> Result<i32> {
    // Fork so the namespace setup runs single-threaded (required for NEWUSER).
    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => match waitpid(child, None).context("waitpid")? {
            WaitStatus::Exited(_, code) => Ok(code),
            WaitStatus::Signaled(_, sig, _) => Ok(128 + sig as i32),
            other => anyhow::bail!("unexpected wait status: {other:?}"),
        },
        ForkResult::Child => {
            let code = match container_parent(rootfs, argv) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("rootless runtime error: {e:#}");
                    127
                }
            };
            unsafe { nix::libc::_exit(code) }
        }
    }
}

/// Single-threaded child: create the user namespace, write id maps, create the
/// remaining namespaces, then fork the container's PID 1 and wait for it.
fn container_parent(rootfs: &Path, argv: &[&str]) -> Result<i32> {
    let uid = getuid();
    let gid = getgid();

    unshare(CloneFlags::CLONE_NEWUSER).context("unshare user namespace")?;
    // Map container root -> our uid/gid (single id; multi-id ranges need
    // newuidmap/newgidmap and come with the systemd milestone).
    std::fs::write("/proc/self/setgroups", "deny").ok();
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).context("write uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).context("write gid_map")?;

    unshare(
        CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWPID
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWIPC,
    )
    .context("unshare mount/pid/uts/ipc namespaces")?;

    // The next fork's child is PID 1 in the new PID namespace.
    match unsafe { fork() }.context("fork pid 1")? {
        ForkResult::Parent { child } => match waitpid(child, None).context("waitpid pid1")? {
            WaitStatus::Exited(_, code) => Ok(code),
            WaitStatus::Signaled(_, sig, _) => Ok(128 + sig as i32),
            other => anyhow::bail!("unexpected wait status: {other:?}"),
        },
        ForkResult::Child => {
            if let Err(e) = init_and_exec(rootfs, argv) {
                eprintln!("container init error: {e:#}");
                unsafe { nix::libc::_exit(127) }
            }
            unreachable!()
        }
    }
}

/// PID 1 inside the container: set up mounts, chroot into the rootfs, exec.
fn init_and_exec(rootfs: &Path, argv: &[&str]) -> Result<()> {
    // Detach mount propagation from the host.
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .context("make / private")?;

    // Fresh /proc (reflects our PID namespace).
    let proc = rootfs.join("proc");
    std::fs::create_dir_all(&proc).ok();
    mount(
        Some("proc"),
        &proc,
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .context("mount /proc")?;

    // Bind the host /dev and /sys (rootless: reuse the host nodes).
    for (src, sub) in [("/dev", "dev"), ("/sys", "sys")] {
        let target = rootfs.join(sub);
        std::fs::create_dir_all(&target).ok();
        mount(
            Some(src),
            &target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .with_context(|| format!("bind {src}"))?;
    }

    // Writable tmpfs on /run and /tmp.
    for sub in ["run", "tmp"] {
        let target = rootfs.join(sub);
        std::fs::create_dir_all(&target).ok();
        mount(
            Some("tmpfs"),
            &target,
            Some("tmpfs"),
            MsFlags::empty(),
            None::<&str>,
        )
        .with_context(|| format!("mount tmpfs on /{sub}"))?;
    }

    chdir(rootfs).context("chdir to rootfs")?;
    chroot(".").context("chroot")?;
    chdir("/").context("chdir /")?;

    let _ = sethostname("intune");

    let prog = CString::new(argv[0]).context("argv[0] contains a NUL")?;
    let args: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(*a))
        .collect::<std::result::Result<_, _>>()
        .context("argv contains a NUL")?;

    match execv(&prog, &args) {
        Ok(_) => unreachable!("execv returned Ok"),
        Err(e) => Err(anyhow::Error::from(e)).with_context(|| format!("execv {}", argv[0])),
    }
}

// ===== systemd boot (Phase 3) + exec/attach (Phase 4) =====

/// A running rootless container: the supervisor (intermediate child that reaps
/// PID 1) and the leader (the container's PID 1 / systemd) host pids.
pub struct Container {
    supervisor: Pid,
    leader: Pid,
    pub scope: String,
}

/// Boot the rootfs's `/sbin/init` (systemd) as PID 1 in a rootless container and
/// **return once it's launched**, leaving it running. Use [`Container::exec`] to
/// run programs inside it and [`Container::stop`] to shut it down.
pub fn start_systemd(
    rootfs: &Path,
    binds: &[(PathBuf, PathBuf)],
    log: Option<&Path>,
    hardened: bool,
) -> Result<Container> {
    let uid = getuid().as_raw();
    let gid = getgid().as_raw();

    let log_file = match log {
        Some(p) => Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .context("open boot log")?,
        ),
        None => None,
    };
    let log_fd = log_file.as_ref().map(|f| f.as_raw_fd());

    // ready: child -> parent (userns created); go: parent -> child (maps+scope
    // done); gpid: child -> parent (PID 1's host pid).
    let (ready_r, ready_w) = make_pipe()?;
    let (go_r, go_w) = make_pipe()?;
    let (gpid_r, gpid_w) = make_pipe()?;

    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => {
            close_fd(ready_w);
            close_fd(go_r);
            close_fd(gpid_w);
            let mut b = [0u8; 1];
            let n = unsafe { nix::libc::read(ready_r, b.as_mut_ptr() as *mut _, 1) };
            close_fd(ready_r);
            if n != 1 {
                // The child `_exit`s before saying it is ready only when its
                // `unshare` was refused. Report THAT, rather than going on to map
                // ids into a process that no longer exists.
                close_fd(go_w);
                close_fd(gpid_r);
                let status = waitpid(child, None);
                anyhow::bail!(
                    "could not create the container's namespaces ({status:?}): {}",
                    NAMESPACES_REFUSED
                );
            }
            let scope = format!("intune-{}.scope", child.as_raw());
            let setup = set_id_maps(child, uid, gid)
                .and_then(|_| delegate_cgroup(child.as_raw(), &scope, hardened));
            if let Err(e) = setup {
                // Never release a child we could not set up: it would boot
                // systemd outside any cgroup of its own, publish no state, and
                // keep running as an orphan nobody can find or stop.
                abort_child(child, go_w, gpid_r);
                return Err(e);
            }
            let _ = unsafe { nix::libc::write(go_w, b.as_ptr() as *const _, 1) };
            close_fd(go_w);
            // Receive the container PID 1's host pid.
            let mut pbuf = [0u8; 4];
            let n = unsafe { nix::libc::read(gpid_r, pbuf.as_mut_ptr() as *mut _, 4) };
            close_fd(gpid_r);
            if n != 4 {
                anyhow::bail!("container failed to start (no PID 1)");
            }
            let leader = Pid::from_raw(i32::from_ne_bytes(pbuf));
            Ok(Container {
                supervisor: child,
                leader,
                scope,
            })
        }
        ForkResult::Child => {
            close_fd(ready_r);
            close_fd(go_w);
            close_fd(gpid_r);
            let _ = setsid();
            // We deliberately keep the host network namespace (the broker needs
            // egress and the GUI path reaches the host display sockets).
            //
            // IPC is profile-dependent: the `hardened` (headless) profile
            // unshares it for a private SysV/POSIX shm + /dev/shm. The `compat`
            // profile keeps the host IPC namespace so XWayland's MIT-SHM
            // ShmAttach succeeds for forwarded GUI apps (a private IPC ns makes
            // the host X server unable to attach the container's shared-memory
            // segment → fatal BadAccess in GTK). This matches distrobox/toolbox.
            let mut flags = CloneFlags::CLONE_NEWUSER
                | CloneFlags::CLONE_NEWNS
                | CloneFlags::CLONE_NEWPID
                | CloneFlags::CLONE_NEWUTS;
            if hardened {
                flags |= CloneFlags::CLONE_NEWIPC;
            }
            if unshare(flags).is_err() {
                unsafe { nix::libc::_exit(126) }
            }
            let b = [1u8; 1];
            let _ = unsafe { nix::libc::write(ready_w, b.as_ptr() as *const _, 1) };
            close_fd(ready_w);
            let mut r = [0u8; 1];
            let n = unsafe { nix::libc::read(go_r, r.as_mut_ptr() as *mut _, 1) };
            close_fd(go_r);
            if n != 1 {
                // The parent closed `go` without writing: the id maps or the
                // cgroup could not be set up, so this boot is abandoned.
                unsafe { nix::libc::_exit(124) }
            }

            // In the delegated scope now: pin it as our cgroup-ns root.
            let _ = unshare(CloneFlags::CLONE_NEWCGROUP);

            match unsafe { fork() } {
                Ok(ForkResult::Parent { child: pid1 }) => {
                    // Relay PID 1's host pid to the launcher, then supervise it.
                    let pb = pid1.as_raw().to_ne_bytes();
                    let _ = unsafe { nix::libc::write(gpid_w, pb.as_ptr() as *const _, 4) };
                    close_fd(gpid_w);
                    let code = match waitpid(pid1, None) {
                        Ok(WaitStatus::Exited(_, c)) => c,
                        Ok(WaitStatus::Signaled(_, s, _)) => 128 + s as i32,
                        _ => 1,
                    };
                    unsafe { nix::libc::_exit(code) }
                }
                Ok(ForkResult::Child) => {
                    if let Err(e) = init_systemd(rootfs, binds, log_fd) {
                        eprintln!("container init error: {e:#}");
                        unsafe { nix::libc::_exit(127) }
                    }
                    unreachable!()
                }
                Err(_) => unsafe { nix::libc::_exit(125) },
            }
        }
    }
}

/// Boot systemd and block until it exits (e.g. when something powers it off).
/// Convenience wrapper over [`start_systemd`] + [`Container::wait`].
pub fn boot_systemd(
    rootfs: &Path,
    binds: &[(PathBuf, PathBuf)],
    log: Option<&Path>,
    hardened: bool,
) -> Result<i32> {
    start_systemd(rootfs, binds, log, hardened)?.wait()
}

impl Container {
    /// Run `argv` (absolute path) inside the running container by joining its
    /// namespaces with `setns` (no sudo — we own the user namespace). When
    /// `as_uid` is set, drop to that in-container uid/gid before exec.
    pub fn exec(&self, argv: &[&str], as_uid: Option<u32>) -> Result<i32> {
        exec_pid(self.leader.as_raw(), argv, as_uid)
    }

    /// The host pid of the container's PID 1 (systemd). This is the handle a
    /// separate process needs in order to `setns` into the running container.
    pub fn leader_pid(&self) -> i32 {
        self.leader.as_raw()
    }

    /// A serializable snapshot of this container's runtime handles, so another
    /// process (e.g. a later CLI invocation) can find and re-enter it.
    pub fn state(&self) -> RuntimeState {
        RuntimeState {
            leader: self.leader.as_raw(),
            scope: self.scope.clone(),
            start_time: process_start_time(self.leader.as_raw()),
        }
    }

    /// Ask the container's systemd to power off (SIGRTMIN+4), then reap it.
    pub fn stop(self) -> Result<i32> {
        poweroff_pid(self.leader.as_raw());
        // Fall back to SIGKILLing the whole group if it doesn't exit promptly.
        spawn_watchdog(self.supervisor, Duration::from_secs(20));
        self.wait()
    }

    /// Wait for the container to exit and return its code.
    pub fn wait(self) -> Result<i32> {
        match waitpid(self.supervisor, None).context("waitpid supervisor")? {
            WaitStatus::Exited(_, code) => Ok(code),
            WaitStatus::Signaled(_, sig, _) => Ok(128 + sig as i32),
            other => anyhow::bail!("unexpected wait status: {other:?}"),
        }
    }
}

/// Run `argv` inside a running container identified by its PID 1 host pid
/// (`leader`), joining its namespaces with `setns`. This is the cross-process
/// entry point: a later CLI/GUI invocation loads the leader pid from saved
/// state and uses this to enter the container without a [`Container`] handle.
pub fn exec_pid(leader: i32, argv: &[&str], as_uid: Option<u32>) -> Result<i32> {
    exec_pid_env(leader, argv, as_uid, &[])
}

/// Like [`exec_pid`], but also sets `env` (name, value) on the launched process
/// — used to hand a GUI app its display environment (DISPLAY, WAYLAND_DISPLAY,
/// XDG_RUNTIME_DIR, XAUTHORITY).
pub fn exec_pid_env(
    leader: i32,
    argv: &[&str],
    as_uid: Option<u32>,
    env: &[(String, String)],
) -> Result<i32> {
    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => match waitpid(child, None).context("waitpid")? {
            WaitStatus::Exited(_, code) => Ok(code),
            WaitStatus::Signaled(_, sig, _) => Ok(128 + sig as i32),
            other => anyhow::bail!("unexpected wait status: {other:?}"),
        },
        ForkResult::Child => {
            let code = match enter_and_exec(leader, argv, as_uid, env) {
                Ok(()) => unreachable!(),
                Err(e) => {
                    eprintln!("exec error: {e:#}");
                    127
                }
            };
            unsafe { nix::libc::_exit(code) }
        }
    }
}

/// Whether a container with PID 1 host pid `leader` is still alive: the process
/// exists, it is PID 1 of a pid namespace of its own, and that namespace is not
/// ours.
///
/// Existing is not enough. A pid is recycled, and inside a Docker container or a
/// Kubernetes pod the pid namespace starts again at 1 on every restart while the
/// saved state survives on a volume — so the leader a previous run published is,
/// a few pids later, an ordinary process of this one. Read as "already running",
/// it would make every start a no-op and every stop signal a stranger.
pub fn is_running(leader: i32) -> bool {
    if leader <= 1 {
        return false;
    }
    let status = std::fs::read_to_string(format!("/proc/{leader}/status")).unwrap_or_default();
    is_namespace_init(&status) && !shares_namespace(leader, "pid")
}

/// Whether a `/proc/<pid>/status` says the process is PID 1 of a nested pid
/// namespace: its `NSpid` line lists more than one pid, and the innermost is 1.
fn is_namespace_init(status: &str) -> bool {
    status
        .lines()
        .find_map(|l| l.strip_prefix("NSpid:"))
        .map(|pids| {
            let pids: Vec<&str> = pids.split_whitespace().collect();
            pids.len() > 1 && pids.last() == Some(&"1")
        })
        .unwrap_or(false)
}

/// The start time of `pid` in clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`), which tells a process apart from a later one that was
/// handed the same pid.
pub fn process_start_time(pid: i32) -> Option<u64> {
    stat_start_time(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Field 22 of a `/proc/<pid>/stat` line. The command name (field 2) is in
/// parentheses and may itself hold spaces and parentheses, so fields are counted
/// from the LAST `)`.
fn stat_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // `rest` starts at field 3 (state), so field 22 is the 20th one.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Ask the container's systemd (PID 1 host pid `leader`) to power off by sending
/// SIGRTMIN+4, the signal systemd interprets as a poweroff request.
pub fn poweroff_pid(leader: i32) {
    unsafe {
        nix::libc::kill(leader, nix::libc::SIGRTMIN() + 4);
    }
}

/// Enter the running container (by PID 1 host pid `leader`) and run a Rust
/// closure inside it, returning its exit code. Used to run in-process bridges
/// (e.g. the browser native-messaging host) inside the container while keeping
/// the caller's stdin/stdout (the browser pipes) connected through the fork.
///
/// The closure runs in a doubly-forked grandchild that is genuinely inside the
/// container's pid/mount/user namespaces, as `as_uid` (when set), with a clean
/// environment plus `env`. It may build its own async runtime (the grandchild is
/// single-threaded at entry, so the prior `setns` into the user namespace is
/// valid).
pub fn run_in_container<F>(
    leader: i32,
    as_uid: Option<u32>,
    env: &[(String, String)],
    f: F,
) -> Result<i32>
where
    F: FnOnce() -> i32,
{
    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => match waitpid(child, None).context("waitpid")? {
            WaitStatus::Exited(_, code) => Ok(code),
            WaitStatus::Signaled(_, sig, _) => Ok(128 + sig as i32),
            other => anyhow::bail!("unexpected wait status: {other:?}"),
        },
        ForkResult::Child => {
            let code = match enter_and_run(leader, as_uid, env, f) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("run_in_container error: {e:#}");
                    127
                }
            };
            unsafe { nix::libc::_exit(code) }
        }
    }
}

/// Namespaces to `setns` into when entering a running container, in join order
/// (user first, so we hold caps in the container's user ns for the rest).
///
/// IPC is included only when the leader is in a *different* IPC namespace than us
/// (the hardened profile's private ns, owned by the container user ns, which we
/// can join). In the compat profile the container shares the host IPC namespace,
/// owned by the host's *initial* user ns: re-entering it as an unprivileged user
/// fails with `EPERM` (we lack `CAP_SYS_ADMIN` there), and it would be a no-op
/// anyway since we're already in it.
fn ns_join_order(leader: i32) -> Vec<(&'static str, CloneFlags)> {
    let mut order = vec![
        ("user", CloneFlags::CLONE_NEWUSER),
        ("uts", CloneFlags::CLONE_NEWUTS),
    ];
    if !shares_namespace(leader, "ipc") {
        order.push(("ipc", CloneFlags::CLONE_NEWIPC));
    }
    order.push(("pid", CloneFlags::CLONE_NEWPID));
    order.push(("cgroup", CloneFlags::CLONE_NEWCGROUP));
    order.push(("mnt", CloneFlags::CLONE_NEWNS));
    order
}

/// Whether `leader` is in the same `ns` namespace as the calling process. Reads
/// the magic symlink targets (`ipc:[<inode>]`), which are equal iff the two
/// processes share that namespace.
fn shares_namespace(leader: i32, ns: &str) -> bool {
    let ours = std::fs::read_link(format!("/proc/self/ns/{ns}")).ok();
    let theirs = std::fs::read_link(format!("/proc/{leader}/ns/{ns}")).ok();
    ours.is_some() && ours == theirs
}

/// `setns` the calling process into the container's namespaces, in join order.
/// Opens every ns fd first (joining the mount ns changes what `/proc/<leader>`
/// resolves to), then joins each. Must run in a freshly-forked child.
fn join_container_namespaces(leader: i32) -> Result<()> {
    use nix::sched::setns;
    let order = ns_join_order(leader);
    let mut fds = Vec::new();
    for (ns, flag) in order {
        let path = format!("/proc/{leader}/ns/{ns}");
        let f = std::fs::File::open(&path).with_context(|| format!("open {path}"))?;
        fds.push((f, flag));
    }
    for (f, flag) in &fds {
        setns(f, *flag).with_context(|| format!("setns {flag:?}"))?;
    }
    Ok(())
}

/// A pseudo-terminal attached to an interactive `/bin/bash` running inside the
/// container. `master` is the host-side PTY fd (read its output, write input,
/// resize it); `pid` is the host pid of the in-container shell, so the caller
/// can `kill` it to end the session (closing the master also hangs it up).
pub struct ShellPty {
    pub master: RawFd,
    pub pid: i32,
}

/// Open an interactive login shell inside the running container (`leader`) on a
/// fresh PTY and return its host-side handle. The shell runs as `as_uid` with a
/// clean environment plus `env`. Used by the GUI to embed a terminal.
pub fn open_shell_pty(
    leader: i32,
    as_uid: Option<u32>,
    env: &[(String, String)],
    rows: u16,
    cols: u16,
) -> Result<ShellPty> {
    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(Some(&ws), None).context("openpty")?;
    // Relay the in-container shell's *host* pid back to the caller.
    let (pid_r, pid_w) = make_pipe()?;

    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => {
            drop(pty.slave);
            close_fd(pid_w);
            let mut buf = [0u8; 4];
            let n = unsafe { nix::libc::read(pid_r, buf.as_mut_ptr() as *mut _, 4) };
            close_fd(pid_r);
            // Reap the short-lived namespace-entering relay (it exits at once).
            let _ = waitpid(child, None);
            if n != 4 {
                drop(pty.master);
                anyhow::bail!("shell failed to start in the container");
            }
            Ok(ShellPty {
                master: pty.master.into_raw_fd(),
                pid: i32::from_ne_bytes(buf),
            })
        }
        ForkResult::Child => {
            // Relay process: enter the container namespaces, then fork the shell
            // (which lands in the container's pid namespace).
            drop(pty.master);
            close_fd(pid_r);
            let slave = pty.slave.as_raw_fd();
            if join_container_namespaces(leader).is_err() {
                unsafe { nix::libc::_exit(126) }
            }
            match unsafe { fork() } {
                Ok(ForkResult::Parent { child: shell }) => {
                    let b = shell.as_raw().to_ne_bytes();
                    let _ = unsafe { nix::libc::write(pid_w, b.as_ptr() as *const _, 4) };
                    close_fd(pid_w);
                    unsafe { nix::libc::_exit(0) }
                }
                Ok(ForkResult::Child) => {
                    // The shell: own a new session with the PTY as controlling tty.
                    let _ = setsid();
                    unsafe { nix::libc::ioctl(slave, nix::libc::TIOCSCTTY as _, 0) };
                    let _ = dup2(slave, 0);
                    let _ = dup2(slave, 1);
                    let _ = dup2(slave, 2);
                    if slave > 2 {
                        close_fd(slave);
                    }
                    close_fd(pid_w);
                    if let Some(uid) = as_uid {
                        let _ = nix::unistd::setgid(nix::unistd::Gid::from_raw(uid));
                        let _ = nix::unistd::setuid(nix::unistd::Uid::from_raw(uid));
                    }
                    apply_clean_env(env);
                    let home = env
                        .iter()
                        .find(|(k, _)| k == "HOME")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("/");
                    let _ = chdir(home);
                    let prog = CString::new("/bin/bash").unwrap();
                    let args = [
                        CString::new("/bin/bash").unwrap(),
                        CString::new("--login").unwrap(),
                    ];
                    let _ = execv(&prog, &args);
                    unsafe { nix::libc::_exit(127) }
                }
                Err(_) => unsafe { nix::libc::_exit(125) },
            }
        }
    }
}

/// Tell the PTY its new window size (so full-screen programs like `htop` and
/// line editing reflow). Best-effort.
pub fn pty_resize(master: RawFd, rows: u16, cols: u16) {
    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        nix::libc::ioctl(master, nix::libc::TIOCSWINSZ as _, &ws as *const Winsize);
    }
}

/// Join the leader's namespaces, fork into the pid namespace, and run `f` there.
fn enter_and_run<F>(leader: i32, as_uid: Option<u32>, env: &[(String, String)], f: F) -> Result<i32>
where
    F: FnOnce() -> i32,
{
    use nix::sched::setns;

    let order = ns_join_order(leader);
    let mut fds = Vec::new();
    for (ns, flag) in order {
        let path = format!("/proc/{leader}/ns/{ns}");
        let file = std::fs::File::open(&path).with_context(|| format!("open {path}"))?;
        fds.push((file, flag));
    }
    for (file, flag) in &fds {
        setns(file, *flag).with_context(|| format!("setns {flag:?}"))?;
    }

    // Fork so the child is actually in the joined pid namespace.
    match unsafe { fork() }.context("fork into pid ns")? {
        ForkResult::Parent { child } => match waitpid(child, None)? {
            WaitStatus::Exited(_, code) => Ok(code),
            WaitStatus::Signaled(_, sig, _) => Ok(128 + sig as i32),
            _ => Ok(1),
        },
        ForkResult::Child => {
            if let Some(uid) = as_uid {
                let _ = nix::unistd::setgid(nix::unistd::Gid::from_raw(uid));
                let _ = nix::unistd::setuid(nix::unistd::Uid::from_raw(uid));
            }
            apply_clean_env(env);
            let _ = chdir("/");
            let code = f();
            unsafe { nix::libc::_exit(code) }
        }
    }
}

/// Replace the current process environment with a clean container base plus
/// `env`, so the host session (DBUS_SESSION_BUS_ADDRESS, XDG_RUNTIME_DIR, …)
/// never leaks in. Must be called from a single-threaded (post-fork) context.
fn apply_clean_env(env: &[(String, String)]) {
    let keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    for k in keys {
        std::env::remove_var(k);
    }
    std::env::set_var(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
    std::env::set_var("TERM", "xterm");
    std::env::set_var("container", "intune-container");
    for (k, v) in env {
        std::env::set_var(k, v);
    }
}

/// Join the leader's namespaces and exec. Must run in a forked child (the pid-ns
/// join only affects subsequently-forked processes).
fn enter_and_exec(
    leader: i32,
    argv: &[&str],
    as_uid: Option<u32>,
    env: &[(String, String)],
) -> Result<()> {
    use nix::sched::setns;

    // Open all ns fds first — after we join the mount ns, /proc/<leader> changes.
    let order = ns_join_order(leader);
    let mut fds = Vec::new();
    for (ns, flag) in order {
        let path = format!("/proc/{leader}/ns/{ns}");
        match std::fs::File::open(&path) {
            Ok(f) => fds.push((f, flag)),
            Err(e) => return Err(anyhow::Error::from(e)).with_context(|| format!("open {path}")),
        }
    }
    for (f, flag) in &fds {
        setns(f, *flag).with_context(|| format!("setns {flag:?}"))?;
    }

    // After joining the PID namespace, fork so the child is actually in it.
    match unsafe { fork() }.context("fork into pid ns")? {
        ForkResult::Parent { child } => match waitpid(child, None)? {
            WaitStatus::Exited(_, code) => unsafe { nix::libc::_exit(code) },
            WaitStatus::Signaled(_, sig, _) => unsafe { nix::libc::_exit(128 + sig as i32) },
            _ => unsafe { nix::libc::_exit(1) },
        },
        ForkResult::Child => {
            if let Some(uid) = as_uid {
                let _ = nix::unistd::setgid(nix::unistd::Gid::from_raw(uid));
                let _ = nix::unistd::setuid(nix::unistd::Uid::from_raw(uid));
            }
            // Start from a clean environment so the host session does not leak
            // into the container (a stale DBUS_SESSION_BUS_ADDRESS / XDG_RUNTIME_DIR
            // would point the app at a bus that doesn't exist inside, breaking the
            // Secret Service — and thus device-key storage / device info).
            apply_clean_env(env);
            let _ = chdir("/");
            let prog = CString::new(argv[0]).unwrap();
            let args: Vec<CString> = argv.iter().map(|a| CString::new(*a).unwrap()).collect();
            match execv(&prog, &args) {
                Ok(_) => unreachable!(),
                Err(e) => Err(anyhow::Error::from(e)).with_context(|| format!("execv {}", argv[0])),
            }
        }
    }
}

/// PID 1 inside the container: mounts, `pivot_root`, stdio, exec systemd.
fn init_systemd(rootfs: &Path, binds: &[(PathBuf, PathBuf)], log_fd: Option<RawFd>) -> Result<()> {
    let nosuid = MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV;

    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .context("make / private")?;

    // pivot_root needs new_root to be a mount point: bind the rootfs onto itself.
    mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .context("bind rootfs")?;

    // Fresh /proc for our PID namespace.
    let proc = rootfs.join("proc");
    std::fs::create_dir_all(&proc).ok();
    mount(Some("proc"), &proc, Some("proc"), nosuid, None::<&str>).context("mount /proc")?;

    // /dev: bind the host nodes recursively (brings /dev/pts, /dev/shm, …).
    let dev = rootfs.join("dev");
    std::fs::create_dir_all(&dev).ok();
    mount(
        Some("/dev"),
        &dev,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .context("bind /dev")?;
    // /sys: must be recursive inside a user namespace (the host's submounts are
    // locked together; a non-recursive bind is rejected with EINVAL).
    let sys = rootfs.join("sys");
    std::fs::create_dir_all(&sys).ok();
    mount(
        Some("/sys"),
        &sys,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .context("bind /sys")?;

    // A namespaced cgroup2 view (rooted at our delegated scope) over /sys/fs/cgroup
    // so systemd manages its own subtree. The recursive /sys bind carried in a
    // *locked* host cgroup mount (can't be detached in a userns), so first mask it
    // with a tmpfs, then mount a fresh cgroup2 over that clean mountpoint.
    let cgroup = rootfs.join("sys/fs/cgroup");
    let _ = mount(
        Some("tmpfs"),
        &cgroup,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
        Some("mode=0755"),
    );
    if let Err(e) = mount(
        Some("cgroup2"),
        &cgroup,
        Some("cgroup2"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
        None::<&str>,
    ) {
        eprintln!("warning: could not mount cgroup2 ({e}); systemd may run degraded");
    }

    // Writable tmpfs for /run and /tmp.
    for (sub, mode) in [("run", "mode=0755"), ("tmp", "mode=1777")] {
        let target = rootfs.join(sub);
        std::fs::create_dir_all(&target).ok();
        mount(
            Some("tmpfs"),
            &target,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some(mode),
        )
        .with_context(|| format!("mount tmpfs /{sub}"))?;
    }

    // Extra binds (probe directory, display sockets, …). Created on the freshly-
    // mounted tmpfs. A directory source needs a directory mountpoint; a file or
    // socket source needs an empty file as its mountpoint.
    for (src, dst) in binds {
        let rel = dst.strip_prefix("/").unwrap_or(dst);
        let target = rootfs.join(rel);
        let src_is_dir = std::fs::metadata(src).map(|m| m.is_dir()).unwrap_or(true);
        if src_is_dir {
            std::fs::create_dir_all(&target).ok();
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if !target.exists() {
                let _ = std::fs::File::create(&target);
            }
        }
        mount(
            Some(src.as_path()),
            &target,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .with_context(|| format!("bind {}", src.display()))?;
    }

    // DNS for the broker (best-effort; host networking is shared).
    let _ = std::fs::copy("/etc/resolv.conf", rootfs.join("etc/resolv.conf"));

    // pivot_root into the rootfs and detach the old root.
    chdir(rootfs).context("chdir rootfs")?;
    let oldroot = rootfs.join(".oldroot");
    std::fs::create_dir_all(&oldroot).ok();
    pivot_root(rootfs, &oldroot).context("pivot_root")?;
    chdir("/").context("chdir /")?;
    let _ = umount2("/.oldroot", MntFlags::MNT_DETACH);
    let _ = std::fs::remove_dir("/.oldroot");

    let _ = sethostname("intune");

    // Capture systemd's own logs by pointing /dev/console at our log file and
    // asking systemd to log to the console.
    let mut systemd_args = vec![CString::new("/sbin/init").unwrap()];
    if let Some(fd) = log_fd {
        let src = format!("/proc/self/fd/{fd}");
        let _ = mount(
            Some(src.as_str()),
            "/dev/console",
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        );
        systemd_args.push(CString::new("--log-target=console").unwrap());
        systemd_args.push(CString::new("--log-level=info").unwrap());
        let _ = dup2(fd, 1);
        let _ = dup2(fd, 2);
    }
    if let Ok(devnull) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        let _ = dup2(devnull.as_raw_fd(), 0);
    }

    // PID 1 starts with nothing it did not ask for. Whatever launched the
    // supervisor — a desktop session, a service, an app that embeds this crate —
    // would otherwise hand the container's init its whole environment, readable
    // by root inside the container at /proc/1/environ for the container's life:
    // session bus addresses, tokens, credentials.
    for (key, _) in std::env::vars_os() {
        std::env::remove_var(key);
    }
    // Tell systemd it's in a container so it uses container mode (skips host-only
    // setup). Any non-empty value works; this matches the nspawn/podman pattern.
    std::env::set_var("container", "intune-container");

    // Sentinel so we can confirm log capture even if systemd logs elsewhere.
    if let Some(fd) = log_fd {
        let msg = b"=== exec /sbin/init (rootless) ===\n";
        unsafe {
            nix::libc::write(fd, msg.as_ptr() as *const _, msg.len());
        }
    }

    let prog = CString::new("/sbin/init").unwrap();
    match execv(&prog, &systemd_args) {
        Ok(_) => unreachable!("execv returned Ok"),
        Err(e) => Err(anyhow::Error::from(e)).context("execv /sbin/init"),
    }
}

/// How many ids the container maps above its root: 1..=65535, the whole range a
/// distribution's users and groups live in (`nobody` is 65534).
const CONTAINER_IDS: u32 = 65535;

/// Map container ids using the `newuidmap`/`newgidmap` helpers: id 0 maps to the
/// caller, ids 1..=65535 to the start of the caller's own range in `/etc/subuid`
/// and `/etc/subgid`.
///
/// The range is READ, never assumed. It used to be a hard-coded 100000, which is
/// only the most common default: a host that allocated the user another range —
/// and every container or pod whose uids are themselves remapped, where 100000 is
/// not a uid at all — refused the map with nothing saying why.
fn set_id_maps(child: Pid, uid: u32, gid: u32) -> Result<()> {
    let pid = child.as_raw().to_string();
    let user = login_name(uid);
    let run = |bin: &str, file: &str, owner: u32| -> Result<()> {
        let start = subordinate_start(file, &user, owner)?;
        let status = std::process::Command::new(bin)
            .args([
                pid.as_str(),
                "0",
                &owner.to_string(),
                "1",
                "1",
                &start.to_string(),
                &CONTAINER_IDS.to_string(),
            ])
            .status()
            .with_context(|| format!("run {bin} (is the uidmap package installed?)"))?;
        if !status.success() {
            anyhow::bail!(
                "{bin} refused to map {CONTAINER_IDS} ids from {start} ({file}): it needs \
                 to be setuid root or to carry the cap_setuid/cap_setgid file capability, \
                 and it cannot gain either under no_new_privs"
            );
        }
        Ok(())
    };
    run("newuidmap", SUBUID, uid)?;
    run("newgidmap", SUBGID, gid)?;
    Ok(())
}

const SUBUID: &str = "/etc/subuid";
const SUBGID: &str = "/etc/subgid";

/// The first id of `user`'s range in `file` (`/etc/subuid` or `/etc/subgid`),
/// refusing a range too small to hold a distribution's ids.
fn subordinate_start(file: &str, user: &str, owner: u32) -> Result<u32> {
    let contents = std::fs::read_to_string(file).with_context(|| format!("read {file}"))?;
    let (start, count) = subid_range(&contents, user, owner).with_context(|| {
        format!(
            "{file} has no range for {user} (uid {owner}): run usermod --add-subuids/--add-subgids"
        )
    })?;
    if count < CONTAINER_IDS {
        anyhow::bail!(
            "{file} gives {user} {count} ids from {start}; the container needs {CONTAINER_IDS}"
        );
    }
    Ok(start)
}

/// The `(start, count)` of the first `name:start:count` line of a subuid/subgid
/// file that names `user` or the numeric `uid` — the two spellings `newuidmap`
/// itself accepts.
fn subid_range(contents: &str, user: &str, uid: u32) -> Option<(u32, u32)> {
    let uid = uid.to_string();
    contents.lines().find_map(|line| {
        let mut f = line.trim().split(':');
        let name = f.next()?;
        if name.is_empty() || (name != user && name != uid) {
            return None;
        }
        let start = f.next()?.parse().ok()?;
        let count = f.next()?.parse().ok()?;
        Some((start, count))
    })
}

/// The login name of `uid`, the way `newuidmap` resolves it (the password
/// database), falling back to `$USER` and then to the number itself. `$USER`
/// alone is not enough: a container image rarely sets it.
fn login_name(uid: u32) -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| uid.to_string())
}

/// What a refused `unshare` means, wherever it surfaces.
const NAMESPACES_REFUSED: &str = "unprivileged user namespaces are refused here \
     (inside Docker or Kubernetes the default seccomp profile refuses unshare, mount and \
     pivot_root; an AppArmor policy or a sysctl can too)";

/// Abandon a child the parent could not finish setting up: kill it, close the
/// pipes it waits on, and reap it so nothing is left behind.
fn abort_child(child: Pid, go_w: RawFd, gpid_r: RawFd) {
    let _ = nix::sys::signal::kill(child, Signal::SIGKILL);
    close_fd(go_w);
    close_fd(gpid_r);
    let _ = waitpid(child, None);
}

/// Remove `path` recursively, even when it contains files owned by our subuids
/// (e.g. the rootless persistence store, whose keyring dir is chowned to the
/// container user). Does the removal inside a user namespace where we map the
/// subuid range and act as root, so no host privilege is needed.
pub fn remove_tree_as_root(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let uid = getuid().as_raw();
    let gid = getgid().as_raw();
    let (ready_r, ready_w) = make_pipe()?;
    let (go_r, go_w) = make_pipe()?;

    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => {
            close_fd(ready_w);
            close_fd(go_r);
            let mut b = [0u8; 1];
            let n = unsafe { nix::libc::read(ready_r, b.as_mut_ptr() as *mut _, 1) };
            close_fd(ready_r);
            if n != 1 {
                close_fd(go_w);
                let status = waitpid(child, None);
                anyhow::bail!(
                    "could not remove {} ({status:?}): {NAMESPACES_REFUSED}",
                    path.display()
                );
            }
            if let Err(e) = set_id_maps(child, uid, gid) {
                let _ = nix::sys::signal::kill(child, Signal::SIGKILL);
                close_fd(go_w);
                let _ = waitpid(child, None);
                return Err(e);
            }
            let _ = unsafe { nix::libc::write(go_w, b.as_ptr() as *const _, 1) };
            close_fd(go_w);
            match waitpid(child, None).context("waitpid")? {
                WaitStatus::Exited(_, 0) => Ok(()),
                WaitStatus::Exited(_, code) => {
                    anyhow::bail!("failed to remove {} (code {code})", path.display())
                }
                other => anyhow::bail!("unexpected wait status: {other:?}"),
            }
        }
        ForkResult::Child => {
            close_fd(ready_r);
            close_fd(go_w);
            let code = if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
                126
            } else {
                let b = [1u8; 1];
                let _ = unsafe { nix::libc::write(ready_w, b.as_ptr() as *const _, 1) };
                close_fd(ready_w);
                let mut r = [0u8; 1];
                let n = unsafe { nix::libc::read(go_r, r.as_mut_ptr() as *mut _, 1) };
                close_fd(go_r);
                if n != 1 {
                    // The parent could not map our ids: removing as an unmapped
                    // root would fail on every subuid-owned file anyway.
                    124
                } else {
                    // Now uid 0 in the namespace (mapped to the host user), with
                    // the subuid range mapped too, so we can unlink subuid-owned
                    // files.
                    match std::fs::remove_dir_all(path) {
                        Ok(()) => 0,
                        Err(_) => 1,
                    }
                }
            };
            unsafe { nix::libc::_exit(code) }
        }
    }
}

fn pivot_root(new_root: &Path, put_old: &Path) -> nix::Result<()> {
    let new = CString::new(new_root.as_os_str().as_bytes()).unwrap();
    let old = CString::new(put_old.as_os_str().as_bytes()).unwrap();
    let ret = unsafe { nix::libc::syscall(nix::libc::SYS_pivot_root, new.as_ptr(), old.as_ptr()) };
    nix::errno::Errno::result(ret).map(drop)
}

fn make_pipe() -> Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    if unsafe { nix::libc::pipe(fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("pipe() failed");
    }
    Ok((fds[0], fds[1]))
}

fn close_fd(fd: RawFd) {
    unsafe {
        nix::libc::close(fd);
    }
}

fn spawn_watchdog(group_leader: Pid, after: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(after);
        // Signal the whole process group (container + PID 1).
        let _ = killpg(group_leader, Signal::SIGKILL);
    });
}

/// The environment variable that chooses where the container's cgroup comes
/// from:
///
/// - `user-manager` — a transient scope from the user's systemd manager over the
///   session bus, the desktop case. A manager that refuses is an error.
/// - `own` — a child of the cgroup this process already runs in, made by this
///   runtime. Inside a Docker container or a Kubernetes pod there is no user
///   manager, and this is the only way in: it needs that cgroup to be writable
///   by this uid, which the container's entrypoint (run as root) arranges by
///   chowning it and its `cgroup.procs`, `cgroup.threads` and
///   `cgroup.subtree_control` before dropping to this uid.
/// - unset or `auto` — the user manager when one answers, and `own` only when
///   there is NO user manager to ask (no session bus, or nothing owning
///   `org.freedesktop.systemd1` on it). A manager that is there and refuses is
///   still an error: falling back then would put the container inside whatever
///   cgroup the caller happens to run in — a terminal's scope, say — to be
///   killed with it.
///
/// An embedding application that knows it runs inside a container should set
/// `own`, so the outcome never depends on a session bus it may have inherited.
pub const CGROUP_ENV: &str = "INTUNE_CONTAINER_CGROUP";

/// Where the container's cgroup comes from (see [`CGROUP_ENV`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delegation {
    Auto,
    UserManager,
    Own,
}

/// Read [`CGROUP_ENV`]. An unknown value is refused rather than guessed at.
fn delegation_from(value: Option<&str>) -> Result<Delegation> {
    match value.map(str::trim) {
        None | Some("") | Some("auto") => Ok(Delegation::Auto),
        Some("user-manager") => Ok(Delegation::UserManager),
        Some("own") => Ok(Delegation::Own),
        Some(other) => {
            anyhow::bail!("{CGROUP_ENV}={other:?}: expected user-manager, own or auto")
        }
    }
}

/// Put `pid` (the container's intermediate process, not yet in its cgroup
/// namespace) into a cgroup the container's systemd can write, named `scope`.
fn delegate_cgroup(pid: i32, scope: &str, hardened: bool) -> Result<()> {
    let mode = delegation_from(std::env::var(CGROUP_ENV).ok().as_deref())?;
    match mode {
        Delegation::Own => own_scope(pid, scope),
        Delegation::UserManager => {
            create_delegated_scope(pid, scope, hardened).map_err(ScopeError::into_error)?;
            wait_in_scope(pid, scope);
            Ok(())
        }
        Delegation::Auto => match create_delegated_scope(pid, scope, hardened) {
            Ok(()) => {
                wait_in_scope(pid, scope);
                Ok(())
            }
            Err(ScopeError::NoUserManager(why)) => {
                tracing::info!(
                    "no systemd user manager to delegate a cgroup ({why:#}); using a child of our own cgroup"
                );
                own_scope(pid, scope).with_context(|| {
                    format!("there is no systemd user manager here ({why:#}), and")
                })
            }
            Err(e) => Err(e.into_error()),
        },
    }
}

/// Why the user manager could not provide a scope.
#[derive(Debug)]
enum ScopeError {
    /// There is no user manager to ask: no session bus, or nothing owns
    /// `org.freedesktop.systemd1` on it.
    NoUserManager(anyhow::Error),
    /// A user manager answered, and refused.
    Refused(anyhow::Error),
}

impl ScopeError {
    fn into_error(self) -> anyhow::Error {
        match self {
            ScopeError::NoUserManager(e) => {
                e.context("no systemd user manager to delegate the container's cgroup")
            }
            ScopeError::Refused(e) => e,
        }
    }
}

/// Whether a D-Bus error name means nobody is there to answer, as opposed to
/// somebody answering no.
fn means_no_manager(error_name: &str) -> bool {
    matches!(
        error_name,
        "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.UnknownObject"
    )
}

/// Ask the user's systemd manager (over the session bus) to place `pid` into a
/// transient scope (`scope`) with cgroup **delegation**, so a `cgroup2` mounted
/// inside is writable and systemd can manage its own subtree.
///
/// Both profiles cap the task count (fork-bomb / runaway containment) at a level
/// generous enough for the GUI path (a browser spawns many helpers). The
/// `hardened` profile additionally caps memory: the headless daemon (broker +
/// compliance agent) is light, so a hard ceiling contains a runaway without
/// risking the OOM-kill of a legitimate forwarded browser.
fn create_delegated_scope(pid: i32, scope: &str, hardened: bool) -> Result<(), ScopeError> {
    // Process-count ceiling, both profiles. High enough for a browser's helpers.
    const TASKS_MAX: u64 = 8192;
    // Memory ceilings, hardened profile only. `High` throttles via reclaim before
    // `Max` hard-fails an allocation. .NET-based broker headroom included.
    const MEMORY_HIGH: u64 = 3 * 1024 * 1024 * 1024;
    const MEMORY_MAX: u64 = 4 * 1024 * 1024 * 1024;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build runtime for scope setup")
        .map_err(ScopeError::Refused)?;
    rt.block_on(async {
        let conn = zbus::Connection::session()
            .await
            .context("connect to the user session bus")
            .map_err(ScopeError::NoUserManager)?;
        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await
        .context("open systemd manager proxy")
        .map_err(ScopeError::Refused)?;

        // StartTransientUnit(name, mode, properties: a(sv), aux: a(sa(sv))) -> o
        let mut props: Vec<(&str, zbus::zvariant::Value)> = vec![
            (
                "Description",
                zbus::zvariant::Value::from("intune-container rootless"),
            ),
            ("PIDs", zbus::zvariant::Value::from(vec![pid as u32])),
            ("Delegate", zbus::zvariant::Value::from(true)),
            ("TasksMax", zbus::zvariant::Value::from(TASKS_MAX)),
        ];
        if hardened {
            props.push(("MemoryHigh", zbus::zvariant::Value::from(MEMORY_HIGH)));
            props.push(("MemoryMax", zbus::zvariant::Value::from(MEMORY_MAX)));
        }
        let aux: Vec<(&str, Vec<(&str, zbus::zvariant::Value)>)> = Vec::new();
        let reply: zbus::Result<zbus::zvariant::OwnedObjectPath> = proxy
            .call("StartTransientUnit", &(scope, "replace", props, aux))
            .await;
        match reply {
            Ok(_job) => Ok(()),
            Err(zbus::Error::MethodError(name, _, _)) if means_no_manager(name.as_str()) => Err(
                ScopeError::NoUserManager(anyhow::anyhow!("{name} on the session bus")),
            ),
            Err(e) => Err(ScopeError::Refused(
                anyhow::Error::from(e).context("StartTransientUnit"),
            )),
        }
    })
}

/// Poll until `pid`'s cgroup reflects the scope (the move is asynchronous).
fn wait_in_scope(pid: i32, scope: &str) {
    for _ in 0..300 {
        if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
            if s.contains(scope) {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The cgroup-v2 path of this process, relative to the cgroup root it sees
/// (`0::/path` in `/proc/self/cgroup`; `0::/` inside a container with a cgroup
/// namespace of its own).
fn own_cgroup_path(proc_self_cgroup: &str) -> Option<PathBuf> {
    proc_self_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| PathBuf::from(p.trim().trim_start_matches('/')))
}

/// Put `pid` into `scope`, a new child of the cgroup this process runs in, with
/// no user manager involved (see [`CGROUP_ENV`]).
///
/// No controller is enabled for it: enabling one would first mean moving every
/// other process out of our cgroup (cgroup v2 allows no process beside an
/// enabled controller), and those processes are the embedding application's,
/// not ours. So TasksMax and MemoryMax do not apply here; the limits of the
/// Docker container or pod around us are the ceiling.
fn own_scope(pid: i32, scope: &str) -> Result<()> {
    let rel = own_cgroup_path(
        &std::fs::read_to_string("/proc/self/cgroup").context("read /proc/self/cgroup")?,
    )
    .context("this process is not on the cgroup v2 hierarchy")?;
    let base = Path::new("/sys/fs/cgroup").join(&rel);
    match place_in_new_cgroup(&base, pid, scope) {
        Ok(()) => Ok(()),
        // A container runtime mounts /sys/fs/cgroup read-only. That is a property
        // of the MOUNT, not of the files: a fresh cgroup2 mount in a namespace of
        // our own is writable wherever the files are ours.
        Err(e) if e.raw_os_error() == Some(nix::libc::EROFS) => {
            place_through_private_mount(pid, scope)
        }
        Err(e) => Err(anyhow::Error::from(e)).with_context(|| own_cgroup_refused(&base)),
    }
}

/// What a refusal to write our own cgroup means, and who can fix it.
fn own_cgroup_refused(base: &Path) -> String {
    format!(
        "cannot create a cgroup for the container under {} as uid {}: the cgroup this \
         process runs in must be delegated to it (chown it and its cgroup.procs, \
         cgroup.threads and cgroup.subtree_control to this uid, as root, before \
         starting this process)",
        base.display(),
        getuid()
    )
}

/// Create `root/scope` (tolerating one left by an earlier boot) and move `pid`
/// into it.
fn place_in_new_cgroup(root: &Path, pid: i32, scope: &str) -> std::io::Result<()> {
    let dir = root.join(scope);
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    std::fs::write(dir.join("cgroup.procs"), pid.to_string())
}

/// [`place_in_new_cgroup`] through a cgroup2 mount of our own: in a forked child,
/// unshare a user, mount and cgroup namespace (whose root is our cgroup), mount
/// cgroup2 over `/sys/fs/cgroup` there, and do the move. The mount dies with the
/// child.
///
/// The read-only mount is first covered with a tmpfs, exactly as the
/// container's own init does: cgroup2 is ONE superblock, and the kernel refuses
/// to mount a filesystem on the root of a mount of that same filesystem
/// (`EBUSY`).
fn place_through_private_mount(pid: i32, scope: &str) -> Result<()> {
    let (uid, gid) = (getuid().as_raw(), getgid().as_raw());
    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => match waitpid(child, None).context("waitpid")? {
            WaitStatus::Exited(_, 0) => Ok(()),
            WaitStatus::Exited(_, 125) => anyhow::bail!(
                "could not delegate the container's cgroup through a namespace of our own: \
                 {NAMESPACES_REFUSED}"
            ),
            WaitStatus::Exited(_, 124) => anyhow::bail!(
                "could not mount a cgroup2 of our own to delegate the container's cgroup \
                 (the reason is on stderr)"
            ),
            other => anyhow::bail!(
                "{} ({other:?})",
                own_cgroup_refused(Path::new("/sys/fs/cgroup"))
            ),
        },
        ForkResult::Child => {
            let namespaces = (|| -> Result<()> {
                unshare(CloneFlags::CLONE_NEWUSER).context("unshare a user namespace")?;
                std::fs::write("/proc/self/setgroups", "deny").ok();
                std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))
                    .context("write uid_map")?;
                std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))
                    .context("write gid_map")?;
                unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWCGROUP)
                    .context("unshare a mount and cgroup namespace")?;
                Ok(())
            })();
            let mounted = namespaces.as_ref().map_err(|_| ()).and_then(|()| {
                let nosuid = MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV;
                (|| -> Result<()> {
                    mount(
                        None::<&str>,
                        "/",
                        None::<&str>,
                        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                        None::<&str>,
                    )
                    .context("make / private")?;
                    mount(
                        Some("tmpfs"),
                        "/sys/fs/cgroup",
                        Some("tmpfs"),
                        nosuid,
                        Some("mode=0755"),
                    )
                    .context("cover /sys/fs/cgroup with a tmpfs")?;
                    mount(
                        Some("cgroup2"),
                        "/sys/fs/cgroup",
                        Some("cgroup2"),
                        nosuid,
                        None::<&str>,
                    )
                    .context("mount cgroup2")?;
                    Ok(())
                })()
                .map_err(|e| eprintln!("cgroup delegation: {e:#}"))
            });
            let code = match (&namespaces, mounted) {
                (Err(e), _) => {
                    eprintln!("cgroup delegation: {e:#}");
                    125
                }
                (Ok(()), Err(())) => 124,
                (Ok(()), Ok(())) => {
                    match place_in_new_cgroup(Path::new("/sys/fs/cgroup"), pid, scope) {
                        Ok(()) => 0,
                        Err(e) => {
                            eprintln!("cgroup delegation: {e}");
                            1
                        }
                    }
                }
            };
            unsafe { nix::libc::_exit(code) }
        }
    }
}

// ===== Cross-process runtime state =====

/// Handles to a running rootless container, persisted so a separate process can
/// rediscover and re-enter it. Stored as JSON at
/// `~/.local/share/intune-container/rootless.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuntimeState {
    /// Host pid of the container's PID 1 (systemd).
    pub leader: i32,
    /// The cgroup scope holding the container (a transient unit of the user
    /// manager, or a child of our own cgroup — see [`delegate_cgroup`]).
    pub scope: String,
    /// The leader's start time (see [`process_start_time`]), so a recycled pid
    /// is never mistaken for it. Absent in state written by an older version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<u64>,
}

/// Per-user data directory (`$XDG_DATA_HOME` or `~/.local/share`).
fn data_dir() -> Result<PathBuf> {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("cannot determine data directory: neither $XDG_DATA_HOME nor $HOME is set")
        .map(|d| d.join("intune-container"))
}

fn runtime_state_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("rootless.json"))
}

/// Persist the running container's handles so later invocations can re-enter it.
pub fn save_runtime_state(state: &RuntimeState) -> Result<()> {
    let path = runtime_state_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create data dir")?;
    }
    let json = serde_json::to_string_pretty(state).context("serialize runtime state")?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Load the last-saved container handles, if any. Returns `Ok(None)` when no
/// state file exists (no container has been started).
pub fn load_runtime_state() -> Result<Option<RuntimeState>> {
    let path = runtime_state_path()?;
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(
            serde_json::from_str(&s).context("parse runtime state")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e)).with_context(|| format!("read {}", path.display())),
    }
}

/// Remove any persisted runtime state (after the container has stopped).
pub fn clear_runtime_state() -> Result<()> {
    let path = runtime_state_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::from(e)).with_context(|| format!("remove {}", path.display())),
    }
}

/// Load the saved state and, if the container is still alive, return its leader
/// pid. Stale state (process gone) is cleared and reported as `None`.
pub fn running_leader() -> Result<Option<i32>> {
    match load_runtime_state()? {
        Some(state) if state_is_live(&state) => Ok(Some(state.leader)),
        Some(_) => {
            let _ = clear_runtime_state();
            Ok(None)
        }
        None => Ok(None),
    }
}

/// Whether `state` still describes a running container: its leader is a
/// container init ([`is_running`]) and, where the state recorded one, it is the
/// very process that was started.
///
/// Public so a caller can judge a [`load_runtime_state`] answer without
/// [`running_leader`]'s side effect of clearing stale state.
pub fn state_is_live(state: &RuntimeState) -> bool {
    is_running(state.leader)
        && match state.start_time {
            Some(recorded) => process_start_time(state.leader) == Some(recorded),
            None => true,
        }
}

// ===== Host preflight =====

/// Check that this host can run the rootless backend. Returns a descriptive
/// error explaining the first missing prerequisite, so callers can fall back to
/// the nspawn backend with a clear reason.
pub fn preflight() -> Result<()> {
    // Unprivileged user namespaces must be permitted.
    if let Ok(v) = std::fs::read_to_string("/proc/sys/user/max_user_namespaces") {
        if v.trim().parse::<u64>().unwrap_or(0) == 0 {
            anyhow::bail!(
                "unprivileged user namespaces are disabled \
                 (/proc/sys/user/max_user_namespaces is 0)"
            );
        }
    }
    // Debian/Ubuntu gate (absent on most other distros — only fail if present).
    if let Ok(v) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
        if v.trim() == "0" {
            anyhow::bail!(
                "unprivileged user namespaces are disabled \
                 (kernel.unprivileged_userns_clone is 0)"
            );
        }
    }
    // AppArmor (Ubuntu 24.04+) can restrict unprivileged userns to profiled apps.
    if let Ok(v) = std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
    {
        if v.trim() == "1" {
            anyhow::bail!(
                "AppArmor restricts unprivileged user namespaces \
                 (kernel.apparmor_restrict_unprivileged_userns is 1)"
            );
        }
    }
    // A subuid/subgid range large enough for a distribution's ids must be
    // allocated, and the helpers that apply it must be installed.
    let uid = getuid().as_raw();
    let user = login_name(uid);
    subordinate_start(SUBUID, &user, uid)?;
    subordinate_start(SUBGID, &user, getgid().as_raw())?;
    for helper in ["newuidmap", "newgidmap"] {
        if !on_path(helper) {
            anyhow::bail!("{helper} is not installed (it comes with the uidmap / shadow package)");
        }
    }
    // cgroup v2 must be mounted (unified hierarchy) for delegation.
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    if !mounts.lines().any(|l| {
        let mut f = l.split_whitespace();
        f.next();
        let mp = f.next().unwrap_or("");
        let fstype = f.next().unwrap_or("");
        fstype == "cgroup2" && mp == "/sys/fs/cgroup"
    }) {
        anyhow::bail!("cgroup v2 (unified hierarchy) is not mounted at /sys/fs/cgroup");
    }
    Ok(())
}

/// Whether an executable named `name` is on `$PATH`.
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                std::fs::metadata(dir.join(name))
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{boot_systemd, preflight, run_in_rootfs, start_systemd};
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Extract the published image once into a cached, disk-backed rootfs.
    fn ensure_rootfs() -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("rootless-rootfs");
        if !dir.join("bin/sh").exists() && !dir.join("sbin/init").exists() {
            crate::oci::pull_rootfs("ghcr.io/magicabdel/intune-container:latest", &dir).unwrap();
        }
        dir
    }

    /// Drop a oneshot unit that records the broker's state into the bind-mounted
    /// /run/intune-probe and then powers the container off.
    fn install_probe(rootfs: &Path) {
        let unit = "[Unit]\n\
             Description=intune rootless boot probe\n\
             After=multi-user.target\n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart=/bin/sh -c 'sleep 8; systemctl is-system-running >/run/intune-probe/system 2>&1 || true; systemctl is-active microsoft-identity-device-broker.service >/run/intune-probe/broker 2>&1 || true; systemctl poweroff'\n\
             [Install]\n\
             WantedBy=multi-user.target\n";
        let sysdir = rootfs.join("etc/systemd/system");
        fs::create_dir_all(&sysdir).unwrap();
        fs::write(sysdir.join("intune-probe.service"), unit).unwrap();
        let wants = sysdir.join("multi-user.target.wants");
        fs::create_dir_all(&wants).unwrap();
        let link = wants.join("intune-probe.service");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink("/etc/systemd/system/intune-probe.service", &link).unwrap();
    }

    /// Boot a non-systemd command in our own rootless container.
    ///   cargo test --lib --features rootless run_command_in_rootfs -- --ignored --nocapture
    #[test]
    #[ignore = "needs the extracted rootfs; run manually"]
    fn run_command_in_rootfs() {
        let rootfs = ensure_rootfs();
        let code = run_in_rootfs(
            &rootfs,
            &[
                "/bin/sh",
                "-c",
                "echo IN-CONTAINER; id; head -1 /etc/os-release; echo PID=$$; ls /",
            ],
        )
        .unwrap();
        assert_eq!(code, 0, "container command exited non-zero");
    }

    /// Boot systemd as PID 1 in our own rootless runtime and confirm the Intune
    /// identity broker reaches `active`.
    ///   cargo test --lib --features rootless boot_systemd_rootless -- --ignored --nocapture
    #[test]
    #[ignore = "boots full systemd; run manually"]
    fn boot_systemd_rootless() {
        let rootfs = ensure_rootfs();
        install_probe(&rootfs);

        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        let probe = base.join("probe");
        let _ = fs::remove_dir_all(&probe);
        fs::create_dir_all(&probe).unwrap();
        let log = base.join("systemd-boot.log");
        let _ = fs::remove_file(&log);

        let binds = vec![(probe.clone(), PathBuf::from("/run/intune-probe"))];
        let code = boot_systemd(&rootfs, &binds, Some(&log), false).unwrap();

        let system = fs::read_to_string(probe.join("system")).unwrap_or_default();
        let broker = fs::read_to_string(probe.join("broker")).unwrap_or_default();
        eprintln!("exit={code} system={system:?} broker={broker:?}");
        if let Ok(l) = fs::read_to_string(&log) {
            let lines: Vec<&str> = l.lines().collect();
            let start = lines.len().saturating_sub(40);
            eprintln!("--- boot log tail ---\n{}", lines[start..].join("\n"));
        }

        assert!(broker.contains("active"), "broker not active: {broker:?}");
    }

    /// Boot systemd in our rootless runtime, then `setns` into the running
    /// container and confirm the broker is active and we're really inside.
    ///   cargo test --lib --features rootless exec_in_running_container -- --ignored --nocapture
    #[test]
    #[ignore = "boots systemd and execs into it; run manually"]
    fn exec_in_running_container() {
        let rootfs = ensure_rootfs();
        // Remove the poweroff probe so the container stays up for exec.
        let _ = fs::remove_file(
            rootfs.join("etc/systemd/system/multi-user.target.wants/intune-probe.service"),
        );

        let log = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("exec-boot.log");
        let _ = fs::remove_file(&log);

        let c = start_systemd(&rootfs, &[], Some(&log), false).unwrap();

        let mut active = false;
        for _ in 0..40 {
            let code = c
                .exec(
                    &[
                        "/usr/bin/systemctl",
                        "is-active",
                        "microsoft-identity-device-broker.service",
                    ],
                    None,
                )
                .unwrap_or(1);
            if code == 0 {
                active = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }

        // Show we're genuinely inside the container.
        let _ = c.exec(
            &[
                "/usr/bin/sh",
                "-c",
                "echo IN-CONTAINER-EXEC uid=$(id -u) host=$(hostname); systemctl is-system-running",
            ],
            None,
        );

        let _ = c.stop();
        assert!(active, "broker did not become active via setns exec");
    }

    /// Boot systemd in the **hardened** profile (private IPC namespace) and
    /// confirm we can still `setns` into it. Regression test for the IPC-join
    /// EPERM bug: joining the IPC namespace unconditionally broke entering a
    /// compat container, and a private IPC ns must be enterable here.
    ///   cargo test --lib exec_in_hardened_container -- --ignored --nocapture
    #[test]
    #[ignore = "boots systemd and execs into it; run via `just smoke`"]
    fn exec_in_hardened_container() {
        let rootfs = ensure_rootfs();
        let _ = fs::remove_file(
            rootfs.join("etc/systemd/system/multi-user.target.wants/intune-probe.service"),
        );
        let log = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("exec-hardened-boot.log");
        let _ = fs::remove_file(&log);

        // hardened = true → unshares a private IPC namespace.
        let c = start_systemd(&rootfs, &[], Some(&log), true).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));

        // The exact operation the bug broke: `setns` into the container. A failed
        // IPC join surfaced as exec code 127, so this asserts the join works.
        let code = c
            .exec(&["/usr/bin/sh", "-c", "echo HARDENED-EXEC-OK"], None)
            .unwrap_or(127);

        // The IPC namespace must be genuinely private (different from the host's).
        let host_ipc = fs::read_link("/proc/self/ns/ipc").ok();
        let cont_ipc = fs::read_link(format!("/proc/{}/ns/ipc", c.leader_pid())).ok();

        let _ = c.stop();
        assert_eq!(
            code, 0,
            "exec into hardened container failed (setns regression)"
        );
        assert!(
            host_ipc.is_some() && host_ipc != cont_ipc,
            "hardened container must have a private IPC namespace (host={host_ipc:?} container={cont_ipc:?})"
        );
    }

    /// Boot the container with the host's real display attach plan, then exec a
    /// probe to confirm the forwarded socket is visible inside and the display
    /// environment is set on the launched process.
    ///   cargo test --lib --features rootless display_attach_forwards_socket -- --ignored --nocapture
    #[test]
    #[ignore = "boots systemd and forwards the host display; run manually"]
    fn display_attach_forwards_socket() {
        let rootfs = ensure_rootfs();
        let _ = fs::remove_file(
            rootfs.join("etc/systemd/system/multi-user.target.wants/intune-probe.service"),
        );

        let info = crate::display::DisplayInfo::detect();
        let plan = info.attach_plan(1000);
        if plan.binds.is_empty() {
            eprintln!("no display sockets to forward on this host; skipping assertions");
        }
        let probe_path = plan.binds.first().map(|(_, dst)| dst.display().to_string());

        let log = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("display-attach.log");
        let _ = fs::remove_file(&log);

        let c = start_systemd(&rootfs, &plan.binds, Some(&log), false).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));

        if let Some(path) = &probe_path {
            let script = format!(
                "test -e {path} && echo SOCKET_PRESENT; echo WAYLAND_DISPLAY=$WAYLAND_DISPLAY DISPLAY=$DISPLAY XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
            );
            let code = c.exec(&["/usr/bin/sh", "-c", &script], None).unwrap_or(1);
            assert_eq!(code, 0, "probe exec failed");
        }

        // Confirm the display env is delivered to a process launched via setns.
        let code = super::exec_pid_env(
            c.leader_pid(),
            &["/usr/bin/sh", "-c", "test -n \"$XDG_RUNTIME_DIR\""],
            None,
            &plan.env,
        )
        .unwrap_or(1);
        let _ = c.stop();
        assert_eq!(code, 0, "display env not delivered to launched process");
    }

    /// Boot systemd with NO user manager — the cgroup made by this runtime as a
    /// child of its own — then `setns` into it. This is the Docker / Kubernetes
    /// case, and the only way to exercise it is to run inside one: the
    /// container's entrypoint must hand this uid its own cgroup first (see
    /// `CGROUP_ENV`). From a desktop this test is refused at that step.
    ///   INTUNE_CONTAINER_CGROUP=own cargo test --lib exec_without_a_user_manager -- --ignored --nocapture
    #[test]
    #[ignore = "boots systemd in a container without a user manager; run inside one"]
    fn exec_without_a_user_manager() {
        std::env::set_var(super::CGROUP_ENV, "own");
        let rootfs = ensure_rootfs();
        let _ = fs::remove_file(
            rootfs.join("etc/systemd/system/multi-user.target.wants/intune-probe.service"),
        );
        let log = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("exec-own-cgroup-boot.log");
        let _ = fs::remove_file(&log);

        let c = start_systemd(&rootfs, &[], Some(&log), true).unwrap();
        assert!(c.scope.starts_with("intune-"), "scope {:?}", c.scope);
        let cgroup = fs::read_to_string(format!("/proc/{}/cgroup", c.leader_pid())).unwrap();
        eprintln!("leader cgroup: {cgroup:?}");

        let mut running = String::new();
        let mut broker = false;
        for _ in 0..60 {
            if c.exec(
                &[
                    "/usr/bin/systemctl",
                    "is-active",
                    "--quiet",
                    "microsoft-identity-device-broker.service",
                ],
                None,
            )
            .unwrap_or(1)
                == 0
            {
                broker = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        let _ = c.exec(
            &[
                "/usr/bin/sh",
                "-c",
                "systemctl is-system-running > /run/state 2>&1; cat /run/state; systemctl --failed --no-legend",
            ],
            None,
        );
        if let Ok(l) = fs::read_to_string(&log) {
            let lines: Vec<&str> = l.lines().collect();
            running = lines[lines.len().saturating_sub(30)..].join("\n");
        }
        let _ = c.stop();
        assert!(
            broker,
            "broker not active without a user manager; boot log tail:\n{running}"
        );
    }

    #[test]
    fn a_subordinate_range_is_read_for_the_name_or_the_uid() {
        let file = "# comment\nalice:200000:65536\n1000:300000:65536\nbob:400000:1000\n";
        assert_eq!(
            super::subid_range(file, "alice", 1001),
            Some((200000, 65536))
        );
        // newuidmap accepts the numeric spelling too, which is what an image
        // with no $USER and no password-database name has.
        assert_eq!(
            super::subid_range(file, "teams", 1000),
            Some((300000, 65536))
        );
        assert_eq!(super::subid_range(file, "bob", 1002), Some((400000, 1000)));
        assert_eq!(super::subid_range(file, "carol", 1003), None);
        // A malformed line is skipped rather than read as a range.
        assert_eq!(super::subid_range("alice:x:65536\n", "alice", 1), None);
        // An empty name never matches anybody.
        assert_eq!(super::subid_range(":1:65536\n", "", 1), None);
    }

    #[test]
    fn a_container_init_is_pid_one_of_a_namespace_of_its_own() {
        assert!(super::is_namespace_init(
            "Name:\tsystemd\nNSpid:\t6415\t1\n"
        ));
        assert!(super::is_namespace_init("NSpid:\t9\t40\t1\n"));
        // An ordinary process that was handed a recycled pid.
        assert!(!super::is_namespace_init("Name:\tbash\nNSpid:\t6415\n"));
        // PID 40 of a nested namespace is not that namespace's init.
        assert!(!super::is_namespace_init("NSpid:\t6415\t40\n"));
        assert!(!super::is_namespace_init(""));
        // This test's own process is no container init.
        assert!(!super::is_running(std::process::id() as i32));
    }

    #[test]
    fn a_start_time_is_field_twenty_two_counted_from_the_last_parenthesis() {
        let fields: Vec<String> = (3..=30).map(|n| n.to_string()).collect();
        let stat = format!("4242 (we(ird) name) {}", fields.join(" "));
        assert_eq!(super::stat_start_time(&stat), Some(22));
        assert_eq!(super::stat_start_time("garbage"), None);
        assert!(super::process_start_time(std::process::id() as i32).is_some());
    }

    #[test]
    fn a_state_without_a_start_time_still_reads() {
        let old: super::RuntimeState =
            serde_json::from_str(r#"{"leader": 12, "scope": "intune-11.scope"}"#).unwrap();
        assert_eq!(old.start_time, None);
        let new = serde_json::to_string(&super::RuntimeState {
            start_time: Some(7),
            ..old
        })
        .unwrap();
        assert!(new.contains(r#""start_time":7"#), "{new}");
    }

    #[test]
    fn the_own_cgroup_is_read_from_the_unified_line() {
        assert_eq!(super::own_cgroup_path("0::/\n"), Some(PathBuf::from("")));
        assert_eq!(
            super::own_cgroup_path("0::/kubepods.slice/pod1/cri-x.scope\n"),
            Some(PathBuf::from("kubepods.slice/pod1/cri-x.scope"))
        );
        // A v1-only hierarchy offers no unified line.
        assert_eq!(super::own_cgroup_path("1:name=systemd:/user.slice\n"), None);
    }

    #[test]
    fn the_cgroup_source_is_chosen_by_name_and_an_unknown_one_is_refused() {
        use super::{delegation_from, Delegation};
        assert_eq!(delegation_from(None).unwrap(), Delegation::Auto);
        assert_eq!(delegation_from(Some("")).unwrap(), Delegation::Auto);
        assert_eq!(delegation_from(Some("own")).unwrap(), Delegation::Own);
        assert_eq!(
            delegation_from(Some("user-manager")).unwrap(),
            Delegation::UserManager
        );
        assert!(delegation_from(Some("direct")).is_err());
    }

    #[test]
    fn only_an_absent_manager_falls_back_never_a_refusal() {
        assert!(super::means_no_manager(
            "org.freedesktop.DBus.Error.ServiceUnknown"
        ));
        assert!(super::means_no_manager(
            "org.freedesktop.DBus.Error.NameHasNoOwner"
        ));
        assert!(!super::means_no_manager(
            "org.freedesktop.DBus.Error.AccessDenied"
        ));
        assert!(!super::means_no_manager(
            "org.freedesktop.systemd1.UnitExists"
        ));
    }

    /// Report this host's rootless readiness (always runs; never fails the
    /// suite — it just prints the verdict so we can see it on any machine).
    ///   cargo test --lib --features rootless preflight_reports -- --nocapture
    #[test]
    fn preflight_reports() {
        match preflight() {
            Ok(()) => eprintln!("preflight: rootless backend is supported on this host"),
            Err(e) => eprintln!("preflight: rootless unsupported here: {e:#}"),
        }
    }
}
