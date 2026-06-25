//! Pure-Rust rootless container runtime.
//!
//! Creates an unprivileged user namespace (no sudo), plus mount/pid/uts
//! namespaces, sets up `/proc`, `/dev`, `/sys` and `/run`, `pivot_root`s into a
//! rootfs, and boots the rootfs's systemd as PID 1 (cgroup-v2 delegation via a
//! transient scope on the user manager). The host network namespace is kept, so
//! no slirp is needed.
//!
//! ## Security profiles
//!
//! Callers pass `hardened` to select a profile (see `SECURITY.md`):
//!
//! - **hardened** (daemon / headless): also unshares the IPC namespace and the
//!   delegated scope caps memory as well as tasks.
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
            let _ = unsafe { nix::libc::read(ready_r, b.as_mut_ptr() as *mut _, 1) };
            close_fd(ready_r);
            let scope = format!("intune-{}.scope", child.as_raw());
            let setup = set_id_maps(child, uid, gid).and_then(|_| {
                create_delegated_scope(child.as_raw(), &scope, hardened)?;
                wait_in_scope(child.as_raw(), &scope);
                Ok(())
            });
            let _ = unsafe { nix::libc::write(go_w, b.as_ptr() as *const _, 1) };
            close_fd(go_w);
            setup?;
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
            let _ = unsafe { nix::libc::read(go_r, r.as_mut_ptr() as *mut _, 1) };
            close_fd(go_r);

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

/// Whether a container with PID 1 host pid `leader` is still alive. Checks that
/// the process exists and still exposes a pid namespace (i.e. it's a container
/// init, not a recycled unrelated pid).
pub fn is_running(leader: i32) -> bool {
    if leader <= 1 {
        return false;
    }
    Path::new(&format!("/proc/{leader}/ns/pid")).exists()
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

/// Map container ids using the setuid `newuidmap`/`newgidmap` helpers: id 0 maps
/// to the caller, ids 1.. map to the subuid/subgid range (base 100000).
fn set_id_maps(child: Pid, uid: u32, gid: u32) -> Result<()> {
    let pid = child.as_raw().to_string();
    let run = |bin: &str, owner: u32| -> Result<()> {
        let status = std::process::Command::new(bin)
            .args([
                pid.as_str(),
                "0",
                &owner.to_string(),
                "1",
                "1",
                "100000",
                "65535",
            ])
            .status()
            .with_context(|| format!("run {bin}"))?;
        if !status.success() {
            anyhow::bail!("{bin} failed (is the subuid/subgid range configured?)");
        }
        Ok(())
    };
    run("newuidmap", uid)?;
    run("newgidmap", gid)?;
    Ok(())
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
            let _ = unsafe { nix::libc::read(ready_r, b.as_mut_ptr() as *mut _, 1) };
            close_fd(ready_r);
            let maps = set_id_maps(child, uid, gid);
            let _ = unsafe { nix::libc::write(go_w, b.as_ptr() as *const _, 1) };
            close_fd(go_w);
            maps?;
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
                let _ = unsafe { nix::libc::read(go_r, r.as_mut_ptr() as *mut _, 1) };
                close_fd(go_r);
                // Now uid 0 in the namespace (mapped to the host user), with the
                // subuid range mapped too, so we can unlink subuid-owned files.
                match std::fs::remove_dir_all(path) {
                    Ok(()) => 0,
                    Err(_) => 1,
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

/// Ask the user's systemd manager (over the session bus) to place `pid` into a
/// transient scope (`scope`) with cgroup **delegation**, so a `cgroup2` mounted
/// inside is writable and systemd can manage its own subtree.
///
/// Both profiles cap the task count (fork-bomb / runaway containment) at a level
/// generous enough for the GUI path (a browser spawns many helpers). The
/// `hardened` profile additionally caps memory: the headless daemon (broker +
/// compliance agent) is light, so a hard ceiling contains a runaway without
/// risking the OOM-kill of a legitimate forwarded browser.
fn create_delegated_scope(pid: i32, scope: &str, hardened: bool) -> Result<()> {
    // Process-count ceiling, both profiles. High enough for a browser's helpers.
    const TASKS_MAX: u64 = 8192;
    // Memory ceilings, hardened profile only. `High` throttles via reclaim before
    // `Max` hard-fails an allocation. .NET-based broker headroom included.
    const MEMORY_HIGH: u64 = 3 * 1024 * 1024 * 1024;
    const MEMORY_MAX: u64 = 4 * 1024 * 1024 * 1024;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build runtime for scope setup")?;
    rt.block_on(async {
        let conn = zbus::Connection::session()
            .await
            .context("connect to the user session bus")?;
        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await
        .context("open systemd manager proxy")?;

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
        let _job: zbus::zvariant::OwnedObjectPath = proxy
            .call("StartTransientUnit", &(scope, "replace", props, aux))
            .await
            .context("StartTransientUnit")?;
        Ok::<(), anyhow::Error>(())
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

// ===== Cross-process runtime state =====

/// Handles to a running rootless container, persisted so a separate process can
/// rediscover and re-enter it. Stored as JSON at
/// `~/.local/share/intune-container/rootless.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuntimeState {
    /// Host pid of the container's PID 1 (systemd).
    pub leader: i32,
    /// The transient systemd scope holding the container's cgroup.
    pub scope: String,
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
        Some(state) if is_running(state.leader) => Ok(Some(state.leader)),
        Some(_) => {
            let _ = clear_runtime_state();
            Ok(None)
        }
        None => Ok(None),
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
    // A subuid/subgid range must be allocated for multi-id mapping.
    let user = std::env::var("USER").unwrap_or_default();
    let uid = getuid().as_raw();
    if !has_subid_range("/etc/subuid", &user, uid) {
        anyhow::bail!("no /etc/subuid range for the current user (run: usermod --add-subuids ...)");
    }
    if !has_subid_range("/etc/subgid", &user, uid) {
        anyhow::bail!("no /etc/subgid range for the current user (run: usermod --add-subgids ...)");
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

/// Whether `file` (subuid/subgid format `name:start:count`) has an entry for the
/// given user name or numeric uid.
fn has_subid_range(file: &str, user: &str, uid: u32) -> bool {
    let uid_s = uid.to_string();
    std::fs::read_to_string(file)
        .map(|s| {
            s.lines().any(|l| {
                let name = l.split(':').next().unwrap_or("");
                !name.is_empty() && (name == user || name == uid_s)
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
