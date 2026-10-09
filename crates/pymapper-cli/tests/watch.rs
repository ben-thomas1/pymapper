//! End-to-end tests of `pymapper check --watch`: the real binary on a generated project,
//! edited while it runs.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// A throwaway project directory, removed on drop.
struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(name: &str) -> Project {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-watch-e2e-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create project dir");
        let dir = dir.canonicalize().expect("canonicalize");
        Project { dir }
    }

    fn file(&self, rel: &str, text: &str) -> &Project {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("create parents");
        std::fs::write(&p, text).expect("write file");
        self
    }

    fn spawn(&self, args: &[&str]) -> Watch {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pymapper"))
            .arg("check")
            .arg("--watch")
            .args(args)
            .current_dir(&self.dir)
            .env_remove("VIRTUAL_ENV")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pymapper");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        let (tx, lines) = channel();
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(Line::Out(l));
            }
        });
        std::thread::spawn(move || {
            for l in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx2.send(Line::Err(l));
            }
        });
        Watch { child, lines }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

enum Line {
    Out(String),
    Err(String),
}

/// A running `check --watch`, killed on drop.
struct Watch {
    child: Child,
    lines: Receiver<Line>,
}

/// One cycle: the report lines on stdout and the status line on stderr that ends it.
struct Cycle {
    out: Vec<String>,
    status: String,
}

impl Cycle {
    fn has(&self, needle: &str) -> bool {
        self.out.iter().any(|l| l.contains(needle))
    }
}

impl Watch {
    /// Reads until the next report summary on stdout and status line on stderr
    /// (`pymapper: watching ...` or `... file(s) changed ...`); the two pipes are read by
    /// separate threads, so they may arrive in either order.
    fn cycle(&mut self) -> Cycle {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut out = Vec::new();
        let mut errs = Vec::new();
        let mut status = None;
        let mut summary = false;
        loop {
            if summary && let Some(status) = status {
                return Cycle { out, status };
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(Line::Out(l)) => {
                    summary |= l.starts_with("Found ") || l.starts_with("All checks passed");
                    out.push(l);
                }
                Ok(Line::Err(l)) if l.starts_with("pymapper: watching") || l.contains("file(s) changed") => status = Some(l),
                Ok(Line::Err(l)) => errs.push(l),
                Err(RecvTimeoutError::Timeout) => panic!("no cycle within 30 s; stdout {out:?}, stderr {errs:?}"),
                Err(RecvTimeoutError::Disconnected) => panic!("pymapper exited; stdout {out:?}, stderr {errs:?}"),
            }
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn demo() -> Project {
    let p = Project::new("demo");
    p.file("pyproject.toml", "[project]\nname = \"demo\"\nversion = \"0\"\n")
        .file("pkg/__init__.py", "")
        .file("pkg/a.py", "def used():\n    return 1\n\ndef unused():\n    return 2\n")
        .file("pkg/main.py", "from pkg.a import used\n\nif __name__ == \"__main__\":\n    used()\n");
    p
}

/// Gives the watcher time to start after the report (the first event after the start is
/// what matters; FSEvents may also replay events from just before the stream started).
fn settle() {
    std::thread::sleep(Duration::from_millis(150));
}

#[test]
fn watch_rechecks_after_edits_new_files_deletes_and_config_changes() {
    let p = demo();
    let mut w = p.spawn(&["--no-cache"]);
    let c = w.cycle();
    assert!(c.has("pkg.a:unused") && c.has("Found 1 finding"), "{:?}", c.out);
    assert!(c.status.contains("watching"), "{}", c.status);

    settle();
    p.file("pkg/a.py", "def used():\n    return 1\n\ndef unused():\n    return 2\n\ndef unused2():\n    return 3\n");
    let c = w.cycle();
    assert!(c.has("pkg.a:unused2") && c.has("Found 2 findings"), "{:?}", c.out);
    assert!(c.status.contains("1 file(s) changed"), "{}", c.status);

    settle();
    p.file("pkg/extra.py", "x = 1\n");
    let c = w.cycle();
    assert!(c.has("pkg/extra.py:1:1: PM404"), "{:?}", c.out);

    settle();
    std::fs::remove_file(p.dir.join("pkg/extra.py")).expect("rm");
    let c = w.cycle();
    assert!(!c.has("extra"), "{:?}", c.out);
    assert!(c.has("Found 2 findings"), "{:?}", c.out);

    // A config change reloads everything: the newly ignored rule disappears.
    settle();
    p.file("pyproject.toml", "[project]\nname = \"demo\"\nversion = \"0\"\n\n[tool.pymapper.rules]\nignore = [\"PM401\"]\n");
    let c = w.cycle();
    assert!(c.status.contains("watching"), "a reload prints the start line again: {}", c.status);
    assert!(!c.has("PM401"), "{:?}", c.out);
}

#[test]
fn watch_survives_a_broken_config_and_recovers() {
    let p = demo();
    let mut w = p.spawn(&["--no-cache"]);
    w.cycle();
    settle();
    p.file("pyproject.toml", "[tool.pymapper\n");
    // The error is printed and the next change retried.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_error = false;
    while !saw_error {
        match w.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Line::Err(l)) if l.starts_with("pymapper: error:") => saw_error = true,
            Ok(_) => {}
            Err(e) => panic!("no error line: {e:?}"),
        }
    }
    settle();
    p.file("pyproject.toml", "[project]\nname = \"demo\"\nversion = \"0\"\n");
    let c = w.cycle();
    assert!(c.has("pkg.a:unused"), "{:?}", c.out);
}

#[test]
fn watch_exits_when_stdout_closes() {
    let p = demo();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .args(["check", "--watch", "--no-cache"])
        .current_dir(&p.dir)
        .env_remove("VIRTUAL_ENV")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pymapper");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    while !line.starts_with("Found") {
        line.clear();
        assert!(reader.read_line(&mut line).expect("read") > 0, "pymapper exited before its first report");
    }
    // Close our end: the next report hits a closed pipe and pymapper exits.
    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(30);
    for n in 0.. {
        settle();
        if let Some(status) = child.try_wait().expect("wait") {
            assert!(matches!(status.code(), Some(0 | 1)), "{status:?}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("pymapper kept running after stdout closed");
        }
        p.file("pkg/a.py", &format!("def used():\n    return 1\n\ndef f{n}():\n    pass\n"));
    }
}

#[test]
fn watch_conflicts_with_save_baseline() {
    let p = demo();
    let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .args(["check", "--watch", "--save-baseline", "b.json"])
        .current_dir(&p.dir)
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
}
