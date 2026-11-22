// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    {
        use std::io::Write;
        use windows_sys::Win32::{
            Foundation::FALSE,
            System::{Console::SetConsoleCtrlHandler, Diagnostics::Debug::OutputDebugStringW},
        };

        // PTY 자식은 앱의 Ctrl+C 무시 속성을 상속한다.
        if unsafe { SetConsoleCtrlHandler(None, FALSE) } == FALSE {
            let error = std::io::Error::last_os_error();
            let message = format!("SetConsoleCtrlHandler(None, FALSE): {error}");
            let _ = writeln!(std::io::stderr(), "{message}");
            // GUI 실행에서는 표준 오류 핸들이 없을 수 있다.
            let wide: Vec<u16> = message.encode_utf16().chain([0]).collect();
            unsafe { OutputDebugStringW(wide.as_ptr()) };
        }
    }

    zzabbaxterm_lib::run()
}
