use crate::nano_wire::{Ready, Request, Response, decode_len, valid_session_id};
use std::env;
use std::ffi::{CStr, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

fn check_peer(stream: &UnixStream) -> io::Result<()> {
    let mut cred = MaybeUninit::<libc::ucred>::uninit();
    let size = size_of::<libc::ucred>() as libc::socklen_t;
    let mut len = size;
    // SAFETY: getsockopt receives a valid buffer; a successful full-sized result initializes it.
    let peer = unsafe {
        checked(libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(), &mut len))?;
        if len != size {
            return Err(invalid());
        }
        cred.assume_init()
    };
    if peer.uid != uid() || peer.pid <= 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
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

fn broker(listener: UnixListener, session_id: String) -> io::Result<()> {
    let mut input = channel_fd(libc::STDIN_FILENO)?;
    let mut output = channel_fd(libc::STDOUT_FILENO)?;
    send(&mut output, &Ready { session_id }.encode()?)?;
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
        ];
        poll(&mut fds, cancel_deadline)?;
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
            if check_peer(&peer).is_err() || peer.set_nonblocking(true).is_err() {
                continue;
            }
            let deadline = Some(Instant::now() + IO);
            let Ok(Request::Open { id: 1, path }) = receive(&mut peer, deadline).and_then(|payload| Request::decode(&payload)) else {
                continue;
            };
            if active.is_some() {
                let _ = send(&mut peer, &Response::Busy { id: 1 }.encode()?);
            } else {
                let id = next_id;
                next_id = next_id.checked_add(1).ok_or_else(invalid)?;
                send(&mut output, &Request::Open { id, path }.encode()?)?;
                active = Some((id, Some(peer)));
            }
        }
    }
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
    broker(listener, id)
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
    let socket = directory(id.to_str().ok_or_else(invalid)?)?.join(SOCKET_NAME);
    let shell = login_shell()?;
    let mut name = vec![b'-'];
    name.extend_from_slice(shell.file_name().ok_or_else(invalid)?.as_bytes());
    let mut command = Command::new(&shell);
    command.arg0(OsString::from_vec(name)).env_remove(SOCKET_ENV);
    if check_socket(&socket).is_ok() {
        command.env(SOCKET_ENV, socket);
    }
    Err(command.exec())
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

fn no_endpoint(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
}

fn nano(args: &[OsString]) -> io::Result<()> {
    let (Some(path), Some(socket)) = (candidate(args), env::var_os(SOCKET_ENV)) else {
        return fallback(args);
    };
    let socket = socket_from_env(&socket)?;
    // 파일 시스템 조회는 릴레이에 연결하기 전에 끝낸다.
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(_) => return fallback(args),
    };
    let request = Request::Open { id: 1, path: path.into_os_string().into_vec() }.encode()?;
    let mut stream = match check_socket(&socket).and_then(|()| UnixStream::connect(&socket)) {
        Ok(stream) => stream,
        Err(error) if no_endpoint(&error) => return fallback(args),
        Err(error) => return Err(error),
    };
    check_peer(&stream)?;
    stream.set_nonblocking(true)?;
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
