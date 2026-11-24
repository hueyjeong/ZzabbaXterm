//! Windows RDP 계정 창이다. 메모리 대화 상자 템플릿으로 앱 프로세스 안에 띄운다.
//! 자격 증명 관리자의 다른 항목을 보이지 않고, 비밀번호는 저장하지 않는다.

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::{
        Controls::EM_LIMITTEXT,
        WindowsAndMessaging::{
            BS_DEFPUSHBUTTON, DS_CENTER, DS_MODALFRAME, DS_SETFONT, DialogBoxIndirectParamW, ES_AUTOHSCROLL,
            ES_PASSWORD, EndDialog, GWLP_USERDATA, GetDlgItem, GetDlgItemTextW, GetWindowLongPtrW, IDCANCEL, IDOK,
            SendDlgItemMessageW, SendMessageW, SetDlgItemTextW, SetWindowLongPtrW, WM_COMMAND, WM_INITDIALOG,
            WM_NEXTDLGCTL, WS_BORDER, WS_CAPTION, WS_CHILD, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
        },
    },
};
use zeroize::Zeroizing;

const LOGIN: i32 = 101;
const PASSWORD: i32 = 102;
const MAX_FIELD: usize = 256;
const SS_NOPREFIX: u32 = 0x80;
const BUTTON: u16 = 0x80;
const EDIT: u16 = 0x81;
const STATIC: u16 = 0x82;

struct State<'a> {
    remembered: Option<&'a str>,
    answer: Option<(String, Zeroizing<String>)>,
}

/// 확인을 누르면 `(계정, 비밀번호)`를 돌려준다. `remembered`는 계정 칸에 미리 채우고 비밀번호 칸에 포커스를 둔다.
pub(super) fn prompt(parent: HWND, target: &str, remembered: Option<&str>) -> Option<(String, Zeroizing<String>)> {
    let template = template(&format!("검증된 서버: {target}"));
    let mut state = State { remembered, answer: None };
    unsafe {
        DialogBoxIndirectParamW(std::ptr::null_mut(), template.as_ptr().cast(), parent, Some(procedure),
            &mut state as *mut State as LPARAM);
    }
    state.answer
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// DLGTEMPLATE과 DLGITEMTEMPLATE은 DWORD 경계에 놓여야 하므로 u32 버퍼로 돌려준다.
fn template(server: &str) -> Vec<u32> {
    let mut words: Vec<u16> = Vec::new();
    let long = |words: &mut Vec<u16>, value: u32| words.extend([value as u16, (value >> 16) as u16]);
    let text = |words: &mut Vec<u16>, value: &str| words.extend(value.encode_utf16().chain([0]));
    let items: [(u16, u32, [i16; 4], i32, &str); 7] = [
        (STATIC, SS_NOPREFIX, [7, 7, 226, 18], -1, server),
        (STATIC, SS_NOPREFIX, [7, 29, 226, 9], -1, "계정 (또는 도메인\\계정)"),
        (EDIT, WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, [7, 40, 226, 14], LOGIN, ""),
        (STATIC, SS_NOPREFIX, [7, 59, 226, 9], -1, "비밀번호"),
        (EDIT, WS_BORDER | WS_TABSTOP | (ES_AUTOHSCROLL | ES_PASSWORD) as u32, [7, 70, 226, 14], PASSWORD, ""),
        (BUTTON, WS_TABSTOP | BS_DEFPUSHBUTTON as u32, [129, 92, 50, 14], IDOK, "로그인"),
        (BUTTON, WS_TABSTOP, [183, 92, 50, 14], IDCANCEL, "취소"),
    ];
    long(&mut words, WS_POPUP | WS_CAPTION | WS_SYSMENU | (DS_MODALFRAME | DS_SETFONT | DS_CENTER) as u32);
    long(&mut words, 0);
    words.extend([items.len() as u16, 0, 0, 240, 113, 0, 0]);
    text(&mut words, "RDP 계정 인증");
    words.push(9);
    text(&mut words, "Segoe UI");
    for (class, style, [x, y, width, height], id, title) in items {
        if words.len() % 2 == 1 { words.push(0); }
        long(&mut words, WS_CHILD | WS_VISIBLE | style);
        long(&mut words, 0);
        words.extend([x as u16, y as u16, width as u16, height as u16, id as u16, 0xFFFF, class]);
        text(&mut words, title);
        words.push(0);
    }
    if words.len() % 2 == 1 { words.push(0); }
    words.chunks(2).map(|pair| u32::from(pair[0]) | (u32::from(pair[1]) << 16)).collect()
}

fn read(dialog: HWND, id: i32) -> Option<Zeroizing<String>> {
    let mut buffer = Zeroizing::new([0_u16; MAX_FIELD + 1]);
    let length = unsafe { GetDlgItemTextW(dialog, id, buffer.as_mut_ptr(), buffer.len() as i32) } as usize;
    // 재할당으로 지워지지 않은 사본이 남지 않게 미리 넉넉히 잡고 한 글자씩 넣는다.
    let mut text = Zeroizing::new(String::with_capacity(MAX_FIELD * 4));
    for character in char::decode_utf16(buffer[..length.min(MAX_FIELD)].iter().copied()) { text.push(character.ok()?); }
    Some(text)
}

unsafe extern "system" fn procedure(dialog: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
    match message {
        WM_INITDIALOG => unsafe {
            SetWindowLongPtrW(dialog, GWLP_USERDATA, lparam);
            for id in [LOGIN, PASSWORD] { SendDlgItemMessageW(dialog, id, EM_LIMITTEXT, MAX_FIELD, 0); }
            let state = &*(lparam as *const State);
            match state.remembered {
                Some(login) => {
                    SetDlgItemTextW(dialog, LOGIN, wide(login).as_ptr());
                    SendMessageW(dialog, WM_NEXTDLGCTL, GetDlgItem(dialog, PASSWORD) as WPARAM, 1);
                    0
                }
                None => 1,
            }
        },
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as i32;
            if id != IDOK && id != IDCANCEL { return 0; }
            unsafe {
                if id == IDOK {
                    let state = &mut *(GetWindowLongPtrW(dialog, GWLP_USERDATA) as *mut State);
                    state.answer = read(dialog, LOGIN).zip(read(dialog, PASSWORD)).map(|(login, password)| (login.to_string(), password));
                }
                SetDlgItemTextW(dialog, PASSWORD, wide("").as_ptr());
                EndDialog(dialog, id as isize);
            }
            1
        }
        _ => 0,
    }
}
