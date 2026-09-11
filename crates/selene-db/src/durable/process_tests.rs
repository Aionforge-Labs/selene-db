//! Genuine SIGKILL consumer witnesses. OS page cache stays intact, not power loss.
use super::*;
use crate::{CreatePolicy, ExecutionOutcome, ObjectPath, SchemaPath, TransactionAccessMode, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
};

struct Process {
    child: Child,
    output: Option<std::thread::JoinHandle<()>>,
    ready: mpsc::Receiver<()>,
}
impl Process {
    fn spawn(path: &Path, phase: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "durable::process_tests::child",
                "--ignored",
                "--nocapture",
            ])
            .env("SELENE_PR07_DIR", path)
            .env("SELENE_PR07_PHASE", phase)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, ready) = mpsc::channel();
        let output = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.unwrap() == "PR07_RENDEZVOUS" {
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
    fn kill(&mut self) {
        self.ready
            .recv_timeout(Duration::from_secs(10))
            .expect("real phase reached");
        self.child.kill().unwrap();
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "child not reaped"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
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
fn pause() {
    println!("PR07_RENDEZVOUS");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
}
fn path() -> ObjectPath {
    ObjectPath::regular("selene", "memory", "data").unwrap()
}
fn transaction(db: &Database, id: i64) {
    let session = db.session(&path()).unwrap();
    session
        .start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    for part in 0..2 {
        session
            .execute(&format!(
                "INSERT (:Txn {{id: {id}, part: {part}, value: {}}})",
                id * 100 + part
            ))
            .unwrap();
    }
    session.commit_transaction().unwrap();
}
fn expected(ids: &[i64]) -> Vec<Vec<Value>> {
    ids.iter()
        .flat_map(|id| {
            (0..2).map(move |part| {
                vec![
                    Value::Int(*id),
                    Value::Int(part),
                    Value::Int(id * 100 + part),
                ]
            })
        })
        .collect()
}
fn assert_state(dir: &Path, ids: &[i64]) {
    let verified = Database::verify(dir).unwrap();
    assert_eq!((verified.graphs, verified.nodes), (1, ids.len() * 2));
    let db = Database::open(dir).unwrap();
    assert_eq!(
        verified.recovery.position,
        db.recovery_info().unwrap().position
    );
    let ExecutionOutcome::Rows { result, .. } = db
        .session(&path())
        .unwrap()
        .execute("MATCH (n:Txn) RETURN n.id, n.part, n.value ORDER BY n.id, n.part")
        .unwrap()
    else {
        panic!("rows")
    };
    let actual: Vec<_> = result.rows().iter().map(|r| r.values().to_vec()).collect();
    assert_eq!(actual, expected(ids));
}

#[test]
#[ignore = "bounded parent-invoked SIGKILL child only"]
fn child() {
    let path = PathBuf::from(std::env::var_os("SELENE_PR07_DIR").unwrap());
    let phase = std::env::var("SELENE_PR07_PHASE").unwrap();
    let dir = StoreDirectory::open(&path).unwrap();
    if phase == "reader.captured" {
        dir.test_at_phase("reader.captured", pause);
        Database::verify_in(&DatabaseDirectory(dir)).unwrap();
        panic!("parent must kill verifier");
    }
    let db = Database::open_in(&DatabaseDirectory(dir.clone())).unwrap();
    let point = match phase.as_str() {
        "commit.appended" => "commit.appended",
        "commit.synchronized" => "commit.synchronized",
        "commit.published" => "commit.published",
        "current.replace" => "current.replace",
        "current.dir_sync" => "current.dir_sync",
        "prune.dir_sync" => "prune.dir_sync",
        _ => panic!("unknown phase"),
    };
    dir.test_at_phase(point, pause);
    if phase.starts_with("commit.") {
        transaction(&db, 3);
    } else if phase == "prune.dir_sync" {
        db.prune().unwrap();
    } else {
        db.checkpoint().unwrap();
    }
    panic!("parent must kill at phase");
}

#[test]
fn actual_facade_process_crash_preserves_ack_sets_and_whole_unknown_transactions() {
    for phase in [
        "commit.appended",
        "commit.synchronized",
        "commit.published",
        "current.replace",
        "current.dir_sync",
        "prune.dir_sync",
        "reader.captured",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path()).unwrap();
        db.catalog()
            .create_schema(
                &SchemaPath::regular("selene", "memory").unwrap(),
                CreatePolicy::Strict,
            )
            .unwrap();
        db.catalog()
            .create_graph(&path(), None, CreatePolicy::Strict)
            .unwrap();
        transaction(&db, 1);
        db.checkpoint().unwrap();
        transaction(&db, 2);
        let canceled = db.session(&path()).unwrap();
        canceled
            .start_transaction(TransactionAccessMode::ReadWrite)
            .unwrap();
        canceled
            .execute("INSERT (:Txn {id: 99, part: 0, value: 9900})")
            .unwrap();
        canceled
            .execute("INSERT (:Txn {id: 99, part: 1, value: 9901})")
            .unwrap();
        canceled.rollback_transaction().unwrap();
        drop(canceled);
        if phase == "prune.dir_sync" {
            db.checkpoint().unwrap();
            db.checkpoint().unwrap();
        }
        drop(db);
        Process::spawn(dir.path(), phase).kill();
        // Before sync is an unknown acknowledgment outcome, not a proven cancel.
        // In this intact-page-cache witness the complete append is recovered whole.
        assert_state(
            dir.path(),
            if phase.starts_with("commit.") {
                &[1, 2, 3]
            } else {
                &[1, 2]
            },
        );
        let db = Database::open(dir.path()).unwrap();
        // A pre-CURRENT crash can leave a complete unselected successor. Open
        // must not adopt/overwrite it; explicit prune clears eligible artifacts.
        assert!(db.prune().unwrap().cleanup_error.is_none(), "{phase}");
        db.checkpoint().unwrap();
        db.checkpoint().unwrap();
        assert!(db.prune().unwrap().cleanup_error.is_none());
    }
}
