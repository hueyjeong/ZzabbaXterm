use std::{env, io::{self, Read, Write}, net::TcpStream, path::Path, process};

fn main() {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    assert_eq!(args.len(), 8);
    assert_eq!(&args[..4], &["-T", "-oBatchMode=yes", "-s", "-F"]);
    assert_eq!(&args[5..], &["--", "zzabba-target", "sftp"]);
    assert!(!io::IsTerminal::is_terminal(&io::stdin()));
    assert!(!io::IsTerminal::is_terminal(&io::stdout()));
    assert_eq!(env::var_os("SSH_ASKPASS"), None);
    assert_eq!(env::var_os("SSH_ASKPASS_REQUIRE"), Some("never".into()));
    assert_eq!(env::var_os("BW_SESSION"), None);
    let config = Path::new(&args[4]);
    let text = std::fs::read_to_string(config).unwrap();
    for line in ["HostName editor.example.invalid", "Port 22", "User editor", "StrictHostKeyChecking yes", "ProxyCommand none", "ControlPath none"] {
        assert!(text.lines().any(|actual| actual == line), "{line}");
    }
    assert!(std::fs::read_to_string(config.with_file_name("known_hosts")).unwrap()
        .starts_with("[editor.example.invalid]:22 ssh-ed25519 "));
    if env::var_os("ZZABBA_FIXTURE_DENY").is_some() {
        eprintln!("fixture: subsystem denied");
        process::exit(42);
    }

    io::stderr().write_all(&[b'!'; 128 * 1024]).unwrap();
    let mut stream = TcpStream::connect(env::var("ZZABBA_FIXTURE_ADDR").unwrap()).unwrap();
    let mut outgoing = stream.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut outgoing);
        let _ = outgoing.shutdown(std::net::Shutdown::Write);
    });
    let mut stdout = io::stdout().lock();
    let mut buffer = [0; 16 * 1024];
    while let Ok(count @ 1..) = stream.read(&mut buffer) {
        if stdout.write_all(&buffer[..count]).and_then(|()| stdout.flush()).is_err() {
            break;
        }
    }
}
