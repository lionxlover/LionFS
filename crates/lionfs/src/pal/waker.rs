//! Cross-thread wakeup primitive for the portable I/O engine backend.
//!
//! The io_uring backend wakes on CQEs; the portable (threaded) backend's
//! reaper thread parks until work arrives or a timeout elapses, and the
//! reaper must be wakeable from the submission path with minimal cost.
//! Per platform:
//!
//! * **Linux**: `eventfd` -- one fd, one 8-byte write to wake, zero
//!   allocation, readable in epoll (and io_uring viaIORING_OP_READ).
//! * **macOS/BSD**: a self-pipe (kqueue-able), the classic technique.
//! * **Windows**: `SleepConditionVariableSRW` over an SRWLOCK + a
//!   generation counter -- no handle, no kernel object per wake.
//!
//! The API is intentionally tiny: [`Waker::new`], [`Waker::wake`], and
//! [`Waker::wait`]. `wait` returns `true` if woken (event pending) or
//! `false` on timeout, either way consuming the wake state so a stray
//! extra `wake` costs one extra cheap wait-return, never a lost wakeup:
//! the primitive is level-triggered by construction.

use std::io::Result;
use std::time::Duration;

pub struct Waker {
    inner: Inner,
}

impl Waker {
    /// Creates a new, un-woken waker.
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: Inner::new()?,
        })
    }

    /// Wakes the waiter (cheap, async-signal-safe on unix, idempotent).
    pub fn wake(&self) {
        self.inner.wake();
    }

    /// Blocks up to `timeout` for a wake. Returns `true` if woken before
    /// the deadline, `false` on timeout. A `wake` that happened before
    /// this call is honored (level-triggered).
    pub fn wait(&self, timeout: Duration) -> bool {
        self.inner.wait(timeout)
    }
}

impl Default for Waker {
    fn default() -> Self {
        Self::new().expect("waker allocation should not fail on a healthy host")
    }
}

// -- Linux: eventfd -----------------------------------------------------------

#[cfg(target_os = "linux")]
struct Inner {
    fd: std::sync::Arc<LinuxEventFd>,
}

#[cfg(target_os = "linux")]
impl Inner {
    fn new() -> Result<Self> {
        Ok(Self {
            fd: std::sync::Arc::new(LinuxEventFd::new()?),
        })
    }

    fn wake(&self) {
        // SAFETY: fd is a valid, owned eventfd; write is one u64.
        let one: u64 = 1;
        let _ = unsafe { libc::write(self.fd.raw(), &one as *const u64 as *const libc::c_void, 8) };
    }

    fn wait(&self, timeout: Duration) -> bool {
        // poll(2) on the eventfd with the timeout.
        let mut pfd = libc::pollfd {
            fd: self.fd.raw(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: pfd is a local, fully-initialized pollfd array of len 1.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
            // Drain the counter so the next wait actually blocks.
            let mut v: u64 = 0;
            // SAFETY: valid fd; reads exactly 8 bytes into a local.
            unsafe { libc::read(self.fd.raw(), &mut v as *mut u64 as *mut libc::c_void, 8) };
            true
        } else {
            false
        }
    }
}

#[cfg(target_os = "linux")]
struct LinuxEventFd {
    fd: i32,
}

#[cfg(target_os = "linux")]
impl LinuxEventFd {
    fn new() -> Result<Self> {
        // EFD_CLOEXEC = 0o2000000, semaphore off (counter mode).
        // SAFETY: plain fd creation; failure returns -1 with errno.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { fd })
    }
    fn raw(&self) -> i32 {
        self.fd
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxEventFd {
    fn drop(&mut self) {
        // SAFETY: closing our own valid fd exactly once.
        unsafe { libc::close(self.fd) };
    }
}

// SAFETY: the eventfd is a raw kernel object; poll/read/write are
// thread-safe on it.
#[cfg(target_os = "linux")]
unsafe impl Send for LinuxEventFd {}
#[cfg(target_os = "linux")]
unsafe impl Sync for LinuxEventFd {}

// -- Other unix: self-pipe -----------------------------------------------------

#[cfg(all(unix, not(target_os = "linux")))]
struct Inner {
    pipes: std::sync::Arc<Pipes>,
}

#[cfg(all(unix, not(target_os = "linux")))]
struct Pipes {
    read_fd: i32,
    write_fd: i32,
}

#[cfg(all(unix, not(target_os = "linux")))]
impl Inner {
    fn new() -> Result<Self> {
        let mut fds = [0i32; 2];
        // SAFETY: pipe() writes two fds into a local [i32; 2].
        let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Set both fds CLOEXEC so a fork+exec child does not inherit the
        // engine's wake pipe.
        // SAFETY: F_SETFD with FD_CLOEXEC on valid fds.
        unsafe {
            libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok(Self {
            pipes: std::sync::Arc::new(Pipes {
                read_fd: fds[0],
                write_fd: fds[1],
            }),
        })
    }

    fn wake(&self) {
        // SAFETY: valid owned write end; one byte written.
        let b = [b'w'];
        let _ = unsafe { libc::write(self.pipes.write_fd, b.as_ptr().cast(), 1) };
    }

    fn wait(&self, timeout: Duration) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.pipes.read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: local pollfd, len 1.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let mut buf = [0u8; 64];
            // SAFETY: valid read end; bounded buffer.
            unsafe { libc::read(self.pipes.read_fd, buf.as_mut_ptr().cast(), buf.len()) };
            true
        } else {
            false
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
impl Drop for Pipes {
    fn drop(&mut self) {
        // SAFETY: closing each of our own fds exactly once.
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
unsafe impl Send for Pipes {}
#[cfg(all(unix, not(target_os = "linux")))]
unsafe impl Sync for Pipes {}

// -- Windows: condvar + generation ---------------------------------------------

#[cfg(windows)]
struct Inner {
    state: std::sync::Mutex<u64>,
    cond: std::sync::Condvar,
}

#[cfg(windows)]
impl Inner {
    fn new() -> Result<Self> {
        Ok(Self {
            state: std::sync::Mutex::new(0),
            cond: std::sync::Condvar::new(),
        })
    }

    fn wake(&self) {
        let mut g = self.state.lock().unwrap();
        *g = g.wrapping_add(1);
        self.cond.notify_all();
    }

    fn wait(&self, timeout: Duration) -> bool {
        let mut g = self.state.lock().unwrap();
        let start = g.wrapping_add(0);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if *g != start {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _res) = self.cond.wait_timeout(g, deadline - now).unwrap();
            g = guard;
        }
    }
}

// -- tests ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn wait_without_wake_times_out() {
        let w = Waker::new().unwrap();
        let start = Instant::now();
        assert!(!w.wait(Duration::from_millis(30)));
        assert!(start.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn wake_before_wait_is_honored() {
        let w = Waker::new().unwrap();
        w.wake();
        assert!(w.wait(Duration::from_millis(0)));
        // Level-triggered state was consumed by the successful wait.
        assert!(!w.wait(Duration::from_millis(0)));
    }

    #[test]
    fn wake_from_another_thread() {
        let w = std::sync::Arc::new(Waker::new().unwrap());
        let w2 = std::sync::Arc::clone(&w);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            w2.wake();
        });
        let start = Instant::now();
        assert!(w.wait(Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(1));
        t.join().unwrap();
    }

    #[test]
    fn default_waker_works() {
        let w = Waker::default();
        w.wake();
        assert!(w.wait(Duration::from_millis(0)));
    }
}
