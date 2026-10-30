//! Single instance: the primary listens on a Unix socket in `$XDG_RUNTIME_DIR`. A second launch
//! connects, sends `show` (or `quit`) and exits. A socket file left by a crashed instance is
//! detected (nobody accepts) and replaced.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;

use crate::paths;

type Callback = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

pub struct Primary {
    path: PathBuf,
    show: Callback,
    quit: Callback,
}

/// `Some` if this process is now the primary instance, `None` if another one is running.
pub fn acquire() -> anyhow::Result<Option<Primary>> {
    let path = paths::instance_socket();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if UnixStream::connect(&path).is_ok() {
        return Ok(None);
    }
    // Nobody accepts: a stale socket file of a crashed instance (or none at all).
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        // another launch won the race between our connect and bind
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("binding {}", path.display())),
    };
    let primary = Primary { path, show: Arc::default(), quit: Arc::default() };
    let (show, quit) = (primary.show.clone(), primary.quit.clone());
    std::thread::Builder::new()
        .name("instance-listener".into())
        .spawn(move || serve(&listener, &show, &quit))
        .context("instance listener thread")?;
    Ok(Some(primary))
}

fn serve(listener: &UnixListener, show: &Callback, quit: &Callback) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
        let mut line = String::new();
        if BufReader::new(conn).read_line(&mut line).is_err() {
            continue;
        }
        let callback = match line.trim() {
            "show" => show,
            "quit" => quit,
            // `another_running()` probes by connecting without a request
            "" => continue,
            other => {
                tracing::warn!("instance socket: unknown request {other:?}");
                continue;
            }
        };
        if let Some(f) = callback.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            f();
        }
    }
}

pub fn another_running() -> bool {
    UnixStream::connect(paths::instance_socket()).is_ok()
}

fn signal(request: &str) {
    match UnixStream::connect(paths::instance_socket()) {
        Ok(mut s) => {
            let _ = s.write_all(format!("{request}\n").as_bytes());
        }
        Err(e) => tracing::warn!("cannot reach the running instance: {e}"),
    }
}

/// Asks the running instance to show its window.
pub fn signal_existing() {
    signal("show");
}

impl Primary {
    /// `f` runs on the listener thread whenever another launch asks for the window.
    pub fn listen_for_show(&self, f: impl Fn() + Send + 'static) {
        *self.show.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
    }

    /// `f` runs on the listener thread when another process asks this one to exit.
    pub fn listen_for_quit(&self, f: impl Fn() + Send + 'static) {
        *self.quit.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
    }
}

impl Drop for Primary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Asks the running instance to exit (`--quit`). Returns whether one was reached.
pub fn signal_quit() -> bool {
    UnixStream::connect(paths::instance_socket())
        .and_then(|mut s| s.write_all(b"quit\n"))
        .is_ok()
}
