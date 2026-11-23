// 시험 전용 가짜 aws CLI다. `cli` 모드는 실제 aws CLI처럼 플러그인 역할의 자식을 띄우고 종료를 기다린다.
// 인자: <모드> <수신 포트> <세션 ID> [출력할 포트]
use std::{env, io::{Read, Write}, net::TcpListener, process::{Command, exit}, thread, time::Duration};

fn main() {
    let args: Vec<String> = env::args().collect();
    let (mode, port, session) = (args[1].as_str(), args[2].as_str(), args[3].as_str());
    let printed = args.get(4).map_or(port, String::as_str);
    match mode {
        "cli" => {
            let status = Command::new(&args[0]).args(["plugin", port, session, printed]).status().unwrap();
            exit(status.code().unwrap_or(1));
        }
        "plugin" => plugin(port, session, printed),
        "reject" => {
            eprintln!("\nAn error occurred (AccessDeniedException) when calling the StartSession operation: User: \
                arn:aws:sts::123456789012:assumed-role/Sample/Sample is not authorized to perform: ssm:StartSession \
                on resource: arn:aws:ssm:ap-northeast-1::document/AWS-StartPortForwardingSession");
            exit(254);
        }
        "bind" => {
            println!("\nStarting session with SessionId: {session}");
            println!("Cannot perform start session: listen tcp 127.0.0.1:{port}: bind: address already in use");
            exit(1);
        }
        "garbage" => {
            println!("\nStarting session with SessionId: {session}");
            println!("Unexpected banner from another program");
            thread::sleep(Duration::from_secs(60));
        }
        _ => exit(2),
    }
}

fn plugin(port: &str, session: &str, printed: &str) {
    // 일부 Windows 환경은 bind에서 일시적인 WSAEFAULT를 돌려준다. 포트 사용 중만 실패로 본다.
    let mut attempts = 0;
    let listener = loop {
        match TcpListener::bind(format!("127.0.0.1:{port}")) {
            Ok(listener) => break listener,
            Err(error) if error.kind() != std::io::ErrorKind::AddrInUse && attempts < 200 => {
                attempts += 1;
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    let mut out = std::io::stdout();
    write!(out, "\r\nStarting session with SessionId: {session}\r\nPort {printed} opened for sessionId {session}.\r\nWaiting for connections...\r\n\r\n").unwrap();
    out.flush().unwrap();
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        println!("\nConnection accepted for session [{session}]");
        thread::spawn(move || {
            let mut buffer = [0; 64 * 1024];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => if stream.write_all(&buffer[..read]).is_err() { break },
                }
            }
        });
    }
}
