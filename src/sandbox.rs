//! Runs commands inside the container as an unprivileged user.

use anyhow::{Context, Result};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::MetadataExt, process::CommandExt},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub struct User {
    uid: u32,
    home: &'static str,
}

/// Runs whatever the agent asks for. Owns only /scratch.
pub const AGENT: User = User {
    uid: 1001,
    home: "/scratch",
};
/// Runs trusted verification. Owns /verify, which the agent cannot read.
pub const VERIFIER: User = User {
    uid: 1002,
    home: "/verify",
};

const MAX_OUTPUT: usize = 4 << 20;

pub struct Output {
    /// None if the command timed out or died from a signal.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs a bash command as `user`. No process of that user outlives the call.
pub fn run(user: User, command: &str, stdin: &str, timeout: Duration) -> Result<Output> {
    kill_all(user);
    let mut builder = Command::new("/bin/bash");
    builder
        // Root cannot enter the user's directory itself, so bash changes into it.
        .args(["-c", "cd ~ && eval \"$0\"", command])
        .current_dir("/")
        .env_clear()
        .env(
            "PATH",
            "/opt/cargo/bin:/opt/verus:/usr/local/bin:/usr/bin:/bin",
        )
        .env("RUSTUP_HOME", "/opt/rustup")
        .env("CARGO_HOME", "/opt/cargo")
        .env("HOME", user.home)
        .env("TMPDIR", user.home)
        .env("LANG", "C.UTF-8")
        // Setting the uid from root also drops supplementary groups.
        .uid(user.uid)
        .gid(user.uid)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe calls between fork and exec. Runs after the uid change.
    unsafe {
        builder.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 200,
                rlim_max: 200,
            };
            if libc::setrlimit(libc::RLIMIT_NPROC, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = builder.spawn().context("spawning command")?;

    let mut input = child.stdin.take().expect("stdin is piped");
    let stdin = stdin.to_owned();
    thread::spawn(move || input.write_all(stdin.as_bytes()));
    let stdout = read_capped(child.stdout.take().expect("stdout is piped"));
    let stderr = read_capped(child.stderr.take().expect("stderr is piped"));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_all(user);
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    // Background processes would otherwise keep the output pipes open.
    kill_all(user);
    Ok(Output {
        code: if timed_out { None } else { status.code() },
        timed_out,
        stdout: stdout.join().expect("reader thread"),
        stderr: stderr.join().expect("reader thread"),
    })
}

/// Writes a file as `user`, so that root never follows a path the user controls.
pub fn write_file(user: User, path: &str, contents: &str) -> Result<()> {
    let out = run(
        user,
        &format!("cat > '{path}'"),
        contents,
        Duration::from_secs(10),
    )?;
    anyhow::ensure!(out.success(), "writing {path}: {}", out.stderr);
    Ok(())
}

fn read_capped(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let (mut kept, mut buf) = (Vec::new(), [0; 8192]);
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            // Keep draining past the cap so the command is not blocked.
            let room = MAX_OUTPUT - kept.len();
            kept.extend_from_slice(&buf[..n.min(room)]);
        }
        String::from_utf8_lossy(&kept).into_owned()
    })
}

/// Signals every process of the user until none are left. Needs no fork, so it works even if
/// the user has exhausted the process limit.
fn kill_all(user: User) {
    for _ in 0..200 {
        let pids: Vec<i32> = fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.metadata().is_ok_and(|m| m.uid() == user.uid))
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .filter(|pid| {
                // Zombies cannot be killed and are harmless.
                fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|s| !s.rsplit(')').next().unwrap_or("").starts_with(" Z"))
            })
            .collect();
        if pids.is_empty() {
            return;
        }
        for pid in pids {
            // SAFETY: kill has no memory-safety requirements.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        thread::sleep(Duration::from_millis(5));
    }
}
