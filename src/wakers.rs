//! Herdr plugins get no timers or daemons, so each wake deadline gets a detached sleeper process
//! that syncs once the deadline has passed.

use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::Local;

use crate::herdr;
use crate::session::Session;
use crate::store::Store;

const WALL_CLOCK_RECHECK_SECS: u64 = 30;

/// Starts a sleeper for `until`; one already waiting for it makes the new one exit at once.
pub fn spawn(until: i64) -> io::Result<()> {
    Command::new(std::env::current_exe()?)
        .args(["wake-at", &until.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

/// Sleeps until `until` has passed, then syncs, unless another sleeper already has the deadline.
pub fn wake_at(until: i64) -> io::Result<()> {
    let path = Store::for_socket(&herdr::socket_path()).wake_lock_path(until)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    match lock.try_lock() {
        Ok(()) => (),
        Err(fs::TryLockError::WouldBlock) => return Ok(()),
        Err(fs::TryLockError::Error(error)) => return Err(error),
    }
    let now = || Local::now().timestamp();
    while now() <= until {
        let left = u64::try_from(until - now() + 1).unwrap_or(1);
        std::thread::sleep(Duration::from_secs(left.min(WALL_CLOCK_RECHECK_SECS)));
    }
    let _ = fs::remove_file(path);
    let result = Session::connect().and_then(|mut session| session.sync());
    drop(lock);
    result
}
