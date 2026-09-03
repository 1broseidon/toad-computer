//! Booting the machine: the display, the session bus, the desktop, then the
//! agent. `toad-computer boot` is the container's entrypoint.
//!
//! As PID 1 the process forks first. The parent stays a reaper: it collects
//! every child the kernel hands it, forwards SIGTERM to the agent, and exits
//! with the agent's status. The child is the agent, which starts Xvfb and
//! dbus-daemon, becomes the window manager, and serves MCP. A browser helper
//! that outlives its parent, or a program `shell launch` started, is
//! re-parented to PID 1 and collected there instead of lingering as a zombie.
//! A machine whose display or bus has died is a dead machine: the agent exits
//! and the container with it, and Toad starts a fresh one.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::display::Display;
use crate::{App, Config, desktop, serve};

const RUNTIME_DIR: &str = "/tmp/toad-computer";
const START_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);
const SHUTDOWN: Duration = Duration::from_secs(2);

pub fn run(config: Config) -> Result<(), String> {
    let (width, height) = parse_screen(&config.screen)?;
    if std::process::id() == 1 {
        become_init();
    }
    machine(config, width, height)
}

fn parse_screen(screen: &str) -> Result<(u16, u16), String> {
    let invalid = || format!("--screen must be WIDTHxHEIGHT, not {screen:?}");
    let (width, height) = screen.split_once('x').ok_or_else(invalid)?;
    let width: u16 = width.parse().map_err(|_| invalid())?;
    let height: u16 = height.parse().map_err(|_| invalid())?;
    if width < 640 || height < 480 {
        return Err(format!("--screen must be at least 640x480, not {screen:?}"));
    }
    Ok((width, height))
}

/// Fork. The parent becomes the reaper and never returns; the child returns
/// to become the agent.
fn become_init() {
    // SAFETY: the process is still single-threaded, so fork is sound, and
    // the signal set is a plain C struct this function alone touches.
    unsafe {
        let mut signals: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        for signal in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT] {
            libc::sigaddset(&mut signals, signal);
        }
        // Blocked before the fork so an agent that dies at once leaves a
        // pending SIGCHLD rather than a dropped one.
        libc::sigprocmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
        let agent = libc::fork();
        if agent < 0 {
            eprintln!("toad-computer: fork failed");
            std::process::exit(1);
        }
        if agent == 0 {
            libc::sigprocmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut());
            return;
        }
        loop {
            let mut signal = 0;
            if libc::sigwait(&signals, &mut signal) != 0 {
                continue;
            }
            if signal == libc::SIGCHLD {
                loop {
                    let mut status = 0;
                    let pid = libc::waitpid(-1, &mut status, libc::WNOHANG);
                    if pid <= 0 {
                        break;
                    }
                    if pid == agent {
                        std::process::exit(exit_code(status));
                    }
                }
            } else {
                libc::kill(agent, libc::SIGTERM);
            }
        }
    }
}

fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

fn machine(config: Config, width: u16, height: u16) -> Result<(), String> {
    std::fs::create_dir_all(RUNTIME_DIR)
        .map_err(|error| format!("create {RUNTIME_DIR}: {error}"))?;
    let bus_path = Path::new(RUNTIME_DIR).join("bus");
    let bus_address = format!("unix:path={}", bus_path.display());
    // SAFETY: no other thread exists yet. Chromium and the accessibility
    // client find the display and the bus through the environment, so both
    // are set before anything is spawned.
    unsafe {
        std::env::set_var("DISPLAY", &config.display);
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &bus_address);
    }

    let mut xvfb = spawn(
        "Xvfb",
        &[
            &config.display,
            "-screen",
            "0",
            &format!("{width}x{height}x24"),
            "-nolisten",
            "tcp",
            "-noreset",
            "-dpi",
            "96",
        ],
    )?;
    wait_for(&x_socket(&config.display)?, "the display", &mut xvfb)?;
    wait_for_x(&config.display, &mut xvfb)?;

    let mut dbus = spawn(
        "dbus-daemon",
        &[
            "--session",
            "--nofork",
            "--nopidfile",
            &format!("--address={bus_address}"),
        ],
    )?;
    wait_for(&bus_path, "the session bus", &mut dbus)?;

    let app = App::new(config.clone()).with_display(Display::open(&config.display)?);
    let (requests, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    let (ready, desktop_ready) = mpsc::channel();
    let display = config.display.clone();
    std::thread::Builder::new()
        .name("desktop".to_owned())
        .spawn(move || {
            if let Err(error) = desktop::run(&display, requests, ready) {
                eprintln!("toad-computer: desktop: {error}");
            }
        })
        .map_err(|error| format!("spawn desktop thread: {error}"))?;
    match desktop_ready.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("toad-computer: no desktop: {error}"),
        Err(_) => eprintln!("toad-computer: the desktop did not come up in time"),
    }

    let xvfb_pid = xvfb.id();
    let dbus_pid = dbus.id();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let result = runtime.block_on(async {
        let dock = {
            let app = app.clone();
            async move {
                while let Some(request) = incoming.recv().await {
                    match request {
                        desktop::Request::OpenBrowser => {
                            if let Err(error) = app.browser.open().await {
                                eprintln!("toad-computer: dock: {error}");
                            }
                        }
                        desktop::Request::BrowserClosed => app.browser.forget().await,
                    }
                }
            }
        };
        tokio::select! {
            result = serve::run(app) => result,
            status = tokio::task::spawn_blocking(move || xvfb.wait()) => {
                Err(format!("Xvfb exited: {}", describe(status)))
            }
            status = tokio::task::spawn_blocking(move || dbus.wait()) => {
                Err(format!("dbus-daemon exited: {}", describe(status)))
            }
            () = dock => Ok(()),
        }
    });
    // Only PIDs captured at spawn are ever signalled.
    for pid in [xvfb_pid, dbus_pid] {
        // SAFETY: kill with a PID this process spawned is a plain syscall.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    runtime.shutdown_timeout(SHUTDOWN);
    result
}

fn describe(
    status: Result<std::io::Result<std::process::ExitStatus>, tokio::task::JoinError>,
) -> String {
    match status {
        Ok(Ok(status)) => status.to_string(),
        Ok(Err(error)) => error.to_string(),
        Err(error) => error.to_string(),
    }
}

fn spawn(program: &str, arguments: &[&str]) -> Result<Child, String> {
    Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))
}

fn x_socket(display: &str) -> Result<PathBuf, String> {
    let number = display
        .strip_prefix(':')
        .and_then(|rest| rest.split('.').next())
        .filter(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| format!("boot needs a local display like :0, not {display:?}"))?;
    Ok(PathBuf::from(format!("/tmp/.X11-unix/X{number}")))
}

fn wait_for(path: &Path, what: &str, child: &mut Child) -> Result<(), String> {
    let deadline = Instant::now() + START_TIMEOUT;
    while !path.exists() {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("{what} exited before it was ready: {status}"));
        }
        if Instant::now() > deadline {
            return Err(format!(
                "{what} did not appear at {} within {}s",
                path.display(),
                START_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

/// The socket exists a moment before the server accepts on it.
fn wait_for_x(display: &str, xvfb: &mut Child) -> Result<(), String> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if x11rb::connect(Some(display)).is_ok() {
            return Ok(());
        }
        if let Some(status) = xvfb.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("the display exited before it was ready: {status}"));
        }
        if Instant::now() > deadline {
            return Err("the display did not accept a connection in time".to_owned());
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_is_width_by_height() {
        assert_eq!(parse_screen("1920x1080").unwrap(), (1920, 1080));
        assert!(parse_screen("1920").is_err());
        assert!(parse_screen("320x200").is_err());
    }

    #[test]
    fn the_display_socket_is_named_by_its_number() {
        assert_eq!(x_socket(":0").unwrap(), PathBuf::from("/tmp/.X11-unix/X0"));
        assert_eq!(
            x_socket(":12.0").unwrap(),
            PathBuf::from("/tmp/.X11-unix/X12")
        );
        assert!(x_socket("localhost:0").is_err());
    }
}
