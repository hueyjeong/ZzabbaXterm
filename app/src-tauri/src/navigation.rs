//! 앱 자산 밖으로의 최상위 이동을 거부한다. OS 파일 놓기를 끈 창에 파일이 놓여도 문서가 바뀌지 않는다.
//! 앱 출처의 클립보드 읽기는 웹뷰 확인 창 없이 허용한다.

use tauri::{Runtime, Url, plugin::TauriPlugin};

pub(crate) fn app_origin(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost")
            // 개발 서버(`npm run tauri dev`)는 디버그 빌드에서만 허용한다.
            || (cfg!(debug_assertions) && url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))),
        _ => false,
    }
}

pub(crate) fn guard<R: Runtime>() -> TauriPlugin<R> {
    tauri::plugin::Builder::new("navigation-guard").on_navigation(|_, url| app_origin(url)).build()
}

/// 클립보드 읽기를 요청한 문서가 앱 출처일 때만 참이다.
#[cfg(any(windows, test))]
fn grants_clipboard_read(uri: &str) -> bool {
    Url::parse(uri).is_ok_and(|url| app_origin(&url))
}

/// 터미널·AI 입력창·원격 데스크톱 붙여넣기가 쓰는 `navigator.clipboard.readText`의 확인 창을 앱 출처에서만 생략한다.
/// 다른 출처와 다른 권한은 웹뷰 기본 동작을 따른다.
pub(crate) fn clipboard_read<R: Runtime>() -> TauriPlugin<R> {
    tauri::plugin::Builder::new("clipboard-read")
        .on_webview_ready(|webview| {
            #[cfg(windows)]
            if let Err(error) = webview.with_webview(allow_app_clipboard_read) {
                eprintln!("클립보드 권한 처리기 등록 실패: {error}");
            }
            #[cfg(not(windows))]
            let _ = webview;
        })
        .build()
}

#[cfg(windows)]
fn allow_app_clipboard_read(platform: tauri::webview::PlatformWebview) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ, COREWEBVIEW2_PERMISSION_STATE_ALLOW,
    };
    use webview2_com::{PermissionRequestedEventHandler, take_pwstr};

    let handler = PermissionRequestedEventHandler::create(Box::new(|_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut kind = Default::default();
        // SAFETY: 웹뷰가 넘긴 유효한 인자 객체이고, Uri가 돌려준 문자열은 take_pwstr가 해제한다.
        unsafe {
            args.PermissionKind(&mut kind)?;
            if kind != COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ {
                return Ok(());
            }
            let mut uri = Default::default();
            args.Uri(&mut uri)?;
            if grants_clipboard_read(&take_pwstr(uri)) {
                args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
            }
        }
        Ok(())
    }));
    let mut token = 0;
    // SAFETY: 웹뷰 스레드에서 받은 컨트롤러이고, 처리기는 웹뷰가 참조를 잡아 수명을 유지한다.
    let registered = unsafe {
        platform.controller().CoreWebView2().and_then(|core| core.add_PermissionRequested(&handler, &mut token))
    };
    if let Err(error) = registered {
        eprintln!("클립보드 권한 처리기 등록 실패: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_app_assets_and_the_debug_dev_server_are_navigable() {
        for allowed in ["tauri://localhost/", "http://tauri.localhost/index.html", "https://tauri.localhost/"] {
            assert!(app_origin(&Url::parse(allowed).unwrap()), "{allowed}");
        }
        for denied in [
            "file:///C:/Users/me/evil.html", "https://example.com/", "http://tauri.localhost.evil.test/",
            "tauri://evil/", "data:text/html,hi", "about:blank",
        ] {
            assert!(!app_origin(&Url::parse(denied).unwrap()), "{denied}");
        }
        assert_eq!(app_origin(&Url::parse("http://localhost:5173/").unwrap()), cfg!(debug_assertions));
    }

    #[test]
    fn clipboard_read_is_granted_only_to_app_documents() {
        assert!(grants_clipboard_read("tauri://localhost/"));
        assert!(grants_clipboard_read("http://tauri.localhost/index.html"));
        for denied in ["https://example.com/", "http://tauri.localhost.evil.test/", "about:blank", "", "not a url"] {
            assert!(!grants_clipboard_read(denied), "{denied}");
        }
        assert_eq!(grants_clipboard_read("http://localhost:5173/"), cfg!(debug_assertions));
    }
}
