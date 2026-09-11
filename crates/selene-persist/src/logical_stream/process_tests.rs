//! Native process-crash witnesses: OS page cache remains intact, not power loss.
use super::*;
use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

struct Process {
    child: Child,
    output: Option<std::thread::JoinHandle<()>>,
    ready: mpsc::Receiver<()>,
}
impl Process {
    fn spawn(dir: &StoreDirectory, mode: &str, phase: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "logical_stream::tests::process::child",
                "--ignored",
                "--nocapture",
            ])
            .env("SELENE_PR06_DIR", dir.locator())
            .env("SELENE_PR06_MODE", mode)
            .env("SELENE_PR06_PHASE", phase)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, ready) = mpsc::channel();
        let output = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.unwrap() == "PR06_RENDEZVOUS" {
                    let _ = tx.send(());
                }
            }
        });
        Self {
            child,
            output: Some(output),
            ready,
        }
    }
    fn ready(&self) {
        self.ready
            .recv_timeout(Duration::from_secs(10))
            .expect("child reached actual selection/phase");
    }
    fn finish(&mut self, kill: bool) {
        if kill {
            self.child.kill().unwrap();
        } else {
            self.child.stdin.as_mut().unwrap().write_all(b"x").unwrap();
        }
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "child did not finish"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.success(), !kill);
        self.output.take().unwrap().join().unwrap();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(output) = self.output.take() {
            output.join().unwrap();
        }
    }
}

#[allow(clippy::print_stdout)]
fn rendezvous() {
    println!("PR06_RENDEZVOUS");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
}

#[test]
#[ignore = "invoked only by the bounded process lifecycle tests"]
fn child() {
    let dir = StoreDirectory::open(std::path::Path::new(
        &std::env::var_os("SELENE_PR06_DIR").unwrap(),
    ))
    .unwrap();
    let mode = std::env::var("SELENE_PR06_MODE").unwrap();
    if mode == "reader" {
        let mut reader = LogicalReader::open(&dir, &identity(), 1024).unwrap();
        rendezvous(); // selected and pinned, before snapshot or WAL consumption
        assert_eq!(reader.snapshot_body().unwrap(), b"initial");
        assert_eq!(reader.next_body().unwrap().unwrap(), b"one");
        assert!(reader.next_body().unwrap().is_none());
        return;
    }
    let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
    reopen.snapshot_body().unwrap();
    while reopen.next_body().unwrap().is_some() {}
    let mut wal = reopen.finish().unwrap();
    let point = match std::env::var("SELENE_PR06_PHASE").unwrap().as_str() {
        "pre-current" => "current.replace",
        "post-current" => "current.dir_sync",
        "cleanup" => "prune.dir_sync",
        _ => panic!("unknown phase"),
    };
    dir.at_phase(point, rendezvous);
    if mode == "checkpoint" {
        wal.checkpoint(b"two image", 2).unwrap();
    } else {
        wal.prune().unwrap();
    }
    panic!("parent must kill the paused process");
}

#[test]
fn independent_reader_can_finish_or_die_without_blocking_publication_or_leaking_names() {
    for killed in [false, true] {
        let (_temp, dir, mut wal) = fixture();
        let initial = wal.checkpoint(b"initial", 0).unwrap();
        commit(&mut wal, &[b"one"]);
        let old_log = wal.selected.log_name();
        let mut process = Process::spawn(&dir, "reader", "none");
        process.ready();
        wal.checkpoint(b"one image", 1).unwrap();
        commit(&mut wal, &[b"two"]);
        wal.checkpoint(b"two image", 2).unwrap();
        let report = wal.prune().unwrap();
        for name in [&initial.name, &old_log] {
            assert!(report.retained.iter().any(|a| &a.artifact.name == name
                && a.artifact.bytes > 0
                && a.reason == RetentionReason::Reader));
            assert!(dir.contains(name).unwrap());
        }
        process.finish(killed);
        let report = wal.prune().unwrap();
        assert!(report.cleanup_error.is_none());
        assert!(report.removed.iter().any(|a| a.name == initial.name));
        assert!(!dir.contains(&old_log).unwrap());
    }
}

#[test]
fn process_kill_before_after_current_and_mid_cleanup_recovers_whole_selected_state() {
    for phase in ["pre-current", "post-current", "cleanup"] {
        let (_temp, dir, mut wal) = fixture();
        wal.checkpoint(b"initial", 0).unwrap();
        commit(&mut wal, &[b"one"]);
        wal.checkpoint(b"one image", 1).unwrap();
        commit(&mut wal, &[b"two"]);
        if phase == "cleanup" {
            wal.checkpoint(b"two image", 2).unwrap();
        }
        drop(wal);
        let mut process = Process::spawn(
            &dir,
            if phase == "cleanup" {
                "prune"
            } else {
                "checkpoint"
            },
            phase,
        );
        process.ready();
        process.finish(true);
        let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
        let before = phase == "pre-current";
        assert_eq!(
            reopen.snapshot_body().unwrap(),
            if before {
                b"one image".as_slice()
            } else {
                b"two image".as_slice()
            }
        );
        if before {
            assert_eq!(reopen.next_body().unwrap().unwrap(), b"two");
        }
        assert!(reopen.next_body().unwrap().is_none());
        let mut wal = reopen.finish().unwrap();
        assert_eq!(wal.progress().synchronized.sequence, 2);
        wal.prune().unwrap();
        commit(&mut wal, &[b"three"]);
    }
}

#[test]
fn selection_registers_pin_before_releasing_epoch_to_checkpoint_and_prune() {
    let (_temp, dir, mut wal) = fixture();
    let initial = wal.checkpoint(b"initial", 0).unwrap();
    commit(&mut wal, &[b"one"]);
    let (selected_tx, selected_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    dir.at_phase("reader.selected", move || {
        selected_tx.send(()).unwrap();
        resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let reader_dir = dir.clone();
    let reader =
        std::thread::spawn(move || LogicalReader::open(&reader_dir, &identity(), 1024).unwrap());
    selected_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        crate::manifest_lock::set_contention_hook(move || blocked_tx.send(()).unwrap());
        wal.checkpoint(b"one image", 1).unwrap();
        wal.checkpoint(b"one image again", 1).unwrap();
        let report = wal.prune().unwrap();
        (wal, report)
    });
    blocked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    resume_tx.send(()).unwrap();
    let mut reader = reader.join().unwrap();
    let (mut wal, report) = writer.join().unwrap();
    assert!(
        report
            .retained
            .iter()
            .any(|a| a.artifact.name == initial.name && a.reason == RetentionReason::Reader)
    );
    assert_eq!(reader.snapshot_body().unwrap(), b"initial");
    assert_eq!(reader.next_body().unwrap().unwrap(), b"one");
    drop(reader);
    assert!(
        wal.prune()
            .unwrap()
            .removed
            .iter()
            .any(|a| a.name == initial.name)
    );
}
