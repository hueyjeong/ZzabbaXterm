use crate::nano_wire::{ADMITTED, ADMIT_NANO, CONFIRM_LEN, Lease, NO_PROOF, PROOF_LEN, REGISTER, Ready, Request, Response, STARTUP_LIMIT, decode_len, startup_frame, valid_session_id};
use std::env;
use std::ffi::{CStr, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const SOCKET_ENV: &str = "ZZABBA_EDITOR_SOCKET";
const ORIGINAL_NANO: &str = "/usr/bin/nano";
const DIRECTORY_PREFIX: &str = "zzabba-editor-";
const SOCKET_NAME: &str = "session.sock";
const IO: Duration = Duration::from_secs(2);
const CANCEL: Duration = Duration::from_secs(5);
static STOPPING: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: libc::c_int) {
    STOPPING.store(true, Ordering::Relaxed);
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid remote editor session or message")
}

fn uid() -> libc::uid_t {
    // SAFETY: The identity query has no preconditions.
    unsafe { libc::geteuid() }
}

fn checked(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 { Err(io::Error::last_os_error()) } else { Ok(result) }
}

fn directory(id: &str) -> io::Result<PathBuf> {
    if !valid_session_id(id) {
        return Err(invalid());
    }
    let tmp = Path::new("/tmp");
    let info = fs::symlink_metadata(tmp)?;
    if !info.is_dir() || info.uid() != 0 || info.mode() & 0o1000 == 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(tmp.join(format!("{DIRECTORY_PREFIX}{id}")))
}

fn check_directory(path: &Path) -> io::Result<()> {
    let info = fs::symlink_metadata(path)?;
    if !info.is_dir() || info.uid() != uid() || info.mode() & 0o7777 != 0o700 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

fn check_socket(path: &Path) -> io::Result<()> {
    check_directory(path.parent().ok_or_else(invalid)?)?;
    let info = fs::symlink_metadata(path)?;
    if !info.file_type().is_socket() || info.uid() != uid() || info.mode() & 0o7777 != 0o600 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

fn socket_from_env(raw: &OsStr) -> io::Result<PathBuf> {
    let path = Path::new(raw);
    let id = path.parent().and_then(Path::file_name).and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix(DIRECTORY_PREFIX)).ok_or_else(invalid)?;
    let expected = directory(id)?.join(SOCKET_NAME);
    if raw != expected.as_os_str() {
        return Err(invalid());
    }
    Ok(expected)
}

struct Peer {
    cred: libc::ucred,
    handle: OwnedFd,
}

impl Peer {
    fn from_socket(stream: &UnixStream) -> io::Result<Self> {
        let mut cred = MaybeUninit::<libc::ucred>::uninit();
        let size = size_of::<libc::ucred>() as libc::socklen_t;
        let mut len = size;
        // SAFETY: Both calls use exact-sized buffers; a returned pidfd is owned here.
        unsafe {
            checked(libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED,
                cred.as_mut_ptr().cast(), &mut len))?;
            if len != size { return Err(invalid()); }
            let cred = cred.assume_init();
            if cred.uid != uid() || cred.pid <= 0 { return Err(io::ErrorKind::PermissionDenied.into()); }
            let mut fd: RawFd = -1;
            let mut len = size_of::<RawFd>() as libc::socklen_t;
            // Linux 6.5+: 소켓의 피어에서 직접 얻어 숫자 PID 재사용 경합을 피한다.
            const SO_PEERPIDFD: libc::c_int = 77;
            checked(libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, SO_PEERPIDFD,
                (&mut fd as *mut RawFd).cast(), &mut len))?;
            if fd < 0 { return Err(invalid()); }
            let handle = OwnedFd::from_raw_fd(fd);
            if len as usize != size_of::<RawFd>() { return Err(invalid()); }
            let peer = Self { cred, handle };
            if !peer.alive() { return Err(io::ErrorKind::NotConnected.into()); }
            Ok(peer)
        }
    }

    fn alive(&self) -> bool {
        let mut fd = pollfd(self.handle.as_raw_fd(), libc::POLLIN);
        // SAFETY: The descriptor and the poll buffer remain live for this call.
        unsafe { libc::poll(&mut fd, 1, 0) == 0 }
    }

    fn session(&self) -> io::Result<libc::pid_t> {
        if !self.alive() { return Err(io::ErrorKind::NotConnected.into()); }
        // SAFETY: getsid only queries the PID; the surrounding polls reject reuse.
        let sid = checked(unsafe { libc::getsid(self.cred.pid) })?;
        if sid <= 0 || !self.alive() { return Err(io::ErrorKind::NotConnected.into()); }
        Ok(sid)
    }
}

struct Login {
    shell: Peer,
    sid: libc::pid_t,
}

impl Login {
    fn admits(&self, peer: &Peer) -> bool {
        self.shell.cred.uid == peer.cred.uid
            && peer.session().ok() == Some(self.sid)
            && self.shell.session().ok() == Some(self.sid)
    }
}

struct SessionDir(PathBuf);

impl Drop for SessionDir {
    fn drop(&mut self) {
        if check_directory(&self.0).is_ok() {
            let _ = fs::remove_file(self.0.join(SOCKET_NAME));
            let _ = fs::remove_dir(&self.0);
        }
    }
}

fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd { fd, events, revents: 0 }
}

fn poll(fds: &mut [libc::pollfd], deadline: Option<Instant>) -> io::Result<()> {
    loop {
        if STOPPING.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let millis = match deadline {
            Some(end) => end.checked_duration_since(Instant::now())
                .ok_or(io::ErrorKind::TimedOut)?.as_millis().clamp(1, 100) as i32,
            None => 100,
        };
        // SAFETY: poll only accesses the entries in the supplied slice.
        match checked(unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, millis) }) {
            Ok(n) if n > 0 => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn read_exact(stream: &mut (impl Read + AsRawFd), mut bytes: &mut [u8], deadline: Option<Instant>) -> io::Result<()> {
    while !bytes.is_empty() {
        poll(&mut [pollfd(stream.as_raw_fd(), libc::POLLIN)], deadline)?;
        match stream.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => bytes = &mut bytes[n..],
            Err(error) if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn receive(stream: &mut (impl Read + AsRawFd), first_deadline: Option<Instant>) -> io::Result<Vec<u8>> {
    let mut header = [0; 4];
    read_exact(stream, &mut header[..1], first_deadline)?;
    let deadline = Some(Instant::now() + IO);
    read_exact(stream, &mut header[1..], deadline)?;
    let mut payload = vec![0; decode_len(header)?];
    read_exact(stream, &mut payload, deadline)?;
    Ok(payload)
}

fn send(stream: &mut (impl Write + AsRawFd), mut bytes: &[u8]) -> io::Result<()> {
    let deadline = Some(Instant::now() + IO);
    while !bytes.is_empty() {
        poll(&mut [pollfd(stream.as_raw_fd(), libc::POLLOUT)], deadline)?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(error) if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn channel_fd(fd: RawFd) -> io::Result<File> {
    // SAFETY: The new descriptor is exclusively owned by File; flags apply to this SSH channel.
    unsafe {
        let duplicate = checked(libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3))?;
        let file = File::from_raw_fd(duplicate);
        let flags = checked(libc::fcntl(duplicate, libc::F_GETFL))?;
        checked(libc::fcntl(duplicate, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        Ok(file)
    }
}

fn register(listener: &UnixListener, input: &mut (impl Read + AsRawFd), output: &mut (impl Write + AsRawFd)) -> io::Result<Login> {
    let deadline = Some(Instant::now() + STARTUP_LIMIT);
    let mut fds = [pollfd(input.as_raw_fd(), libc::POLLIN), pollfd(listener.as_raw_fd(), libc::POLLIN)];
    poll(&mut fds, deadline)?;
    if fds[0].revents != 0 { return Err(io::ErrorKind::BrokenPipe.into()); }
    let (mut stream, _) = listener.accept()?;
    stream.set_nonblocking(true)?;
    let shell = Peer::from_socket(&stream)?;
    let sid = shell.session()?;
    expect_marker(&mut stream, REGISTER, deadline)?;
    let lease = Lease::issue()?;
    let proof = Zeroizing::new(lease.proof(shell.cred.pid, sid));
    // 난수 원문은 보조 SSH에만 보내고 등록 소켓에는 그 피어의 응답만 보낸다.
    send(output, &lease.challenge(shell.cred.pid, sid)[..])?;
    send(&mut stream, &proof[..])?;
    let mut reply = Zeroizing::new([0; 4 + CONFIRM_LEN]);
    read_exact(input, &mut reply[..], deadline)?;
    if decode_len(reply[..4].try_into().unwrap())? != CONFIRM_LEN
        || !lease.confirmed(&reply[4..]) || shell.session()? != sid
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    // 같은 PID가 로그인 셸로 exec되므로 소켓을 닫은 뒤에도 그 프로세스가 임대를 소유한다.
    send(&mut stream, &ADMITTED)?;
    Ok(Login { shell, sid })
}

fn relay(listener: UnixListener, mut input: impl Read + AsRawFd, mut output: impl Write + AsRawFd, session_id: String) -> io::Result<()> {
    send(&mut output, &Ready { session_id }.encode()?)?;
    let login = register(&listener, &mut input, &mut output)?;
    let mut active: Option<(u64, Option<UnixStream>)> = None;
    let mut cancel_deadline = None;
    let mut next_id = 1u64;
    loop {
        let peer = active.as_ref().and_then(|(_, peer)| peer.as_ref());
        let mut fds = [
            pollfd(input.as_raw_fd(), libc::POLLIN),
            pollfd(listener.as_raw_fd(), libc::POLLIN),
            pollfd(peer.map_or(-1, AsRawFd::as_raw_fd), libc::POLLIN),
            pollfd(output.as_raw_fd(), 0),
            pollfd(login.shell.handle.as_raw_fd(), libc::POLLIN),
        ];
        poll(&mut fds, cancel_deadline)?;
        if fds[4].revents != 0 {
            return Err(io::ErrorKind::NotConnected.into());
        }
        if fds[0].revents != 0 {
            let response = Response::decode(&receive(&mut input, Some(Instant::now() + IO))?)?;
            let (id, peer) = active.take().ok_or_else(invalid)?;
            if response.id() != id {
                return Err(invalid());
            }
            cancel_deadline = None;
            if let Some(mut peer) = peer {
                let local = match response {
                    Response::Closed { .. } => Response::Closed { id: 1 },
                    Response::Unattached { .. } => Response::Unattached { id: 1 },
                    Response::Failed { .. } => Response::Failed { id: 1 },
                    Response::Busy { .. } => Response::Busy { id: 1 },
                };
                let _ = send(&mut peer, &local.encode()?);
            }
        } else if fds[2].revents != 0 {
            let (id, peer) = active.as_mut().ok_or_else(invalid)?;
            peer.take();
            send(&mut output, &Request::Cancel { id: *id }.encode()?)?;
            cancel_deadline = Some(Instant::now() + CANCEL);
        }
        if fds[3].revents != 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if fds[1].revents != 0 {
            let Ok((mut peer, _)) = listener.accept() else { continue };
            let Ok(identity) = admit_nano(&login, &mut peer) else { continue };
            let deadline = Some(Instant::now() + IO);
            let Ok(Request::Open { id: 1, path }) = receive(&mut peer, deadline).and_then(|payload| Request::decode(&payload)) else {
                continue;
            };
            if !login.admits(&identity) {
                let _ = send(&mut peer, &Response::Unattached { id: 1 }.encode()?);
                continue;
            }
            if active.is_some() {
                let _ = send(&mut peer, &Response::Busy { id: 1 }.encode()?);
            } else {
                let id = next_id;
                next_id = next_id.checked_add(1).ok_or_else(invalid)?;
                let mut channels = [pollfd(input.as_raw_fd(), libc::POLLIN), pollfd(output.as_raw_fd(), 0)];
                // SAFETY: poll reads and writes only the live two-entry array.
                checked(unsafe { libc::poll(channels.as_mut_ptr(), channels.len() as _, 0) })?;
                if channels.iter().any(|fd| fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0) {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                send(&mut output, &Request::Open { id, path }.encode()?)?;
                active = Some((id, Some(peer)));
            }
        }
    }
}

fn admit_nano(login: &Login, stream: &mut UnixStream) -> io::Result<Peer> {
    let peer = Peer::from_socket(stream)?;
    if !login.admits(&peer) { return Err(io::ErrorKind::PermissionDenied.into()); }
    stream.set_nonblocking(true)?;
    expect_marker(stream, ADMIT_NANO, Some(Instant::now() + IO))?;
    if !login.admits(&peer) { return Err(io::ErrorKind::PermissionDenied.into()); }
    send(stream, &ADMITTED)?;
    Ok(peer)
}

fn bridge() -> io::Result<()> {
    for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
        // SAFETY: The handler only updates a lock-free atomic and never unwinds.
        unsafe {
            if libc::signal(signal, stop as *const () as libc::sighandler_t) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
        }
    }
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let id = format!("{:032x}", u128::from_be_bytes(random));
    let directory = directory(&id)?;
    // 이 모드는 단일 스레드이며 종료까지 자식 프로세스를 만들지 않는다.
    unsafe { libc::umask(0o077) };
    fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let session = SessionDir(directory);
    check_directory(&session.0)?;
    let path = session.0.join(SOCKET_NAME);
    unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(&path)?;
    check_socket(&path)?;
    listener.set_nonblocking(true)?;
    relay(listener, channel_fd(libc::STDIN_FILENO)?, channel_fd(libc::STDOUT_FILENO)?, id)
}

fn login_shell() -> io::Result<PathBuf> {
    let mut entry = MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut bytes = [0u8; 16 * 1024];
    // SAFETY: getpwuid_r fills entry and points its strings into bytes on success.
    let shell = unsafe {
        let status = libc::getpwuid_r(uid(), entry.as_mut_ptr(), bytes.as_mut_ptr().cast(), bytes.len(), &mut result);
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if result.is_null() || (*result).pw_shell.is_null() {
            return Err(invalid());
        }
        CStr::from_ptr((*result).pw_shell).to_bytes()
    };
    let shell = if shell.is_empty() { b"/bin/sh" } else { shell };
    let path = PathBuf::from(OsStr::from_bytes(shell));
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(invalid());
    }
    Ok(path)
}

fn shell(id: &OsStr) -> io::Result<()> {
    let socket = id.to_str().and_then(|id| directory(id).ok()).map(|path| path.join(SOCKET_NAME));
    let shell = login_shell()?;
    let mut name = vec![b'-'];
    name.extend_from_slice(shell.file_name().ok_or_else(invalid)?.as_bytes());
    let mut command = Command::new(&shell);
    command.arg0(OsString::from_vec(name)).env_remove(SOCKET_ENV);
    if let Some(socket) = socket && register_shell(&socket).is_ok() {
        command.env(SOCKET_ENV, socket);
    }
    Err(command.exec())
}

fn register_shell(socket: &Path) -> io::Result<()> {
    // SAFETY: These queries only check whether stdout is this login's controlling PTY.
    let sid = unsafe { libc::getsid(0) };
    if sid <= 0 || unsafe { libc::tcgetsid(libc::STDOUT_FILENO) } != sid {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let mut stream = match connect(socket) {
        Ok(stream) => stream,
        Err(error) => {
            write_startup(NO_PROOF)?;
            return Err(error);
        }
    };
    let deadline = Some(Instant::now() + STARTUP_LIMIT);
    let mut proof = Zeroizing::new([0; PROOF_LEN]);
    if let Err(error) = send(&mut stream, &REGISTER).and_then(|()| read_exact(&mut stream, &mut proof[..], deadline)) {
        write_startup(NO_PROOF)?;
        return Err(error);
    }
    write_startup(*proof)?;
    expect_marker(&mut stream, ADMITTED, deadline)
}

fn write_startup(proof: [u8; PROOF_LEN]) -> io::Result<()> {
    let frame = Zeroizing::new(startup_frame(proof));
    // SAFETY: stdout stays open through spawn; fcntl only changes its status flags.
    let fd = libc::STDOUT_FILENO;
    let flags = checked(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    let mut output = channel_fd(fd)?;
    let written = send(&mut output, &frame[..]);
    let restored = checked(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) });
    written.and(restored.map(|_| ()))
}

fn candidate(args: &[OsString]) -> Option<&Path> {
    let arg = match args {
        [arg] if !matches!(arg.as_bytes().first().copied(), None | Some(b'-' | b'+')) => arg,
        [separator, arg] if separator == "--" && !arg.is_empty() => arg,
        _ => return None,
    };
    Some(Path::new(arg))
}

fn fallback(args: &[OsString]) -> io::Result<()> {
    Err(Command::new(ORIGINAL_NANO).args(args).env_remove(SOCKET_ENV).exec())
}

fn connect(socket: &Path) -> io::Result<UnixStream> {
    check_socket(socket)?;
    let stream = UnixStream::connect(socket)?;
    Peer::from_socket(&stream)?;
    stream.set_nonblocking(true)?;
    Ok(stream)
}

fn expect_marker(stream: &mut UnixStream, expected: [u8; 2], deadline: Option<Instant>) -> io::Result<()> {
    let mut marker = [0; 2];
    read_exact(stream, &mut marker, deadline)?;
    if marker != expected { return Err(invalid()); }
    Ok(())
}

fn nano(args: &[OsString]) -> io::Result<()> {
    let (Some(path), Some(socket)) = (candidate(args), env::var_os(SOCKET_ENV)) else {
        return fallback(args);
    };
    let Ok(socket) = socket_from_env(&socket) else { return fallback(args); };
    // 파일 시스템 조회는 릴레이에 연결하기 전에 끝낸다.
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(_) => return fallback(args),
    };
    let Ok(request) = (Request::Open { id: 1, path: path.into_os_string().into_vec() }).encode() else { return fallback(args); };
    let Ok(mut stream) = connect(&socket) else { return fallback(args); };
    if send(&mut stream, &ADMIT_NANO).and_then(|()| expect_marker(&mut stream, ADMITTED, Some(Instant::now() + IO))).is_err() {
        return fallback(args);
    }
    let response = send(&mut stream, &request)
        .and_then(|()| receive(&mut stream, None))
        .and_then(|payload| Response::decode(&payload))
        .map_err(|error| io::Error::new(error.kind(), format!("remote editor outcome unknown: {error}")))?;
    if response.id() != 1 {
        return Err(invalid());
    }
    match response {
        Response::Closed { .. } => Ok(()),
        Response::Unattached { .. } => fallback(args),
        Response::Busy { .. } => Err(io::Error::other("remote editor is busy")),
        Response::Failed { .. } => Err(io::Error::other("remote editor did not confirm the requested tab closed")),
    }
}

pub(super) fn run() -> io::Result<()> {
    // SAFETY: The identity queries have no preconditions.
    if unsafe { libc::getuid() != uid() || libc::getgid() != libc::getegid() } {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let mut args = env::args_os().skip(1);
    match args.next().as_deref().and_then(OsStr::to_str) {
        Some("editor-bridge") if args.next().is_none() => bridge(),
        Some("shell") => {
            let id = args.next().ok_or_else(invalid)?;
            if args.next().is_some() {
                return Err(invalid());
            }
            shell(&id)
        }
        Some("nano") => nano(&args.collect::<Vec<_>>()),
        _ => Err(io::ErrorKind::InvalidInput.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "0123456789abcdef0123456789abcdef";

    fn relay_fixture() -> (tempfile::TempDir, PathBuf, UnixStream, std::thread::JoinHandle<io::Result<()>>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        server.set_nonblocking(true).unwrap();
        let output = server.try_clone().unwrap();
        let worker = std::thread::spawn(move || relay(listener, server, output, SID.into()));
        let ready = Ready::decode(&receive(&mut client, Some(Instant::now() + IO)).unwrap()).unwrap();
        assert_eq!(ready.session_id, SID);
        (dir, path, client, worker)
    }

    fn separate_login(path: &Path, mode: &str) -> std::process::Child {
        let mut child = Command::new(env::current_exe().unwrap());
        child.args(["--exact", "nano_node::tests::separate_login_fixture"])
            .env("ZZABBA_NANO_LOGIN_FIXTURE", path)
            .env("ZZABBA_NANO_LOGIN_MODE", mode)
            .env("ZZABBA_NANO_PARENT_SID", unsafe { libc::getsid(0) }.to_string());
        // SAFETY: Only the async-signal-safe setsid syscall runs between fork and exec.
        unsafe { child.pre_exec(|| checked(libc::setsid()).map(|_| ())); }
        child.spawn().unwrap()
    }

    fn wait_child(mut child: std::process::Child) {
        let deadline = Instant::now() + CANCEL;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("별도 로그인 테스트가 끝나지 않았습니다");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn registration(path: &Path, client: &mut UnixStream) -> (UnixStream, Lease) {
        let mut shell = UnixStream::connect(path).unwrap();
        shell.set_nonblocking(true).unwrap();
        send(&mut shell, &REGISTER).unwrap();
        let deadline = Some(Instant::now() + IO);
        let secret = Zeroizing::new(receive(client, deadline).unwrap());
        let (lease, pid, sid) = Lease::decode_challenge(&secret).unwrap();
        assert_eq!(pid as u32, std::process::id());
        assert_eq!(sid, unsafe { libc::getsid(0) });
        let mut proof = Zeroizing::new([0; PROOF_LEN]);
        read_exact(&mut shell, &mut proof[..], deadline).unwrap();
        assert!(*proof == lease.proof(pid, sid));
        (shell, lease)
    }

    #[test]
    fn wrong_confirmation_never_grants_the_first_socket() {
        let (_dir, path, mut client, worker) = relay_fixture();
        let (mut shell, _lease) = registration(&path, &mut client);
        send(&mut client, &Lease::issue().unwrap().confirm()[..]).unwrap();
        let deadline = Some(Instant::now() + IO);
        assert!(expect_marker(&mut shell, ADMITTED, deadline).is_err());
        assert_eq!(receive(&mut client, deadline).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(worker.join().unwrap().unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn relay_admits_only_the_registered_login_and_forwards_one_open() {
        let (_dir, path, mut client, worker) = relay_fixture();
        let (mut shell, lease) = registration(&path, &mut client);
        send(&mut client, &lease.confirm()[..]).unwrap();
        expect_marker(&mut shell, ADMITTED, Some(Instant::now() + IO)).unwrap();
        drop(shell);

        wait_child(separate_login(&path, "admission"));
        let mut probe = [0];
        assert_eq!(client.read(&mut probe).unwrap_err().kind(), io::ErrorKind::WouldBlock);

        let mut nano = UnixStream::connect(path).unwrap();
        nano.set_nonblocking(true).unwrap();
        send(&mut nano, &ADMIT_NANO).unwrap();
        let deadline = Some(Instant::now() + IO);
        expect_marker(&mut nano, ADMITTED, deadline).unwrap();
        let open = Request::Open { id: 1, path: b"/test".to_vec() };
        send(&mut nano, &open.encode().unwrap()).unwrap();
        assert_eq!(Request::decode(&receive(&mut client, deadline).unwrap()).unwrap(), open);
        send(&mut client, &Response::Closed { id: 1 }.encode().unwrap()).unwrap();
        assert_eq!(Response::decode(&receive(&mut nano, deadline).unwrap()).unwrap(), Response::Closed { id: 1 });
        drop(client);
        assert_eq!(worker.join().unwrap().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn registered_process_exit_revokes_the_relay() {
        let (_dir, path, mut client, worker) = relay_fixture();
        let child = separate_login(&path, "registration");
        let deadline = Some(Instant::now() + IO);
        let challenge = Zeroizing::new(receive(&mut client, deadline).unwrap());
        let (lease, pid, _) = Lease::decode_challenge(&challenge).unwrap();
        assert_eq!(pid as u32, child.id());
        send(&mut client, &lease.confirm()[..]).unwrap();
        wait_child(child);
        assert_eq!(receive(&mut client, deadline).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(worker.join().unwrap().unwrap_err().kind(), io::ErrorKind::NotConnected);
    }

    #[test]
    fn separate_login_fixture() {
        let Some(path) = env::var_os("ZZABBA_NANO_LOGIN_FIXTURE") else { return; };
        let parent_sid: libc::pid_t = env::var("ZZABBA_NANO_PARENT_SID").unwrap().parse().unwrap();
        assert_ne!(parent_sid, unsafe { libc::getsid(0) });
        let mut stream = UnixStream::connect(path).unwrap();
        stream.set_nonblocking(true).unwrap();
        if env::var("ZZABBA_NANO_LOGIN_MODE").unwrap() == "registration" {
            let deadline = Some(Instant::now() + IO);
            send(&mut stream, &REGISTER).unwrap();
            let mut proof = Zeroizing::new([0; PROOF_LEN]);
            read_exact(&mut stream, &mut proof[..], deadline).unwrap();
            expect_marker(&mut stream, ADMITTED, deadline).unwrap();
            return;
        }
        let result = send(&mut stream, &ADMIT_NANO)
            .and_then(|()| expect_marker(&mut stream, ADMITTED, Some(Instant::now() + IO)));
        assert!(matches!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe));
    }

    #[test]
    fn only_one_plain_operand_or_explicit_separator_is_admitted() {
        for args in [vec![], vec!["-w", "file"], vec!["+10"], vec!["a", "b"], vec![""]] {
            let args: Vec<_> = args.into_iter().map(OsString::from).collect();
            assert_eq!(candidate(&args), None);
        }
        let raw = OsString::from_vec(b"-\xff".to_vec());
        let args = [OsString::from("--"), raw.clone()];
        assert_eq!(candidate(&args), Some(Path::new(&raw)));
    }
}
