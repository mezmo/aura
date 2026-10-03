#![cfg(unix)]

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::mem::MaybeUninit;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

struct PtyChild {
    child: Child,
    master: File,
    monitor: File,
    output: Vec<u8>,
    _home: tempfile::TempDir,
}

impl PtyChild {
    fn spawn(args: &[&str]) -> Self {
        let home = tempfile::TempDir::new().unwrap();
        let output_path = home.path().join("agent.toml");
        let (master, slave) = open_pty();
        let monitor = duplicate(slave);
        let stdout = duplicate(slave);
        let stderr = duplicate(slave);
        let stdin = unsafe { File::from_raw_fd(slave) };

        set_nonblocking(master.as_raw_fd());
        let child = Command::new(env!("CARGO_BIN_EXE_aura"))
            .args(args)
            .arg("--output")
            .arg(output_path)
            .env("HOME", home.path())
            .env("TERM", "xterm-256color")
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .current_dir(home.path())
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap();

        Self {
            child,
            master,
            monitor,
            output: Vec::new(),
            _home: home,
        }
    }

    fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            self.read_available();
            if self.output_text().contains(needle) {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "{}",
                self.output_text()
            );
            thread::sleep(Duration::from_millis(10));
        }
        panic!("did not see {needle:?}: {}", self.output_text());
    }

    fn wait_for_raw_mode(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if terminal_flags(self.monitor.as_raw_fd()) & libc::ICANON == 0 {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("prompt never entered raw mode");
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn terminate(&self, signal: libc::c_int) {
        let result = unsafe { libc::kill(self.child.id() as libc::pid_t, signal) };
        assert_eq!(result, 0, "failed to send signal {signal}");
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            self.read_available();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.read_available();
                return status;
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.child.kill().unwrap();
        let _ = self.child.wait();
        panic!("process did not exit: {}", self.output_text());
    }

    fn assert_terminal_restored(&self) {
        let flags = terminal_flags(self.monitor.as_raw_fd());
        assert_ne!(flags & libc::ICANON, 0, "canonical mode was not restored");
        assert_ne!(flags & libc::ISIG, 0, "signal handling was not restored");
        assert_ne!(flags & libc::ECHO, 0, "echo was not restored");
    }

    fn output_text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    fn read_available(&mut self) {
        let mut buffer = [0_u8; 4096];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => return,
                Ok(length) => self.output.extend_from_slice(&buffer[..length]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(error) => panic!("failed to read pseudo-terminal: {error}"),
            }
        }
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn open_pty() -> (File, RawFd) {
    let mut master = -1;
    let mut slave = -1;
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, 0, "openpty failed");
    let master = unsafe { File::from_raw_fd(master) };
    (master, slave)
}

fn duplicate(fd: RawFd) -> File {
    let duplicate = unsafe { libc::dup(fd) };
    assert_ne!(duplicate, -1, "dup failed");
    unsafe { File::from_raw_fd(duplicate) }
}

fn set_nonblocking(fd: RawFd) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert_ne!(flags, -1, "F_GETFL failed");
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_ne!(result, -1, "F_SETFL failed");
}

fn terminal_flags(fd: RawFd) -> libc::tcflag_t {
    let mut attributes = MaybeUninit::<libc::termios>::uninit();
    let result = unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) };
    assert_eq!(result, 0, "tcgetattr failed");
    unsafe { attributes.assume_init() }.c_lflag
}

#[test]
fn ctrl_z_exits_the_masked_api_key_prompt_and_restores_terminal() {
    let mut process = PtyChild::spawn(&["init", "--provider", "openai", "--offline"]);
    process.wait_for("Enter your API key: ");
    process.wait_for_raw_mode();
    process.send(b"\x1a");
    let status = process.wait_for_exit();
    assert!(!status.success(), "interruption returned success");
    assert!(process.output_text().contains("init interrupted"));
    process.assert_terminal_restored();
}

#[test]
fn ctrl_u_clears_the_current_answer_before_provider_selection() {
    let mut process = PtyChild::spawn(&["init", "--offline"]);
    process.wait_for("Provider: ");
    process.wait_for_raw_mode();
    process.send(b"12\x152\r");
    process.wait_for("Enter your API key: ");
    assert!(!process.output_text().contains("Please enter a number"));
    process.send(b"\x03");
    let status = process.wait_for_exit();
    assert!(!status.success(), "interruption returned success");
    process.assert_terminal_restored();
}

#[test]
fn sigterm_exits_an_active_prompt_and_restores_terminal() {
    let mut process = PtyChild::spawn(&["init", "--offline"]);
    process.wait_for("Provider: ");
    process.wait_for_raw_mode();
    process.terminate(libc::SIGTERM);
    let status = process.wait_for_exit();
    assert!(!status.success(), "termination returned success");
    process.assert_terminal_restored();
}

#[test]
fn sigterm_keeps_its_default_action_after_a_prompt_completes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let server = thread::spawn(move || {
        let (_connection, _) = listener.accept().unwrap();
        accepted_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });

    let mut process = PtyChild::spawn(&["init", "--provider", "ollama"]);
    process.wait_for("Ollama base URL");
    process.wait_for_raw_mode();
    process.send(format!("http://{address}\r").as_bytes());
    accepted_rx.recv_timeout(Duration::from_secs(3)).unwrap();

    process.terminate(libc::SIGTERM);
    let status = process.wait_for_exit();
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    process.assert_terminal_restored();

    release_tx.send(()).unwrap();
    server.join().unwrap();
}
