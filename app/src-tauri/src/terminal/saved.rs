use std::path::Path;

use portable_pty::CommandBuilder;

use crate::connections::{profile::{MethodRef, Shell}, with_saved_method};

use super::{process::Launch, remote};

pub(super) fn launch(config: &Path, id: &str) -> Result<Launch, String> {
    with_saved_method(config, id, |method| match method {
        MethodRef::Local { shell, start_directory } => local(shell, start_directory).map(Launch::Local),
        MethodRef::Ssm { profile, region, instance_id } => remote::Target::new(profile, region, instance_id).map(Launch::Ssm),
        MethodRef::Ssh { ssh } => crate::ssh::terminal::saved(config, ssh).map(Launch::Ssh),
    })
}

#[cfg(test)]
pub(super) fn command(config: &Path, id: &str) -> Result<CommandBuilder, String> {
    match launch(config, id)? {
        Launch::Local(command) => Ok(command),
        Launch::Ssm(_) | Launch::Ssh(_) => Err("로컬 연결이 아닙니다".into()),
    }
}

fn local(shell: Shell, directory: &Path) -> Result<CommandBuilder, String> {
    super::validate_working_directory(directory)?;

    // portable-pty는 시작 직전에 경로가 사라지면 홈을 선택할 수 있다. 프롬프트 전에 실제 위치를 확인한다.
    let mut command = match shell {
        #[cfg(windows)]
        Shell::PowerShell => {
            let path = std::env::var_os("PATH").ok_or("PowerShell: PATH가 없습니다")?;
            let executable = std::env::split_paths(&path).filter(|path| path.is_absolute())
                .map(|path| path.join("pwsh.exe")).find(|path| path.is_file())
                .ok_or("PowerShell: pwsh.exe를 찾을 수 없습니다")?;
            let mut command = CommandBuilder::new(executable);
            command.args(["-NoLogo", "-NoProfile", "-NoExit", "-Command"]);
            command.arg(format!(r#"
                $directory = $env:ZZABBA_C01_CWD
                Remove-Item Env:ZZABBA_C01_CWD
                try {{
                    if (({}) -and [IO.Directory]::Exists($directory)) {{ return }}
                }} catch {{ [Console]::Error.WriteLine($_.Exception.Message) }}
                [Console]::Error.WriteLine('Could not start in the saved working directory.')
                exit 23
            "#, super::POWERSHELL_CWD_MATCH));
            command
        }
        #[cfg(unix)]
        Shell::Bash => {
            let mut command = CommandBuilder::new("/bin/bash");
            command.env_remove("BASH_ENV");
            command.args(["--noprofile", "--norc", "-c", r#"
                if [[ ! . -ef "$ZZABBA_C01_CWD" ]]; then
                    builtin printf '%s\n' 'Could not start in the saved working directory.' >&2
                    exit 23
                fi
                unset ZZABBA_C01_CWD
                exec /bin/bash --noprofile --norc -i
            "#]);
            command
        }
        _ => return Err("지원하지 않는 셸입니다".into()),
    };
    command.cwd(directory);
    command.env("ZZABBA_C01_CWD", directory);
    command.env("TERM", "xterm-256color");
    Ok(command)
}
