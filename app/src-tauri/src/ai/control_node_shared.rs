use super::*;
use crate::shared_wire::{Client, Node, Ticket};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const PAIR: Duration = Duration::from_secs(60);
const APPROVAL: Duration = Duration::from_secs(15);
const IO: Duration = Duration::from_secs(2);

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Local<'a> {
    Pair { pid: i32, secret: &'a str },
    Claim { pid: i32 },
    Paired { shell_generation: NonZeroU64 },
    Denied,
}

fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn socket_path(locator: &str) -> io::Result<PathBuf> {
    if locator.len() != 32
        || !locator
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(invalid());
    }
    let dir = PathBuf::from(format!("/run/user/{}", uid()));
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid() || meta.mode() & 0o7777 != 0o700 {
        return Err(invalid());
    }
    Ok(dir.join(format!("zzabba-ai-{locator}.sock")))
}

struct SocketPath(PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn peer(socket: &UnixStream) -> io::Result<libc::ucred> {
    let mut cred = MaybeUninit::<libc::ucred>::uninit();
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most len bytes to cred.
    unsafe {
        checked(libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(),
            &mut len,
        ))?;
        if len as usize != std::mem::size_of::<libc::ucred>() {
            return Err(invalid());
        }
        let cred = cred.assume_init();
        if cred.uid != uid() || cred.pid <= 0 {
            return Err(invalid());
        }
        let expected = fs::metadata("/proc/self/exe")?;
        let actual = fs::metadata(format!("/proc/{}/exe", cred.pid))?;
        if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
            return Err(invalid());
        }
        Ok(cred)
    }
}

struct Proc {
    parent: i32,
    group: i32,
    session: i32,
    tty: i32,
    foreground: i32,
}

fn proc_stat(pid: i32) -> io::Result<Proc> {
    let stat = fs::read(format!("/proc/{pid}/stat"))?;
    let end = stat.iter().rposition(|&b| b == b')').ok_or_else(invalid)?;
    let mut fields = stat[end + 1..]
        .split(|b| *b == b' ')
        .filter(|f| !f.is_empty())
        .skip(1);
    let mut number = || -> io::Result<i32> {
        std::str::from_utf8(fields.next().ok_or_else(invalid)?)
            .map_err(|_| invalid())?
            .parse()
            .map_err(|_| invalid())
    };
    Ok(Proc {
        parent: number()?,
        group: number()?,
        session: number()?,
        tty: number()?,
        foreground: number()?,
    })
}

fn foreground(shell: i32, helper: i32) -> io::Result<()> {
    let bash = fs::metadata("/usr/bin/bash")?;
    let exe = fs::metadata(format!("/proc/{shell}/exe"))?;
    let a = proc_stat(shell)?;
    let b = proc_stat(helper)?;
    if (exe.dev(), exe.ino()) != (bash.dev(), bash.ino())
        || b.parent != shell
        || a.tty == 0
        || a.tty != b.tty
        || a.session != b.session
        || a.group != b.group
        || a.group != a.foreground
        || a.group != b.foreground
    {
        return Err(invalid());
    }
    Ok(())
}

fn pidfd(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes a PID and zero flags; success returns a fresh FD.
    unsafe {
        let fd = libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd;
        checked(fd)?;
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn expired(fd: RawFd) -> io::Result<bool> {
    let mut p = poll_fd(fd);
    // SAFETY: p is a valid single-element pollfd array.
    checked(unsafe { libc::poll(&mut p, 1, 0) })?;
    Ok(p.revents != 0)
}

struct Wire<R: BufRead> {
    reader: R,
    line: Vec<u8>,
}

impl<R: BufRead> Wire<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line: Vec::with_capacity(control_wire::MAX_FRAME_BYTES),
        }
    }

    fn read(&mut self) -> io::Result<&[u8]> {
        self.line.clear();
        (&mut self.reader)
            .take(control_wire::MAX_FRAME_BYTES as u64)
            .read_until(b'\n', &mut self.line)?;
        if self.line.pop() != Some(b'\n') || self.line.contains(&b'\r') {
            return Err(invalid());
        }
        Ok(&self.line)
    }

    fn read_control(&mut self) -> io::Result<Option<&[u8]>> {
        loop {
            let available = match self.reader.fill_buf() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                result => result?,
            };
            if available.is_empty() {
                return Err(invalid());
            }
            let end = available.iter().position(|&byte| byte == b'\n');
            let size = end.map_or(available.len(), |end| end + 1);
            if size + self.line.len() > control_wire::MAX_FRAME_BYTES {
                return Err(invalid());
            }
            self.line.extend_from_slice(&available[..size]);
            self.reader.consume(size);
            if end.is_some() {
                self.line.pop();
                if self.line.contains(&b'\r') {
                    return Err(invalid());
                }
                return Ok(Some(&self.line));
            }
        }
    }
}

type SocketWire = Wire<BufReader<UnixStream>>;

fn timed_socket(socket: UnixStream) -> io::Result<SocketWire> {
    socket.set_read_timeout(Some(IO))?;
    socket.set_write_timeout(Some(IO))?;
    Ok(Wire::new(BufReader::new(socket)))
}

fn events(fds: &mut [libc::pollfd], ready: bool) -> io::Result<()> {
    if CLOSING.load(Ordering::Relaxed) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    // SAFETY: fds contains initialized pollfd entries for its entire length.
    checked(unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, if ready { 0 } else { 50 }) })
}

fn live(fd: &libc::pollfd) -> io::Result<()> {
    if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        Err(io::ErrorKind::BrokenPipe.into())
    } else {
        Ok(())
    }
}

pub(crate) fn control() -> io::Result<()> {
    setup()?;
    let locator = nonce()?;
    let secret = nonce()?;
    let path = socket_path(&locator)?;
    // SAFETY: the process is single-threaded, and the parent directory is 0700.
    let mask = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(&path);
    unsafe { libc::umask(mask) };
    let listener = listener?;
    let path = SocketPath(path);
    fs::set_permissions(&path.0, fs::Permissions::from_mode(0o600))?;
    // SAFETY: before fork there are no threads or held stdio locks. The parent only supervises cleanup.
    let parent = unsafe { libc::getpid() };
    let child = unsafe { libc::fork() };
    checked(child)?;
    // The watcher owns the path even if either process is killed independently.
    if child != 0 {
        drop(listener);
        let mut status = 0;
        loop {
            if CLOSING.load(Ordering::Relaxed) {
                unsafe { libc::kill(child, libc::SIGTERM) };
            }
            match checked(unsafe { libc::waitpid(child, &mut status, 0) }) {
                Ok(_) if status == 0 => return Ok(()),
                Ok(_) => return Err(invalid()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
    unsafe {
        checked(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0))?;
        if libc::getppid() != parent {
            return Err(invalid());
        }
    }
    broker(listener, &format!("{locator}{secret}"), &secret)
}

struct Shell {
    pid: i32,
    lifetime: OwnedFd,
    generation: NonZeroU64,
}

fn pair_peer(wire: &mut SocketWire, secret: &str, deadline: Instant) -> io::Result<Shell> {
    let cred = peer(wire.reader.get_ref())?;
    let Local::Pair {
        pid,
        secret: supplied,
    } = serde_json::from_slice(wire.read()?)?
    else {
        return Err(invalid());
    };
    if supplied != secret || pid <= 0 || Instant::now() >= deadline {
        return Err(invalid());
    }
    let lifetime = pidfd(pid)?;
    foreground(pid, cred.pid)?;
    let mut random = [0; 8];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let generation = NonZeroU64::new(u64::from_ne_bytes(random) | 1).unwrap();
    write_frame(
        wire.reader.get_mut(),
        Local::Paired {
            shell_generation: generation,
        },
    )?;
    Ok(Shell {
        pid,
        lifetime,
        generation,
    })
}

fn broker(listener: UnixListener, code: &str, secret: &str) -> io::Result<()> {
    let _terminal = Terminal::enter()?;
    // A partial control frame must release the node even if the peer stops transmitting.
    let _nonblocking_input = Transport::enter(INPUT)?;
    // SAFETY: dup returns a new owned descriptor for the already validated TTY.
    let fd = unsafe { libc::fcntl(INPUT, libc::F_DUPFD_CLOEXEC, 3) };
    checked(fd)?;
    let stdin = unsafe { File::from_raw_fd(fd) };
    let mut input = Wire::new(BufReader::new(stdin));
    let mut out = Transport::enter(libc::STDOUT_FILENO)?;
    let nonce = nonce()?;
    write_frame(
        &mut out,
        NodeFrame::Ready {
            version: control_wire::VERSION,
            nonce: &nonce,
        },
    )?;
    let mut session = Session {
        out,
        nonce,
        channel: None,
        input_seq: 0,
        output_seq: 0,
        last_job: None,
        job: None,
        main_tty: None,
    };
    loop {
        let mut first = [poll_fd(INPUT)];
        events(&mut first, false)?;
        live(&first[0])?;
        if first[0].revents != 0
            && let Some(line) = input.read_control()?
        {
            session.receive(line)?;
            input.line.clear();
            break;
        }
    }
    let channel = session.channel.as_deref().ok_or_else(invalid)?;
    let end = Instant::now() + PAIR;
    write_frame(
        &mut session.out,
        Node::Pairing {
            channel,
            seq: next(&mut session.output_seq)?,
            code,
            expires_in_ms: PAIR.as_millis() as u64,
        },
    )?;
    let shell = loop {
        let mut fds = [poll_fd(INPUT), poll_fd(listener.as_raw_fd())];
        events(&mut fds, false)?;
        if Instant::now() >= end || !input.reader.buffer().is_empty() || fds[0].revents != 0 {
            return Err(invalid());
        }
        if fds[1].revents != 0 {
            live(&fds[1])?;
            let mut wire = timed_socket(listener.accept()?.0)?;
            if let Ok(shell) = pair_peer(&mut wire, secret, end) {
                break shell;
            }
        }
    };
    write_frame(
        &mut session.out,
        Node::Paired {
            channel,
            seq: next(&mut session.output_seq)?,
            uid: uid(),
            shell_generation: shell.generation,
        },
    )?;
    let mut reserved: Option<(Ticket, Instant)> = None;
    let mut claiming: Option<(SocketWire, i32)> = None;
    let mut running: Option<(SocketWire, NonZeroU64, u64, bool)> = None;
    let mut last = 0;
    loop {
        let peer_wire = if let Some((wire, ..)) = &running {
            Some(wire)
        } else {
            claiming.as_ref().map(|(wire, _)| wire)
        };
        let socket_ready = peer_wire.is_some_and(|wire| !wire.reader.buffer().is_empty());
        let socket_fd = peer_wire.map_or(-1, |wire| wire.reader.get_ref().as_raw_fd());
        let input_ready = !input.reader.buffer().is_empty();
        let mut fds = [
            poll_fd(INPUT),
            poll_fd(listener.as_raw_fd()),
            poll_fd(shell.lifetime.as_raw_fd()),
            poll_fd(socket_fd),
        ];
        events(&mut fds, input_ready || socket_ready)?;
        live(&fds[0])?;
        if fds[2].revents != 0 {
            return Ok(());
        }
        live(&fds[3])?;
        if reserved.is_some_and(|(_, deadline)| Instant::now() >= deadline) {
            reserved = None;
            if let Some((mut wire, _)) = claiming.take() {
                write_frame(wire.reader.get_mut(), Local::Denied)?;
            }
        }
        if (input_ready || fds[0].revents != 0)
            && let Some(line) = input.read_control()?
        {
            match serde_json::from_slice(line)? {
                Client::Reserve {
                    channel: got,
                    seq,
                    ticket,
                    operation: WireOperation::Uptime,
                } if got == channel
                    && ticket.shell_generation == shell.generation
                    && ticket.approval_id.get() > last
                    && running.is_none()
                    && reserved.is_none() =>
                {
                    advance(&mut session.input_seq, seq)?;
                    last = ticket.approval_id.get();
                    reserved = Some((ticket, Instant::now() + APPROVAL));
                }
                Client::Grant {
                    channel: got,
                    seq,
                    ticket,
                    operation: WireOperation::Uptime,
                } if got == channel && reserved.is_some_and(|(expected, _)| expected == ticket) => {
                    advance(&mut session.input_seq, seq)?;
                    let (_, deadline) = reserved.take().ok_or_else(invalid)?;
                    let (mut wire, helper) = claiming.take().ok_or_else(invalid)?;
                    if Instant::now() >= deadline || expired(shell.lifetime.as_raw_fd())? {
                        return Err(invalid());
                    }
                    foreground(shell.pid, helper)?;
                    write_frame(
                        wire.reader.get_mut(),
                        ClientFrame::Start {
                            channel,
                            seq: NonZeroU64::MIN,
                            job_id: ticket.approval_id,
                            operation: WireOperation::Uptime,
                        },
                    )?;
                    running = Some((wire, ticket.approval_id, 0, false));
                }
                Client::Deny {
                    channel: got,
                    seq,
                    ticket,
                } if got == channel && reserved.is_some_and(|(expected, _)| expected == ticket) => {
                    advance(&mut session.input_seq, seq)?;
                    reserved = None;
                    if let Some((mut wire, _)) = claiming.take() {
                        write_frame(wire.reader.get_mut(), Local::Denied)?;
                    }
                }
                Client::Stop {
                    channel: got,
                    seq,
                    job_id,
                } if got == channel => {
                    advance(&mut session.input_seq, seq)?;
                    if let Some((wire, id, _, _)) = &mut running {
                        if *id != job_id {
                            return Err(invalid());
                        }
                        write_frame(
                            wire.reader.get_mut(),
                            ClientFrame::Stop {
                                channel,
                                job_id,
                                seq: NonZeroU64::new(2).unwrap(),
                            },
                        )?;
                    } else if session.last_job != Some((job_id, false)) {
                        return Err(invalid());
                    } else {
                        session.last_job = Some((job_id, true));
                    }
                }
                _ => return Err(invalid()),
            }
            input.line.clear();
        }
        if let Some((wire, id, local_seq, started)) = &mut running {
            if socket_ready || fds[3].revents != 0 {
                let (message, done) = relay(
                    serde_json::from_slice(wire.read()?)?,
                    channel,
                    *id,
                    local_seq,
                    started,
                    &mut session.output_seq,
                )?;
                write_frame(&mut session.out, message)?;
                if done {
                    session.last_job = Some((*id, false));
                    running = None;
                }
            }
        } else if claiming.is_some() && (socket_ready || fds[3].revents != 0) {
            return Err(invalid());
        }
        if fds[1].revents != 0 {
            live(&fds[1])?;
            let mut wire = timed_socket(listener.accept()?.0)?;
            if let Some((ticket, deadline)) = reserved
                && claiming.is_none()
                && running.is_none()
                && let Ok(cred) = peer(wire.reader.get_ref())
                && let Ok(Local::Claim { pid }) = serde_json::from_slice(wire.read()?)
                && pid == shell.pid
                && Instant::now() < deadline
                && !expired(shell.lifetime.as_raw_fd())?
                && foreground(pid, cred.pid).is_ok()
            {
                write_frame(
                    &mut session.out,
                    Node::Claim {
                        channel,
                        seq: next(&mut session.output_seq)?,
                        ticket,
                    },
                )?;
                claiming = Some((wire, cred.pid));
            } else {
                let _ = write_frame(wire.reader.get_mut(), Local::Denied);
            }
        }
    }
}

fn relay<'a>(
    frame: NodeFrame<'a>,
    channel: &'a str,
    id: NonZeroU64,
    incoming: &mut u64,
    started: &mut bool,
    outgoing: &mut u64,
) -> io::Result<(NodeFrame<'a>, bool)> {
    let seq = next(outgoing)?;
    match frame {
        NodeFrame::Started {
            channel: got,
            job_id,
            seq: previous,
        } if got == channel && job_id == id && !*started => {
            advance(incoming, previous)?;
            *started = true;
            Ok((
                NodeFrame::Started {
                    channel,
                    job_id,
                    seq,
                },
                false,
            ))
        }
        NodeFrame::Output {
            channel: got,
            job_id,
            seq: previous,
            stream,
            data,
        } if got == channel && job_id == id && *started => {
            advance(incoming, previous)?;
            Ok((
                NodeFrame::Output {
                    channel,
                    job_id,
                    seq,
                    stream,
                    data,
                },
                false,
            ))
        }
        NodeFrame::Exited {
            channel: got,
            job_id,
            seq: previous,
            code,
            signal,
        } if got == channel && job_id == id && *started => {
            advance(incoming, previous)?;
            Ok((
                NodeFrame::Exited {
                    channel,
                    job_id,
                    seq,
                    code,
                    signal,
                },
                true,
            ))
        }
        _ => Err(invalid()),
    }
}

fn local() -> io::Result<(i32, String)> {
    setup()?;
    let mut line = String::new();
    io::stdin().take(96).read_to_string(&mut line)?;
    let (pid, value) = line.split_once('\n').ok_or_else(invalid)?;
    let pid = pid.parse().map_err(|_| invalid())?;
    let value = value.strip_suffix('\n').ok_or_else(invalid)?;
    if pid <= 0 {
        return Err(invalid());
    }
    foreground(pid, unsafe { libc::getpid() })?;
    Ok((pid, value.to_owned()))
}

fn connect(locator: &str) -> io::Result<SocketWire> {
    let path = socket_path(locator)?;
    let meta = fs::symlink_metadata(&path)?;
    if !meta.file_type().is_socket() || meta.uid() != uid() || meta.mode() & 0o7777 != 0o600 {
        return Err(invalid());
    }
    let wire = timed_socket(UnixStream::connect(path)?)?;
    peer(wire.reader.get_ref())?;
    Ok(wire)
}

pub(crate) fn pair() -> io::Result<()> {
    let (pid, code) = local()?;
    if code.len() != 64 || !code.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(invalid());
    }
    let (locator, secret) = code.split_at(32);
    let mut wire = connect(locator)?;
    write_frame(wire.reader.get_mut(), Local::Pair { pid, secret })?;
    if !matches!(serde_json::from_slice(wire.read()?)?, Local::Paired { .. }) {
        return Err(invalid());
    }
    println!("{locator}");
    Ok(())
}

pub(crate) fn claim() -> io::Result<()> {
    let (pid, locator) = local()?;
    let mut wire = connect(&locator)?;
    let socket = wire.reader.get_mut();
    socket.set_read_timeout(Some(APPROVAL + IO))?;
    write_frame(socket, Local::Claim { pid })?;
    let ClientFrame::Start {
        channel,
        job_id,
        seq,
        operation,
    } = serde_json::from_slice(wire.read()?)?
    else {
        return Err(invalid());
    };
    let channel = channel.to_owned();
    let mut tty = OpenOptions::new().write(true).open("/dev/tty")?;
    foreground(pid, unsafe { libc::getpid() })?;
    let socket_fd = wire.reader.get_ref().as_raw_fd();
    if expired(socket_fd)? || CLOSING.load(Ordering::Relaxed) {
        return Err(invalid());
    }
    tty.write_all(b"\n/usr/bin/uptime\n")?;
    let mut session = Session {
        out: wire.reader.get_ref().try_clone()?,
        nonce: String::new(),
        channel: Some(channel.clone()),
        input_seq: 0,
        output_seq: 0,
        last_job: None,
        job: None,
        main_tty: Some(tty),
    };
    session.handle(ClientFrame::Start {
        channel: &channel,
        job_id,
        seq,
        operation,
    })?;
    while session.job.is_some() {
        let job = session.job.as_ref().ok_or_else(invalid)?;
        let mut fds = [
            poll_fd(socket_fd),
            poll_fd(job.stdout.as_ref().map_or(-1, AsRawFd::as_raw_fd)),
            poll_fd(job.stderr.as_ref().map_or(-1, AsRawFd::as_raw_fd)),
        ];
        let buffered = !wire.reader.buffer().is_empty();
        events(&mut fds, buffered)?;
        live(&fds[0])?;
        if buffered || fds[0].revents != 0 {
            let frame = serde_json::from_slice(wire.read()?)?;
            if !matches!(frame, ClientFrame::Stop { .. }) {
                return Err(invalid());
            }
            session.handle(frame)?;
        }
        session.drain(fds[1].revents != 0, fds[2].revents != 0)?;
    }
    // Keep the final frame readable before exposing peer hangup to the broker.
    loop {
        let mut fds = [poll_fd(socket_fd)];
        events(&mut fds, false)?;
        if live(&fds[0]).is_err() {
            return Ok(());
        }
        if fds[0].revents != 0 {
            let frame = serde_json::from_slice(wire.read()?)?;
            if !matches!(frame, ClientFrame::Stop { .. }) {
                return Err(invalid());
            }
            session.handle(frame)?;
        }
    }
}
