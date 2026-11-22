use std::{io, process::{ExitStatus, Stdio}, sync::Arc, time::Duration};

use tokio::{io::AsyncReadExt, process::{ChildStderr, Command}, task::JoinHandle, time::timeout};
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::task_tracker::TaskTrackerToken;

use crate::ssh::{Client, ConnectionPlan};

use super::{Connection, Entry, Error, File, Result, SaveOutcome};

const DEADLINE: Duration = Duration::from_secs(10);
const STDERR_LIMIT: usize = 4096;

pub(crate) struct Session {
    connection: Connection,
    process: Process,
    deadline: Duration,
}

struct Process {
    stop: DropGuard,
    ended: CancellationToken,
    worker: Option<JoinHandle<io::Result<(ExitStatus, String)>>>,
}

impl Session {
    pub(crate) async fn connect(plan: Arc<ConnectionPlan>, cancel: CancellationToken, worker: TaskTrackerToken) -> Result<Self> {
        let command = Self::command(&plan);
        Self::spawn(plan, cancel, worker, command, DEADLINE).await
    }

    pub(super) fn command(plan: &ConnectionPlan) -> Command {
        let invocation = plan.invocation(Client::Sftp);
        let mut command = Command::new(invocation.program);
        command.args(invocation.args);
        for &(key, value) in invocation.environment {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command.env_remove("BW_SESSION");
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        command
    }

    pub(super) async fn spawn(
        plan: Arc<ConnectionPlan>, cancel: CancellationToken, worker: TaskTrackerToken,
        mut command: Command, deadline: Duration,
    ) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(Error::Disconnected);
        }
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .kill_on_drop(true).spawn().map_err(Error::Spawn)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let ended = CancellationToken::new();
        let notify = ended.clone().drop_guard();
        let worker_stop = cancel.clone();
        let worker_plan = Arc::clone(&plan);
        let worker = tokio::spawn(async move {
            let _worker = worker;
            let _notify = notify;
            // SSH가 종료할 때까지 앱 소유 설정·PIN 파일을 유지한다.
            let _plan = worker_plan;
            let mut diagnostics = Vec::new();
            let mut stderr = stderr;
            let status = tokio::select! {
                status = child.wait() => status,
                _ = worker_stop.cancelled() => {
                    if child.try_wait()?.is_none() { child.start_kill()?; }
                    child.wait().await
                },
                never = async { drain_stderr(&mut stderr, &mut diagnostics).await; std::future::pending::<std::convert::Infallible>().await } => match never {},
            }?;
            let _ = timeout(Duration::from_millis(100), drain_stderr(&mut stderr, &mut diagnostics)).await;
            Ok((status, String::from_utf8_lossy(&diagnostics).into_owned()))
        });
        let mut process = Process { stop: cancel.drop_guard(), ended, worker: Some(worker) };
        let result = process.race(deadline, Connection::from_subsystem(plan, stdin, stdout, 60)).await;
        let connection = match result.and_then(|result| result) {
            Ok(connection) => connection,
            Err(cause) => {
                let (status, stderr) = process.reap().await?.expect("preparation owns its worker");
                return Err(Error::Prepare { cause: Box::new(cause), status, stderr });
            }
        };
        Ok(Self { connection, process, deadline })
    }

    pub(crate) async fn close(mut self) -> Result<()> {
        self.process.reap().await.map(|_| ())
    }

    /// 명시적 디렉터리는 승인 절차를 끝낸 Rust 호출자가 넘긴다.
    pub(crate) async fn list(&mut self, approved_directory: Option<&str>) -> Result<Vec<Entry>> {
        self.process.run(self.deadline, self.connection.list(approved_directory)).await
    }

    pub(crate) async fn list_directory(&mut self, entry: &Entry) -> Result<Vec<Entry>> {
        self.process.run(self.deadline, self.connection.list_directory(entry)).await
    }

    pub(crate) async fn open(&mut self, entry: &Entry) -> Result<(File, String)> {
        self.process.run(self.deadline, self.connection.open(entry)).await
    }

    pub(in crate::editor) async fn open_absolute(&mut self, path: &str) -> Result<(File, String)> {
        self.process.run(self.deadline, self.connection.open_absolute(path)).await
    }

    #[cfg(test)]
    pub(super) fn in_memory(connection: Connection) -> Self {
        Self {
            connection,
            process: Process {
                stop: CancellationToken::new().drop_guard(),
                ended: CancellationToken::new(),
                worker: None,
            },
            deadline: DEADLINE,
        }
    }

    pub(crate) async fn reopen(&mut self, file: &File) -> Result<(File, String)> {
        self.process.run(self.deadline, self.connection.reopen(file)).await
    }

    pub(crate) async fn save(&mut self, file: &mut File, edited: &str) -> Result<SaveOutcome> {
        let already_unconfirmed = file.unconfirmed;
        match self.process.run(self.deadline, async { Ok(self.connection.save(file, edited).await) }).await {
            Err(error) if !already_unconfirmed && file.unconfirmed => Ok(SaveOutcome::Unconfirmed(error)),
            outcome => outcome,
        }
    }
}

impl Process {
    async fn race<T>(&self, deadline: Duration, operation: impl Future<Output = T>) -> Result<T> {
        tokio::select! {
            biased;
            _ = self.stop.token().cancelled() => Err(Error::Disconnected),
            _ = self.ended.cancelled() => Err(Error::Disconnected),
            result = timeout(deadline, operation) => result.map_err(|_| Error::TimedOut),
        }
    }

    async fn run<T>(&mut self, deadline: Duration, operation: impl Future<Output = Result<T>>) -> Result<T> {
        match self.race(deadline, operation).await {
            Ok(result) => result,
            Err(error) => {
                self.reap().await?;
                Err(error)
            }
        }
    }

    async fn reap(&mut self) -> Result<Option<(ExitStatus, String)>> {
        self.stop.token().cancel();
        let Some(worker) = &mut self.worker else { return Ok(None) };
        let result = worker.await;
        self.worker.take();
        result.map_err(|error| Error::Reap(io::Error::other(error)))?.map(Some).map_err(Error::Reap)
    }
}

async fn drain_stderr(stderr: &mut ChildStderr, diagnostics: &mut Vec<u8>) {
    let mut buffer = [0; STDERR_LIMIT];
    while let Ok(count @ 1..) = stderr.read(&mut buffer).await {
        let count = count.min(STDERR_LIMIT - diagnostics.len());
        diagnostics.extend_from_slice(&buffer[..count]);
    }
}
