use std::{env, fs::OpenOptions, io::Write, path::PathBuf};

const ITEM: &str = "21eb36c3-234d-439d-9e1f-1d3cc5ba8125";

fn main() {
    let home = PathBuf::from(env::current_exe().unwrap())
        .parent()
        .unwrap()
        .to_owned();
    let args: Vec<_> = env::args().skip(1).collect();
    assert_eq!(args[..2], ["--raw", "--nointeraction"]);
    assert_eq!(env::var("BW_SESSION").unwrap(), "isolated-fixture-session");
    let mode = std::fs::read_to_string(home.join("mode")).unwrap_or_default();
    let mut events = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("events"))
        .unwrap();
    if args[2..] == ["status"] {
        writeln!(events, "status").unwrap();
        match mode.as_str() {
            "locked" => print!("{{\"status\":\"locked\"}}"),
            "slow" => std::thread::sleep(std::time::Duration::from_secs(60)),
            _ => print!("{{\"status\":\"unlocked\"}}"),
        }
    } else {
        assert_eq!(args[2..], ["get", "password", ITEM]);
        writeln!(events, "get").unwrap();
        if mode == "missing" {
            eprint!("Not found.");
            std::process::exit(1);
        }
        print!("only-a-fixture-password");
    }
}
