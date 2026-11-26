//! ZXT 서버 측 sudo 승격 helper.
//!
//! 관리자가 고정 경로(`/usr/local/libexec/zxt-elevate`)에 root 소유·비쓰기로 설치한다.
//! 앱은 이 파일을 설치하지 않는다. Linux에서만 동작하며, Windows 빌드에는 실행되지 않는
//! stub만 둔다.

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("zxt-elevate는 Linux에서만 실행됩니다");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::run().into()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        env,
        ffi::OsString,
        fs::{self, OpenOptions},
        io::{self, BufRead, BufReader, Read, Write},
        os::unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            process::ExitStatusExt,
        },
        path::{Path, PathBuf},
        process::{Command, ExitCode, Stdio},
        time::Duration,
    };

    const VERSION: &str = "zxt-elevate 1";
    const SUDO: &str = "/usr/bin/sudo";
    const MKFIFO: &str = "/usr/bin/mkfifo";
    const ID: &str = "/usr/bin/id";
    const FRAME_LIMIT: usize = 65_536;
    const MAX_PASSWORD: usize = 1022;
    const O_NONBLOCK: i32 = 0x800;

    pub(super) fn run() -> ExitCode {
        let args: Vec<OsString> = env::args_os().skip(1).collect();
        let result = match args.first().and_then(|arg| arg.to_str()) {
            Some("--version") => version(),
            Some("--verify") => verify(),
            Some("serve") => serve(&args),
            Some("bridge") => bridge(&args),
            Some(other) if other.starts_with('-') => Err(format!("unknown option: {other}")),
            _ => askpass(),
        };
        match result {
            Ok(code) => code,
            Err(error) => {
                eprintln!("zxt-elevate: {error}");
                ExitCode::from(125)
            }
        }
    }

    fn version() -> Result<ExitCode, String> {
        println!("{VERSION}");
        Ok(ExitCode::SUCCESS)
    }

    fn verify() -> Result<ExitCode, String> {
        let exe = canonical_exe()?;
        let meta = fs::symlink_metadata(&exe).map_err(|error| format!("stat: {error}"))?;
        if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err("helper 소유자·권한이 신뢰 조건에 맞지 않습니다".into());
        }
        let uid = euid()?;
        let user = user_name()?;
        println!("{VERSION}");
        println!("user={user}");
        println!("uid={uid}");
        println!("owner={}", meta.uid());
        println!("mode={:o}", meta.mode() & 0o7777);
        println!("path={}", exe.display());
        Ok(ExitCode::SUCCESS)
    }

    fn serve(args: &[OsString]) -> Result<ExitCode, String> {
        let (id, nonce) = pair(args)?;
        let mut stdin = io::stdin().lock();
        let line = read_line(&mut stdin, "run 프레임")?;
        let frame = parse_run(&line, &id, &nonce)?;
        let directory = Directory::create(&id, &nonce)?;

        let mut command = Command::new(SUDO);
        command.args(["-A", "--"]).arg(&frame.program).args(&frame.args);
        command.env_clear();
        command.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
        for key in ["HOME", "LANG", "LC_ALL", "TZ"] {
            if let Some(value) = env::var_os(key) {
                command.env(key, value);
            }
        }
        let user = user_name()?;
        command.env("USER", &user);
        command.env("LOGNAME", &user);
        command.env("SHELL", "/bin/bash");
        command.env("TERM", "xterm-256color");
        command.env("SUDO_ASKPASS", canonical_exe()?);
        command.env("ZXT_ELEVATE_DIR", directory.path());
        command.env("ZXT_ELEVATE_NONCE", &nonce);
        command.stdin(Stdio::null());
        command.stdout(Stdio::inherit());
        command.stderr(Stdio::inherit());

        let status = command
            .status()
            .map_err(|error| format!("sudo 시작: {error}"))?;
        drop(directory);
        Ok(ExitCode::from(exit_code(&status)))
    }

    fn bridge(args: &[OsString]) -> Result<ExitCode, String> {
        let (id, nonce) = pair(args)?;
        // serve가 run 프레임을 읽고 디렉터리를 만드는 동안 잠시 기다린다.
        let started = std::time::Instant::now();
        let directory = loop {
            match Directory::open(&id, &nonce) {
                Ok(directory) => break directory,
                Err(error) if started.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = error;
                }
                Err(error) => return Err(error),
            }
        };
        let request = read_fifo(directory.request(), "askpass 요청")?;
        let request = parse_askpass(&request, &id, &nonce)?;
        println!("REQUEST 2 {} {}", request.id, request.nonce);
        io::stdout()
            .flush()
            .map_err(|error| format!("stdout: {error}"))?;

        // SSH 채널이 EOF를 늦게 보내도 bridge가 무기한 매달리지 않는다.
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = io::stdin().read_line(&mut line);
            let _ = sender.send(line);
        });
        let line = receiver
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| "password 프레임 시간 초과")?;
        let password = parse_password(&line, &id, &nonce)?;
        let started = std::time::Instant::now();
        let mut response = loop {
            match OpenOptions::new()
                .write(true)
                .custom_flags(O_NONBLOCK)
                .open(directory.response())
            {
                Ok(response) => break response,
                Err(_) if started.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(format!("password FIFO: {error}")),
            }
        };
        response
            .write_all(&password)
            .and_then(|_| response.write_all(b"\n"))
            .and_then(|_| response.flush())
            .map_err(|error| format!("password 쓰기: {error}"))?;
        drop(directory);
        Ok(ExitCode::SUCCESS)
    }

    fn askpass() -> Result<ExitCode, String> {
        let requested = env::var_os("SUDO_ASKPASS").ok_or("SUDO_ASKPASS 없음")?;
        if Path::new(&requested).canonicalize().map_err(|error| error.to_string())?
            != canonical_exe()?
        {
            return Err("SUDO_ASKPASS가 이 helper가 아닙니다".into());
        }
        let directory = env::var_os("ZXT_ELEVATE_DIR").ok_or("승격 디렉터리 없음")?;
        let nonce = env::var("ZXT_ELEVATE_NONCE").map_err(|_| "승격 난수 없음")?;
        if !valid_token(&nonce) {
            return Err("승격 난수가 올바르지 않습니다".into());
        }
        let directory = PathBuf::from(directory);
        let (id, _) = split_directory(&directory, &nonce)?;
        let directory = Directory::open(&id, &nonce)?;
        // bridge가 늦게 떠도 한 번의 askpass가 유실되지 않게 잠시 기다린다.
        let started = std::time::Instant::now();
        let mut request = loop {
            match OpenOptions::new()
                .write(true)
                .custom_flags(O_NONBLOCK)
                .open(directory.request())
            {
                Ok(request) => break request,
                Err(_) if started.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(format!("askpass 요청 채널 없음: {error}")),
            }
        };
        writeln!(request, "ASKPASS 2 {id} {nonce}").map_err(|error| format!("요청 쓰기: {error}"))?;
        request.flush().map_err(|error| format!("요청 flush: {error}"))?;
        // bridge가 응답을 쓰지 않고 죽어도 askpass가 무기한 매달리지 않는다.
        let mut response = BufReader::new(
            OpenOptions::new()
                .read(true)
                .custom_flags(O_NONBLOCK)
                .open(directory.response())
                .map_err(|error| format!("password FIFO 열기: {error}"))?,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut line = String::new();
        loop {
            match response.read_line(&mut line) {
                Ok(0) => {
                    if std::time::Instant::now() >= deadline {
                        return Err("password 응답 시간 초과".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err("password 응답 시간 초과".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(format!("password 읽기: {error}")),
            }
        }
        let password = line.strip_suffix('\n').unwrap_or(&line);
        let password = password.strip_suffix('\r').unwrap_or(password);
        if password.is_empty() || password.len() > MAX_PASSWORD || password.contains('\0') {
            return Err("password 응답이 올바르지 않습니다".into());
        }
        let mut out = io::stdout().lock();
        out.write_all(password.as_bytes())
            .and_then(|_| out.write_all(b"\n"))
            .and_then(|_| out.flush())
            .map_err(|error| format!("stdout: {error}"))?;
        Ok(ExitCode::SUCCESS)
    }

    struct RunFrame {
        program: String,
        args: Vec<String>,
    }

    fn parse_run(line: &str, expected_id: &str, expected_nonce: &str) -> Result<RunFrame, String> {
        let fields: Vec<&str> = line.trim_end_matches(['\r', '\n']).split(' ').collect();
        if fields.len() < 6 || fields[0] != "ZXT-ELEVATE" || fields[1] != "1" {
            return Err("run 프레임 형식이 아닙니다".into());
        }
        if fields[2] != expected_id || fields[3] != expected_nonce {
            return Err("run 프레임의 승인 ID·난수가 다릅니다".into());
        }
        let program = decode_text(fields[4], "실행 파일")?;
        if !program.starts_with('/') || has_control(&program) {
            return Err("실행 파일은 제어 문자 없는 절대 경로여야 합니다".into());
        }
        let count: usize = fields[5]
            .parse()
            .map_err(|_| "인자 개수가 올바르지 않습니다")?;
        if count != fields.len().saturating_sub(6) || count > 64 {
            return Err("인자 개수가 프레임과 다릅니다".into());
        }
        let args = fields[6..]
            .iter()
            .map(|value| decode_text(value, "인자"))
            .collect::<Result<Vec<_>, _>>()?;
        if args.iter().any(|argument| argument.len() > 8192 || has_control(argument)) {
            return Err("인자가 올바르지 않습니다".into());
        }
        Ok(RunFrame { program, args })
    }

    struct AskpassRequest {
        id: String,
        nonce: String,
    }

    fn parse_askpass(line: &str, expected_id: &str, expected_nonce: &str) -> Result<AskpassRequest, String> {
        let fields: Vec<&str> = line.trim_end_matches(['\r', '\n']).split(' ').collect();
        if fields.len() != 4 || fields[0] != "ASKPASS" || fields[1] != "2" {
            return Err("askpass 프레임 형식이 아닙니다".into());
        }
        if fields[2] != expected_id || fields[3] != expected_nonce {
            return Err("askpass 승인 ID·난수가 다릅니다".into());
        }
        Ok(AskpassRequest {
            id: fields[2].to_owned(),
            nonce: fields[3].to_owned(),
        })
    }

    fn parse_password(line: &str, expected_id: &str, expected_nonce: &str) -> Result<Vec<u8>, String> {
        let fields: Vec<&str> = line.trim_end_matches(['\r', '\n']).split(' ').collect();
        if fields.len() != 5 || fields[0] != "PASSWORD" || fields[1] != "2" {
            return Err("password 프레임 형식이 아닙니다".into());
        }
        if fields[2] != expected_id || fields[3] != expected_nonce {
            return Err("password 승인 ID·난수가 다릅니다".into());
        }
        let password = decode(fields[4])?;
        if password.is_empty() || password.len() > MAX_PASSWORD || password.iter().any(|b| *b == 0) {
            return Err("password 값이 올바르지 않습니다".into());
        }
        Ok(password)
    }

    fn pair(args: &[OsString]) -> Result<(String, String), String> {
        if args.len() != 3 {
            return Err("serve|bridge <승인 ID> <난수> 형식이 아닙니다".into());
        }
        let id = args[1].to_str().ok_or("승인 ID 인코딩")?.to_owned();
        let nonce = args[2].to_str().ok_or("난수 인코딩")?.to_owned();
        if !valid_token(&id) || !valid_token(&nonce) {
            return Err("승인 ID·난수는 32자 16진수여야 합니다".into());
        }
        Ok((id, nonce))
    }

    struct Directory {
        path: PathBuf,
    }

    impl Directory {
        fn create(id: &str, nonce: &str) -> Result<Self, String> {
            let base = base_directory()?;
            let path = base.join(format!("{id}-{nonce}"));
            fs::create_dir_all(&base).map_err(|error| format!("승격 디렉터리: {error}"))?;
        cleanup_stale(&base, &base.join(format!("{id}-{nonce}")))?;
            fs::set_permissions(&base, PermissionsExt::from_mode(0o700))
                .map_err(|error| format!("승격 디렉터리 권한: {error}"))?;
            let base_meta = fs::symlink_metadata(&base)
                .map_err(|error| format!("승격 디렉터리 확인: {error}"))?;
            if !base_meta.is_dir() || base_meta.mode() & 0o777 != 0o700 || base_meta.uid() != euid()? {
                return Err("승격 기본 디렉터리 신뢰 조건이 맞지 않습니다".into());
            }
            if fs::symlink_metadata(&path).is_ok() {
                return Err("같은 승격 디렉터리가 이미 있습니다".into());
            }
            fs::create_dir(&path).map_err(|error| format!("승격 디렉터리 생성: {error}"))?;
            fs::set_permissions(&path, PermissionsExt::from_mode(0o700))
                .map_err(|error| format!("승격 디렉터리 권한: {error}"))?;
            let directory = Self { path };
            directory.fifo("req")?;
            directory.fifo("pw")?;
            Ok(directory)
        }

        fn open(id: &str, nonce: &str) -> Result<Self, String> {
            let base = base_directory()?;
            let path = base.join(format!("{id}-{nonce}"));
            let meta = fs::symlink_metadata(&path).map_err(|_| "승격 디렉터리가 없습니다")?;
            if !meta.is_dir() || meta.mode() & 0o777 != 0o700 || meta.uid() != euid()? {
                return Err("승격 디렉터리 신뢰 조건이 맞지 않습니다".into());
            }
            for name in ["req", "pw"] {
                let meta = fs::symlink_metadata(path.join(name))
                    .map_err(|_| "승격 FIFO가 없습니다")?;
                if meta.file_type().is_symlink() || !meta.file_type().is_fifo() {
                    return Err("승격 FIFO 신뢰 조건이 맞지 않습니다".into());
                }
            }
            Ok(Self { path })
        }

        fn fifo(&self, name: &str) -> Result<(), String> {
            let status = Command::new(MKFIFO)
                .arg("-m")
                .arg("600")
                .arg(self.path.join(name))
                .status()
                .map_err(|error| format!("mkfifo: {error}"))?;
            if !status.success() {
                return Err("mkfifo 실패".into());
            }
            Ok(())
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn request(&self) -> PathBuf {
            self.path.join("req")
        }

        fn response(&self) -> PathBuf {
            self.path.join("pw")
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.path.join("req"));
            let _ = fs::remove_file(self.path.join("pw"));
            let _ = fs::remove_dir(&self.path);
        }
    }

    /// 정상 종료하지 못한 예전 FIFO 디렉터리는 6시간 뒤 다음 serve가 정리한다.
    fn cleanup_stale(base: &Path, current: &Path) -> Result<(), String> {
        let uid = euid()?;
        let Ok(entries) = fs::read_dir(base) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == current {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o777 != 0o700 {
                continue;
            }
            if meta
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_none_or(|age| age < Duration::from_secs(6 * 3600))
            {
                continue;
            }
            let _ = fs::remove_file(path.join("req"));
            let _ = fs::remove_file(path.join("pw"));
            let _ = fs::remove_dir(path);
        }
        Ok(())
    }

    fn base_directory() -> Result<PathBuf, String> {
        let uid = euid()?;
        let run_user = PathBuf::from(format!("/run/user/{uid}"));
        if fs::metadata(&run_user).is_ok_and(|meta| meta.is_dir()) {
            return Ok(run_user.join("zxt-elevate"));
        }
        Ok(PathBuf::from(format!("/tmp/zxt-elevate-{uid}")))
    }

    fn split_directory(directory: &Path, nonce: &str) -> Result<(String, String), String> {
        let name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("승격 디렉터리 이름")?;
        let id = name
            .strip_suffix(&format!("-{nonce}"))
            .ok_or("승격 디렉터리 난수")?;
        if !valid_token(id) {
            return Err("승격 디렉터리 승인 ID".into());
        }
        Ok((id.to_owned(), nonce.to_owned()))
    }

    fn canonical_exe() -> Result<PathBuf, String> {
        env::current_exe()
            .and_then(|path| path.canonicalize())
            .map_err(|error| format!("실행 파일 경로: {error}"))
    }

    fn euid() -> Result<u32, String> {
        let status = fs::read_to_string("/proc/self/status").map_err(|error| error.to_string())?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| "euid 확인 실패".into())
    }

    fn user_name() -> Result<String, String> {
        let output = Command::new(ID)
            .arg("-un")
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("id -un: {error}"))?;
        if !output.status.success() {
            return Err("id -un 실패".into());
        }
        let name = String::from_utf8(output.stdout).map_err(|_| "계정 이름 인코딩")?;
        let name = name.trim();
        if name.is_empty() || has_control(name) {
            return Err("계정 이름이 올바르지 않습니다".into());
        }
        Ok(name.to_owned())
    }

    fn read_fifo(path: PathBuf, context: &str) -> Result<String, String> {
        let started = std::time::Instant::now();
        let file = loop {
            match OpenOptions::new()
                .read(true)
                .custom_flags(O_NONBLOCK)
                .open(&path)
            {
                Ok(file) => break file,
                Err(_) if started.elapsed() < Duration::from_secs(10) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(format!("{context} FIFO 열기: {error}")),
            }
        };
        let mut reader = BufReader::new(file);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut line = String::new();
        loop {
            match reader.read_line(&mut line) {
                Ok(0) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(format!("{context} 시간 초과"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(_) => return Ok(line),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(format!("{context} 시간 초과"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(format!("{context} 읽기: {error}")),
            }
        }
    }

    fn read_line(reader: &mut impl BufRead, context: &str) -> Result<String, String> {
        let mut line = String::new();
        let mut limited = reader.take(FRAME_LIMIT as u64 + 1);
        limited
            .read_line(&mut line)
            .map_err(|error| format!("{context} 읽기: {error}"))?;
        if line.is_empty() || line.len() > FRAME_LIMIT {
            return Err(format!("{context}가 비었거나 너무 깁니다"));
        }
        Ok(line)
    }

    fn exit_code(status: &std::process::ExitStatus) -> u8 {
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or_else(|| {
                let signal = status.signal().unwrap_or(0);
                u8::try_from(128 + signal).unwrap_or(255)
            })
    }

    fn valid_token(value: &str) -> bool {
        value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn has_control(value: &str) -> bool {
        value.chars().any(char::is_control)
    }

    fn decode_text(value: &str, context: &str) -> Result<String, String> {
        let bytes = decode(value)?;
        if bytes.contains(&0) {
            return Err(format!("{context}에 NUL이 있습니다"));
        }
        String::from_utf8(bytes).map_err(|_| format!("{context} 인코딩이 올바르지 않습니다"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const ID: &str = "0123456789abcdef0123456789abcdef";
        const NONCE: &str = "fedcba9876543210fedcba9876543210";

        fn encode(value: &str) -> String {
            const ALPHABET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let bytes = value.as_bytes();
            let mut output = String::new();
            for chunk in bytes.chunks(3) {
                let mut buffer = 0u32;
                for (index, byte) in chunk.iter().enumerate() {
                    buffer |= u32::from(*byte) << (16 - index * 8);
                }
                for index in 0..4 {
                    if index <= chunk.len() {
                        output.push(ALPHABET[((buffer >> (18 - index * 6)) & 0x3f) as usize] as char);
                    } else {
                        output.push('=');
                    }
                }
            }
            output
        }

        #[test]
        fn run_frame_requires_id_nonce_absolute_program_and_boundaries() {
            let good = format!(
                "ZXT-ELEVATE 1 {ID} {NONCE} {} 2 {} {}\n",
                encode("/usr/bin/printf"),
                encode("a b"),
                encode("c")
            );
            let frame = parse_run(&good, ID, NONCE).unwrap();
            assert_eq!(frame.program, "/usr/bin/printf");
            assert_eq!(frame.args, ["a b", "c"]);
            for bad in [
                format!("ZXT-ELEVATE 1 {ID} {NONCE} {}\n", encode("/usr/bin/id")),
                format!("ZXT-ELEVATE 1 {ID} {} {} 0\n", "0".repeat(32), encode("/usr/bin/id")),
                format!("ZXT-ELEVATE 1 {ID} {NONCE} {} 1\n", encode("relative/id")),
                format!("ZXT-ELEVATE 1 {ID} {NONCE} {} 2 {}\n", encode("/usr/bin/id"), encode("a")),
                format!("ZXT-ELEVATE 2 {ID} {NONCE} {} 0\n", encode("/usr/bin/id")),
            ] {
                assert!(parse_run(&bad, ID, NONCE).is_err(), "{bad:?}");
            }
        }

        #[test]
        fn askpass_and_password_frames_check_sequence_and_nonce() {
            assert!(parse_askpass(&format!("ASKPASS 2 {ID} {NONCE}\n"), ID, NONCE).is_ok());
            assert!(parse_askpass(&format!("ASKPASS 1 {ID} {NONCE}\n"), ID, NONCE).is_err());
            assert!(parse_askpass(&format!("ASKPASS 2 {ID} {}\n", "0".repeat(32)), ID, NONCE).is_err());
            let password = format!("PASSWORD 2 {ID} {NONCE} {}\n", encode("DemoPass123!"));
            assert_eq!(parse_password(&password, ID, NONCE).unwrap(), b"DemoPass123!");
            for bad in [
                format!("PASSWORD 1 {ID} {NONCE} {}\n", encode("x")),
                format!("PASSWORD 2 {ID} {}\n", "0".repeat(32)),
                format!("PASSWORD 2 {ID} {NONCE} !!!\n"),
                format!("PASSWORD 2 {ID} {NONCE} \n"),
            ] {
                assert!(parse_password(&bad, ID, NONCE).is_err(), "{bad:?}");
            }
        }
    }

    fn decode(value: &str) -> Result<Vec<u8>, String> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut table = [255u8; 256];
        for (index, byte) in ALPHABET.iter().enumerate() {
            table[*byte as usize] = index as u8;
        }
        let mut output = Vec::with_capacity(value.len() * 3 / 4);
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for byte in value.bytes() {
            if byte == b'=' {
                break;
            }
            let digit = table[byte as usize];
            if digit == 255 {
                return Err("base64 형식이 올바르지 않습니다".into());
            }
            buffer = (buffer << 6) | u32::from(digit);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                output.push((buffer >> bits) as u8);
                buffer &= (1 << bits) - 1;
            }
        }
        Ok(output)
    }
}
