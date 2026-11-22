use std::{env, fs, io, path::Path, process, thread, time::Duration};

fn main() {
    let exe = env::current_exe().unwrap();
    let dir = exe.parent().unwrap();
    let mode = fs::read_to_string(dir.join("mode")).unwrap();
    let args: Vec<_> = env::args().skip(1).collect();
    let action = args[1].as_str();
    assert_eq!(&args[..6], ["ssm", action, "--region", "us-west-2", "--output", "json"]);
    assert_eq!(&args[args.len()-2..], ["--no-cli-pager", "--no-cli-auto-prompt"]);
    assert_eq!(env::var("AWS_SESSION_TOKEN").unwrap(), "fixture-token");
    assert_eq!(env::var("AWS_MAX_ATTEMPTS").unwrap(), "1");
    let record = |name: &str| fs::write(dir.join(name), b"called").unwrap();
    if action == "start-session" {
        assert_eq!(&args[6..10], ["--target", "i-01234567", "--reason", "zzabbaxterm-0123456789abcdef0123456789abcdef"]);
        assert_eq!(args.len(), 12);
        assert_eq!(env::var("AWS_CLI_ERROR_FORMAT").unwrap(), "legacy");
        assert_eq!(env::var("PATH").unwrap(), "fixture-plugin-path");
        record("start");
        match mode.as_str() {
            "reject" => {
                eprintln!("An error occurred (TargetNotConnected) when calling the StartSession operation: offline");
                process::exit(254);
            }
            "unknown" => {
                println!("Starting session with SessionId: not-ownership");
                eprintln!("An error occurred (TargetNotConnected) when calling the StartSession operation: plugin output");
                process::exit(254);
            }
            "connected" => {
                println!("fixture-started");
                let mut line = String::new();
                io::stdin().read_line(&mut line).unwrap();
                assert_eq!(line.trim_end(), "fixture-input");
                record("ended");
                println!("fixture-input-received");
            }
            _ => panic!("unexpected mode"),
        }
        return;
    }
    assert_eq!(action, "describe-sessions");
    let state = args[7].as_str();
    assert_eq!(&args[6..10], ["--state", state, "--filters", "key=Target,value=i-01234567"]);
    record(state);
    let connected = mode == "connected";
    let status = match state {
        "Active" if connected && exists(dir, "start") => Some("Connected"),
        "History" if connected && exists(dir, "ended") => Some("Terminated"),
        "Active" | "History" => None,
        _ => panic!("unexpected state"),
    };
    if let Some(status) = status {
        println!(r#"{{"Sessions":[{{"SessionId":"Sample-0123456789abcdef0","Reason":"zzabbaxterm-0123456789abcdef0123456789abcdef","Owner":"arn:aws:sts::123456789012:assumed-role/Sample/Sample","Target":"i-01234567","Status":"{status}"}}]}}"#);
    } else {
        // 빠르게 끝나는 자식의 연결 확인이 먼저 실패하지 않도록 빈 목록을 반환한다.
        thread::sleep(Duration::from_millis(10));
        println!(r#"{{"Sessions":[]}}"#);
    }
}

fn exists(dir: &Path, name: &str) -> bool {
    dir.join(name).is_file()
}
