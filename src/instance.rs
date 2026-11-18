//! `gitsnack diff [path]`: shows a local working tree's uncommitted changes in the running
//! app, starting one when none runs. The app listens on a Unix socket in its data dir; the
//! command sends it the working tree's root, then brings its window forward.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::{git, store};

/// Set on an app the command starts, so it opens this working tree once it's up.
pub const OPEN_DIFF: &str = "GITSNACK_OPEN_DIFF";

fn socket_path() -> PathBuf {
    store::data_dir().join("gitsnack.sock")
}

/// Listens for `gitsnack diff` requests, calling `open` with each working tree's root.
/// Does nothing if another running app already listens.
pub fn listen(open: impl Fn(String) + Send + 'static) {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        return;
    }
    let _ = std::fs::create_dir_all(store::data_dir());
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else {
        return;
    };
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                continue;
            }
            if let Some(dir) = line.trim_end().strip_prefix("diff ") {
                open(dir.to_string());
            }
            let _ = writeln!(stream, "{}", std::process::id());
        }
    });
}

/// `gitsnack diff [path]`. Returns false for any other command, so the app runs.
pub fn cli(args: &[String]) -> bool {
    if args.get(1).map(String::as_str) != Some("diff") {
        return false;
    }
    if let Err(e) = diff(args.get(2).map(Path::new)) {
        eprintln!("gitsnack: {e:#}");
        std::process::exit(1);
    }
    true
}

fn diff(path: Option<&Path>) -> Result<()> {
    let dir = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let root =
        git::toplevel(&dir).map_err(|_| anyhow!("{} isn't in a git repository", dir.display()))?;
    if let Ok(mut stream) = UnixStream::connect(socket_path()) {
        writeln!(stream, "diff {root}")?;
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply)?;
        if let Ok(pid) = reply.trim().parse() {
            activate(pid);
        }
        return Ok(());
    }
    // No app running: start one, in its own process group so closing the terminal leaves it.
    use std::os::unix::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .env(OPEN_DIFF, &root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("start gitsnack")?;
    Ok(())
}

/// Brings the running app forward. macOS only lets the active app (this command's terminal)
/// hand activation to another, so it's done from here rather than by the app itself.
#[cfg(target_os = "macos")]
fn activate(pid: i32) {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    // SAFETY: plain AppKit calls; NSRunningApplication is thread-safe.
    unsafe {
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
            #[allow(deprecated)]
            app.activateWithOptions(
                NSApplicationActivationOptions::NSApplicationActivateIgnoringOtherApps,
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn activate(_pid: i32) {}
