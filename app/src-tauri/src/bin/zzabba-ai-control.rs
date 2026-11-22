#[cfg(target_os = "linux")]
#[path = "../ai/control_wire.rs"]
mod control_wire;
#[cfg(target_os = "linux")]
#[path = "../ai/shared_wire.rs"]
mod shared_wire;

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next();
    let result = match (mode.as_deref().and_then(|arg| arg.to_str()), args.next()) {
        (Some("--shared-control"), None) => linux::shared::control(),
        (Some("--shared-pair"), None) => linux::shared::pair(),
        (Some("--shared-claim"), None) => linux::shared::claim(),
        _ => Err(std::io::ErrorKind::InvalidInput.into()),
    };
    if result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(target_os = "linux")]
#[path = "../ai"]
mod linux {
    #[path = "control_node_shared.rs"]
    pub(super) mod shared;

    use super::control_wire::{self, ClientFrame, NodeFrame, WireOperation, WireStream};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::fmt::Write as _;
    use std::io::{self, Read, Write};
    use std::mem::MaybeUninit;
    use std::num::NonZeroU64;
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};

    const INPUT: RawFd = libc::STDIN_FILENO;
    const CHUNK: usize = 1024;
    static CLOSING: AtomicBool = AtomicBool::new(false);

    extern "C" fn close_on_signal(_: libc::c_int) {
        CLOSING.store(true, Ordering::Relaxed);
    }

    fn invalid() -> io::Error {
        io::ErrorKind::InvalidData.into()
    }

    fn checked(result: libc::c_int) -> io::Result<()> {
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn terminal_flags() -> io::Result<libc::termios> {
        let mut flags = MaybeUninit::uninit();
        // SAFETY: tcgetattr 초기화에 성공한 경우에만 구조체를 읽는다.
        unsafe {
            checked(libc::tcgetattr(INPUT, flags.as_mut_ptr()))?;
            Ok(flags.assume_init())
        }
    }

    struct Terminal(libc::termios);

    impl Terminal {
        fn enter() -> io::Result<Self> {
            let old = Self(terminal_flags()?);
            let mut raw = old.0;
            // SAFETY: raw는 초기화된 termios이며 INPUT은 위에서 검사한 FD다.
            unsafe {
                libc::cfmakeraw(&mut raw);
                raw.c_lflag &= !libc::ECHONL;
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                checked(libc::tcsetattr(INPUT, libc::TCSAFLUSH, &raw))?;
            }
            let actual = terminal_flags()?;
            if (
                actual.c_iflag,
                actual.c_oflag,
                actual.c_cflag,
                actual.c_lflag,
            ) != (raw.c_iflag, raw.c_oflag, raw.c_cflag, raw.c_lflag)
                || actual.c_cc[libc::VMIN] != 1
                || actual.c_cc[libc::VTIME] != 0
            {
                return Err(invalid());
            }
            Ok(old)
        }
    }

    impl Drop for Terminal {
        fn drop(&mut self) {
            // SAFETY: 저장한 termios를 표준 입력 TTY에 복구한다.
            unsafe { libc::tcsetattr(INPUT, libc::TCSAFLUSH, &self.0) };
        }
    }

    struct Job {
        id: NonZeroU64,
        child: Child,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        stopped: bool,
    }

    impl Job {
        fn spawn(id: NonZeroU64, operation: WireOperation) -> io::Result<Self> {
            let program = match operation {
                WireOperation::Uptime => "/usr/bin/uptime",
            };
            let mut command = Command::new(program);
            command
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // SAFETY: fork 뒤에는 비동기 시그널 안전 호출만 한다. 설정 전 부모 단절도 확인한다.
            unsafe {
                let parent = libc::getpid();
                command.pre_exec(move || {
                    checked(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0))?;
                    if libc::getppid() != parent {
                        libc::_exit(1);
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn()?;
            Ok(Self {
                id,
                stdout: child.stdout.take(),
                stderr: child.stderr.take(),
                child,
                stopped: false,
            })
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    struct Session<W> {
        out: W,
        nonce: String,
        channel: Option<String>,
        input_seq: u64,
        output_seq: u64,
        last_job: Option<(NonZeroU64, bool)>,
        job: Option<Job>,
        main_tty: Option<std::fs::File>,
    }

    impl<W: Write> Session<W> {
        fn send(&mut self, frame: NodeFrame<'_>) -> io::Result<()> {
            write_frame(&mut self.out, frame)
        }

        fn receive(&mut self, line: &[u8]) -> io::Result<()> {
            if CLOSING.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let frame = serde_json::from_slice(line)?;
            if !matches!(frame, ClientFrame::Hello { .. }) {
                return Err(invalid());
            }
            self.handle(frame)
        }

        fn handle(&mut self, frame: ClientFrame<'_>) -> io::Result<()> {
            let Some(channel) = self.channel.as_deref() else {
                if let ClientFrame::Hello {
                    version,
                    nonce,
                    channel,
                } = frame
                    && version == control_wire::VERSION
                    && nonce == self.nonce
                    && channel != nonce
                    && channel.len() == 32
                    && channel
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    self.channel = Some(channel.to_owned());
                    return self.send(NodeFrame::HelloAck { version, channel });
                }
                return Err(invalid());
            };
            match frame {
                ClientFrame::Start {
                    channel: received,
                    job_id,
                    seq,
                    operation,
                } if received == channel
                    && self.job.is_none()
                    && self.last_job.is_none_or(|(last, _)| job_id > last) =>
                {
                    advance(&mut self.input_seq, seq)?;
                    self.job = Some(Job::spawn(job_id, operation)?);
                    write_frame(
                        &mut self.out,
                        NodeFrame::Started {
                            channel,
                            job_id,
                            seq: next(&mut self.output_seq)?,
                        },
                    )
                }
                ClientFrame::Stop {
                    channel: received,
                    job_id,
                    seq,
                } if received == channel => {
                    advance(&mut self.input_seq, seq)?;
                    match &mut self.job {
                        Some(job) if job.id == job_id && !job.stopped => {
                            job.child.kill()?;
                            job.stopped = true;
                        }
                        // exited와 교차한 stop 한 건은 추가 프레임 없이 소비한다.
                        None if self.last_job == Some((job_id, false)) => {
                            self.last_job = Some((job_id, true));
                        }
                        _ => return Err(invalid()),
                    }
                    Ok(())
                }
                _ => Err(invalid()),
            }
        }

        fn drain(&mut self, stdout: bool, stderr: bool) -> io::Result<()> {
            let Some(job) = &mut self.job else {
                return Ok(());
            };
            let channel = self.channel.as_deref().ok_or_else(invalid)?;
            let mut bytes = [0; CHUNK];
            let mut encoded = [0; CHUNK.div_ceil(3) * 4];
            for (ready, stream) in [(stdout, WireStream::Stdout), (stderr, WireStream::Stderr)] {
                if !ready {
                    continue;
                }
                let size = match stream {
                    WireStream::Stdout => read_pipe(&mut job.stdout, &mut bytes)?,
                    WireStream::Stderr => read_pipe(&mut job.stderr, &mut bytes)?,
                };
                if size != 0 {
                    if let Some(tty) = &mut self.main_tty {
                        tty.write_all(&bytes[..size])?;
                    }
                    let len = STANDARD
                        .encode_slice(&bytes[..size], &mut encoded)
                        .map_err(|_| invalid())?;
                    write_frame(
                        &mut self.out,
                        NodeFrame::Output {
                            channel,
                            job_id: job.id,
                            seq: next(&mut self.output_seq)?,
                            stream,
                            data: std::str::from_utf8(&encoded[..len]).map_err(|_| invalid())?,
                        },
                    )?;
                }
            }
            if job.stdout.is_none()
                && job.stderr.is_none()
                && let Some(status) = job.child.try_wait()?
            {
                let job_id = job.id;
                self.last_job = Some((job_id, job.stopped));
                self.job = None;
                write_frame(
                    &mut self.out,
                    NodeFrame::Exited {
                        channel,
                        job_id,
                        seq: next(&mut self.output_seq)?,
                        code: status.code(),
                        signal: status.signal(),
                    },
                )?;
            }
            Ok(())
        }
    }

    fn next(seq: &mut u64) -> io::Result<NonZeroU64> {
        let value = seq
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or_else(invalid)?;
        *seq = value.get();
        Ok(value)
    }

    fn advance(last: &mut u64, received: NonZeroU64) -> io::Result<()> {
        if next(last)? != received {
            return Err(invalid());
        }
        Ok(())
    }

    fn write_frame(out: &mut impl Write, frame: impl serde::Serialize) -> io::Result<()> {
        let mut bytes = [0; control_wire::MAX_FRAME_BYTES];
        let mut body = &mut bytes[..control_wire::MAX_FRAME_BYTES - 1];
        serde_json::to_writer(&mut body, &frame)?;
        let len = control_wire::MAX_FRAME_BYTES - 1 - body.len();
        bytes[len] = b'\n';
        out.write_all(&bytes[..=len])
    }

    fn read_pipe(pipe: &mut Option<impl Read>, bytes: &mut [u8]) -> io::Result<usize> {
        let Some(reader) = pipe else { return Ok(0) };
        match reader.read(bytes) {
            Ok(0) => {
                *pipe = None;
                Ok(0)
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(0),
            result => result,
        }
    }

    fn poll_fd(fd: RawFd) -> libc::pollfd {
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }
    }

    struct Transport(RawFd, libc::c_int);

    impl Transport {
        fn enter(fd: RawFd) -> io::Result<Self> {
            // SAFETY: 표준 출력의 상태 플래그만 읽고 갱신한다.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                checked(flags)?;
                checked(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
                Ok(Self(fd, flags))
            }
        }
    }

    impl Drop for Transport {
        fn drop(&mut self) {
            // SAFETY: 시작 시 저장한 상태 플래그를 같은 FD에 복구한다.
            unsafe { libc::fcntl(self.0, libc::F_SETFL, self.1) };
        }
    }

    impl Write for Transport {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            loop {
                if CLOSING.load(Ordering::Relaxed) {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                let mut fds = [poll_fd(INPUT), poll_fd(self.0)];
                fds[0].events = 0;
                fds[1].events = libc::POLLOUT;
                // SAFETY: 두 pollfd와 bytes는 이 호출 동안 유효하다.
                unsafe {
                    match checked(libc::poll(fds.as_mut_ptr(), 2, 100)) {
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        result => result?,
                    }
                    if fds.iter().any(|fd| {
                        fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
                    }) {
                        return Err(io::ErrorKind::BrokenPipe.into());
                    }
                    if fds[1].revents & libc::POLLOUT == 0 {
                        continue;
                    }
                    let size = libc::write(self.0, bytes.as_ptr().cast(), bytes.len());
                    if size >= 0 {
                        return Ok(size as usize);
                    }
                }
                let err = io::Error::last_os_error();
                if !matches!(
                    err.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    return Err(err);
                }
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn setup() -> io::Result<()> {
        // SAFETY: 호출 인자는 상수 또는 초기화된 구조체이며 시그널 핸들러는 원자 플래그만 설정한다.
        unsafe {
            if libc::geteuid() == 0
                || libc::getuid() != libc::geteuid()
                || libc::getgid() != libc::getegid()
            {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            checked(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = close_on_signal as *const () as libc::sighandler_t;
            checked(libc::sigemptyset(&mut action.sa_mask))?;
            for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
                checked(libc::sigaction(signal, &action, std::ptr::null_mut()))?;
            }
        }
        Ok(())
    }

    fn nonce() -> io::Result<String> {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let mut nonce = String::with_capacity(random.len() * 2);
        for byte in random {
            write!(nonce, "{byte:02x}").map_err(io::Error::other)?;
        }
        Ok(nonce)
    }
}
