//! `sudo -A -- <program> <args…>` 자리를 대신하는 시험 실행기다.
//! 암호 내용은 절대 출력하지 않고 성공 여부만 남긴다.

use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    process::Command,
    thread,
    time::Duration,
};

fn home() -> PathBuf {
    PathBuf::from(env::current_exe().unwrap()).parent().unwrap().to_owned()
}

fn event(line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home().join("events"))
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

fn ask(askpass: &std::ffi::OsStr) -> bool {
    use std::time::Instant;
    let mut child = Command::new(askpass)
        .arg("[sudo] password for demo:")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            return output.status.success() && !output.stdout.is_empty();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mode = fs::read_to_string(home().join("mode")).unwrap_or_default();
    event(&format!("run {}", args.join(" ")));
    event(&format!("askpass={:?}", env::var_os("SUDO_ASKPASS").is_some()));
    event(&format!(
        "endpoint={}",
        env::var("ZXT_ASKPASS_ENDPOINT").unwrap_or_default()
    ));
    event(&format!("ssh_askpass={:?}", env::var_os("SSH_ASKPASS").is_some()));
    if mode == "cached" {
        println!("SUDO-CACHED");
        return;
    }
    if mode == "slow" {
        event("waiting");
        thread::sleep(Duration::from_secs(1));
    }
    if mode == "hold" {
        event("waiting");
        thread::sleep(Duration::from_secs(2));
        return;
    }
    let askpass = env::var_os("SUDO_ASKPASS").expect("SUDO_ASKPASS");
    let first = ask(&askpass);
    event(&format!("ask1={first}"));
    if mode == "replay" {
        let second = ask(&askpass);
        event(&format!("ask2={second}"));
        println!("REPLAY {first} {second}");
    } else {
        println!("SUDO-RAN {first}");
    }
}
