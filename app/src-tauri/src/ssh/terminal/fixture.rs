use std::{env, ffi::OsStr, fs, io, path::Path};

fn main() {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    assert_eq!(args.len(), 4);
    assert_eq!(args[0], "-F");
    assert_eq!(args[2], "--");
    assert_eq!(args[3], "zzabba-target");
    let config = Path::new(&args[1]);
    assert!(config.is_absolute());
    assert_eq!(config.file_name(), Some(OsStr::new("ssh_config")));
    assert!(config.parent().unwrap().file_name().unwrap().to_str().unwrap().contains(' '));
    assert_eq!(env::var_os("SSH_ASKPASS"), None);
    assert_eq!(env::var_os("SSH_ASKPASS_REQUIRE"), Some("never".into()));
    assert_eq!(env::var_os("BW_SESSION"), None);
    assert_eq!(env::var_os("TERM"), Some("xterm-256color".into()));
    let check = || {
        let text = fs::read_to_string(config).unwrap();
        for line in [
            "HostName dev.example.invalid",
            "Port 2222",
            "User approved-user",
            "StrictHostKeyChecking yes",
            "PreferredAuthentications publickey",
            "PasswordAuthentication no",
        ] {
            assert!(text.lines().any(|actual| actual == line), "{line}");
        }
        assert!(fs::read_to_string(config.with_file_name("known_hosts")).unwrap()
            .starts_with("[dev.example.invalid]:2222 ssh-ed25519 "));
        assert!(config.with_file_name("agent.pub").is_file());
    };
    check();
    println!("F01-READY");
    let mut line = String::new();
    io::stdin().read_line(&mut line).unwrap();
    check();
    assert_eq!(line.trim_end(), "fixture-input");
    println!("F01-INPUT-OK");
    if env::var_os("ZZABBA_F01_FIXTURE_WAIT").is_some() {
        loop { std::thread::park(); }
    }
    std::process::exit(7);
}
