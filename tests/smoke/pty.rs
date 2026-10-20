use std::{
    fs::File,
    io::{Read, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use super::{BINARY, http::Server};

pub struct Terminal {
    child: Child,
    master: File,
    _slave: File,
    before: libc::termios,
    width: u16,
}

fn attributes(file: &File) -> libc::termios {
    let mut attributes = MaybeUninit::uninit();
    // SAFETY: tcgetattr initializes the termios value on success.
    assert_eq!(
        unsafe { libc::tcgetattr(file.as_raw_fd(), attributes.as_mut_ptr()) },
        0
    );
    unsafe { attributes.assume_init() }
}

impl Terminal {
    pub fn start(
        server: &Server,
        args: &[&str],
        username: Option<&str>,
        key: Option<&std::path::Path>,
    ) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty writes two owned descriptors and reads the supplied window size.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let before = attributes(&slave);
        // SAFETY: master is an open descriptor, and fcntl takes no pointer arguments here.
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let mut command = Command::new(BINARY);
        command
            .args(["--base-url", &server.base])
            .args(args)
            .env_remove("DZ_USERNAME")
            .env_remove("DZ_SSH_KEY")
            .env_remove("DZ_BASE_URL")
            .env("DZ_CONFIG_DIR", server.directory.path().join("no-session"))
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        if let Some(username) = username {
            command.env("DZ_USERNAME", username);
        }
        if let Some(key) = key {
            command.env("DZ_SSH_KEY", key);
        }
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        // A controlling terminal is required for OpenSSH passphrase prompts.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self {
            child,
            master,
            _slave: slave,
            before,
            width: 100,
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    fn read(&mut self, output: &mut Vec<u8>) {
        let mut buffer = [0; 65536];
        match self.master.read(&mut buffer) {
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::Interrupted
                    || error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => panic!("PTY read: {error}"),
        }
    }

    pub fn wait_for(&mut self, expected: &str) {
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut resize_at = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            if Instant::now() >= resize_at {
                self.width = if self.width == 100 { 101 } else { 100 };
                let size = libc::winsize {
                    ws_row: 30,
                    ws_col: self.width,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                // Full repaints expose text omitted by differential terminal updates.
                unsafe {
                    assert_eq!(
                        libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size),
                        0
                    );
                    libc::kill(self.child.id() as libc::pid_t, libc::SIGWINCH);
                }
                resize_at = Instant::now() + Duration::from_millis(250);
            }
            self.read(&mut output);
            if output
                .windows(expected.len())
                .any(|window| window == expected.as_bytes())
            {
                // Drain the repaint before sending another key.
                let until = Instant::now() + Duration::from_millis(200);
                while Instant::now() < until {
                    self.read(&mut Vec::new());
                    thread::sleep(Duration::from_millis(10));
                }
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "did not see {expected:?}; output: {}",
            String::from_utf8_lossy(&output[output.len().saturating_sub(2000)..])
        );
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "TUI did not exit in time");
            self.read(&mut Vec::new());
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn finish(mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.send(b"\x03yq");
        }
        assert!(self.wait_exit(Duration::from_secs(3)).success());
        let after = attributes(&self.master);
        assert_eq!(
            after.c_iflag, self.before.c_iflag,
            "input mode was not restored"
        );
        assert_eq!(
            after.c_oflag, self.before.c_oflag,
            "output mode was not restored"
        );
        assert_eq!(
            after.c_cflag, self.before.c_cflag,
            "control mode was not restored"
        );
        assert_eq!(
            after.c_lflag, self.before.c_lflag,
            "local mode was not restored"
        );
        assert_eq!(
            after.c_cc, self.before.c_cc,
            "terminal characters were not restored"
        );
        unsafe {
            assert_eq!(libc::cfgetispeed(&after), libc::cfgetispeed(&self.before));
            assert_eq!(libc::cfgetospeed(&after), libc::cfgetospeed(&self.before));
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: setsid made the child's PID its process-group ID.
            let _ = unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
        }
        let _ = self.child.wait();
    }
}
