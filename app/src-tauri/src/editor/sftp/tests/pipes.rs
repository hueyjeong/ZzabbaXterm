use std::{path::Path, time::Duration};

use tokio::{net::TcpListener, process::Command, task::JoinHandle, time::timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use super::*;

const LIMIT: Duration = Duration::from_secs(5);

fn command(plan: &ConnectionPlan, fixture: &Path) -> Command {
    let ssh = Session::command(plan);
    let ssh = ssh.as_std();
    let mut command = Command::new(fixture);
    command.args(ssh.get_args());
    for (key, value) in ssh.get_envs() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    command
}

async fn peer(plan: &ConnectionPlan, fixture: &Path, root: &Path, scenario: Scenario) -> (Command, JoinHandle<Fixture>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cmd = command(plan, fixture);
    cmd.env("ZZABBA_FIXTURE_ADDR", listener.local_addr().unwrap().to_string());
    let fixture = Fixture::new(root.to_owned(), scenario);
    let server = tokio::spawn(async move {
        let (stream, _) = timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        fixture.run(stream).await
    });
    (cmd, server)
}

#[tokio::test]
async fn isolated_binary_pipes_keep_paths_revisions_and_children_separate() {
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join(format!("ssh fixture{}", std::env::consts::EXE_SUFFIX));
    let compiled = std::process::Command::new(option_env!("RUSTC").unwrap_or("rustc"))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/editor/sftp/tests/pipe_fixture.rs"))
        .arg("-o").arg(&fixture).output().unwrap();
    assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));

    let plan = plan(&root.path().join("trust"), "editor");
    let first_dir = tempfile::tempdir_in(root.path()).unwrap();
    let other_dir = tempfile::tempdir_in(root.path()).unwrap();
    let first_workers = TaskTracker::new();
    let other_workers = TaskTracker::new();
    let owner = CancellationToken::new();
    let (cmd, first_server) = peer(&plan, &fixture, first_dir.path(), Scenario::ExternalEdit).await;
    let mut first = Session::spawn(plan.clone(), owner.child_token(), first_workers.token(), cmd, LIMIT).await.unwrap();
    let entries = first.list(Some(DIR)).await.unwrap();
    let entry = entries.iter().find(|entry| entry.name() == "한글.txt").unwrap();
    let (mut file, text) = first.open(entry).await.unwrap();
    assert_eq!(text, ORIGINAL);
    assert_eq!(ORIGINAL.len(), EXTERNAL.len());
    assert!(matches!(first.save(&mut file, EDITED).await.unwrap(),
        SaveOutcome::Conflict { remote: RemoteVersion::Text(remote), cleanup: Cleanup::Removed } if remote == EXTERNAL));
    assert_eq!(fs::read_to_string(first_dir.path().join("original")).unwrap(), EXTERNAL);

    let other_plan = super::plan(&root.path().join("other-trust"), "editor");
    let (cmd, other_server) = peer(&other_plan, &fixture, other_dir.path(), Scenario::Normal).await;
    let mut other = Session::spawn(other_plan, CancellationToken::new(), other_workers.token(), cmd, LIMIT).await.unwrap();
    assert!(matches!(other.open(entry).await, Err(Error::WrongConnectionPlan)));
    assert!(matches!(other.reopen(&file).await, Err(Error::WrongConnectionPlan)));
    assert!(matches!(other.save(&mut file, EDITED).await.unwrap(),
        SaveOutcome::Failed { error: Error::WrongConnectionPlan, cleanup: None, .. }));

    owner.cancel();
    assert!(matches!(first.reopen(&file).await, Err(Error::Disconnected)));
    assert!(first_workers.is_empty());
    assert!(!timeout(LIMIT, first_server).await.unwrap().unwrap().renamed);
    let other_entry = other.list(None).await.unwrap().pop().unwrap();
    let (mut other_file, text) = other.open(&other_entry).await.unwrap();
    assert_eq!(text, ORIGINAL);
    assert!(matches!(other.save(&mut other_file, EDITED).await.unwrap(), SaveOutcome::Saved));
    other.close().await.unwrap();
    assert!(other_workers.is_empty());
    assert!(timeout(LIMIT, other_server).await.unwrap().unwrap().renamed);
    assert_eq!(fs::read_to_string(other_dir.path().join("original")).unwrap(), EDITED);

    let workers = TaskTracker::new();
    let mut denied = command(&plan, &fixture);
    denied.env("ZZABBA_FIXTURE_DENY", "1");
    let result = Session::spawn(plan.clone(), CancellationToken::new(), workers.token(), denied, LIMIT).await;
    assert!(matches!(result, Err(Error::Prepare { status, stderr, .. })
        if status.code() == Some(42) && stderr.contains("fixture: subsystem denied")));
    assert!(workers.is_empty());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut stalled = command(&plan, &fixture);
    stalled.env("ZZABBA_FIXTURE_ADDR", listener.local_addr().unwrap().to_string());
    let server = tokio::spawn(async move {
        let (mut stream, _) = timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        let mut init = [0; 9];
        stream.read_exact(&mut init).await.unwrap();
        assert_eq!(init, [0, 0, 0, 5, 1, 0, 0, 0, 3]);
        assert_socket_closed(stream.read(&mut init).await);
    });
    let result = Session::spawn(plan, CancellationToken::new(), workers.token(), stalled, Duration::from_secs(2)).await;
    assert!(matches!(result, Err(Error::Prepare { cause, .. }) if matches!(*cause, Error::TimedOut)));
    assert!(workers.is_empty());
    timeout(LIMIT, server).await.unwrap().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plan = super::plan(&root.path().join("timeout-trust"), "editor");
    let mut stalled = command(&plan, &fixture);
    stalled.env("ZZABBA_FIXTURE_ADDR", listener.local_addr().unwrap().to_string());
    let server = tokio::spawn(async move {
        let (mut stream, _) = timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        let mut init = [0; 9];
        stream.read_exact(&mut init).await.unwrap();
        stream.write_all(&[0, 0, 0, 5, 2, 0, 0, 0, 3]).await.unwrap();
        let len = stream.read_u32().await.unwrap();
        let mut request = vec![0; len as usize];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(request[0], 16);
        assert_socket_closed(stream.read(&mut init).await);
    });
    let mut session = Session::spawn(plan, CancellationToken::new(), workers.token(), stalled, Duration::from_secs(2)).await.unwrap();
    assert!(matches!(session.list(None).await, Err(Error::TimedOut)));
    assert!(workers.is_empty());
    timeout(LIMIT, server).await.unwrap().unwrap();
}

fn assert_socket_closed(result: std::io::Result<usize>) {
    match result {
        Ok(0) => (),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
        other => panic!("소켓 종료가 필요합니다: {other:?}"),
    }
}
