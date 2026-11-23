//! bridge 채널 시험용 가짜 helper다. 암호 값은 기록하지 않고 프레임 형식만 확인한다.

use std::{
    env,
    fs,
    io::{self, Read, Write},
    path::PathBuf,
};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let home = PathBuf::from(&args[0]);
    let (id, nonce, mode) = (&args[1], &args[2], &args[3]);
    match mode.as_str() {
        "invalid" => {
            println!("REQUEST 1 {id} {nonce}");
            return;
        }
        "truncated" => {
            print!("REQUEST 2 {id}");
            return;
        }
        "missing" => return,
        _ => (),
    }
    println!("REQUEST 2 {id} {nonce}");
    io::stdout().flush().unwrap();
    let mut line = String::new();
    io::stdin().read_to_string(&mut line).unwrap();
    let ok = line.starts_with(&format!("PASSWORD 2 {id} {nonce} ")) && line.ends_with('\n');
    fs::write(home.join("bridge-events"), format!("{} {ok}\n", line.lines().count())).unwrap();
    if mode == "fail" {
        std::process::exit(1);
    }
}
