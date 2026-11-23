//! 앱 자산 밖으로의 최상위 이동을 거부한다. OS 파일 놓기를 끈 창에 파일이 놓여도 문서가 바뀌지 않는다.

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
}
