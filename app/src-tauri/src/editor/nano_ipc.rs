use tauri::{
    Manager,
    ipc::{CommandArg, CommandItem, Invoke, InvokeBody, InvokeError},
};

use crate::terminal::Sessions;

use super::{cli_open::MAIN, nano_bridge::NanoBridge};

const PENDING: &str = "editor_remote_nano_pending";
const CLAIM: &str = "editor_remote_nano_claim";
const MOUNTED: &str = "editor_remote_nano_mounted";
const FAILED: &str = "editor_remote_nano_failed";

pub(crate) fn dispatch(invoke: Invoke) -> bool {
    let Invoke { message, resolver, acl } = invoke;
    let (command, arity) = match message.command() {
        PENDING => (PENDING, 0),
        CLAIM => (CLAIM, 1),
        MOUNTED => (MOUNTED, 2),
        FAILED => (FAILED, 1),
        _ => return false,
    };
    let webview = message.webview_ref();
    let label = webview.label();
    if acl.is_none() || label != MAIN || webview.window_ref().label() != MAIN {
        resolver.reject("앱의 로컬 기본 창에서만 원격 nano 편집을 사용할 수 있습니다");
        return true;
    }
    // IPC를 받은 문서의 세대를 동기 경계에서 캡처한다.
    let document = webview.state::<Sessions>().document(label);
    if !matches!(message.payload(), InvokeBody::Json(body) if
        (arity == 0 && body.is_null()) || body.as_object().is_some_and(|fields| fields.len() == arity))
    {
        resolver.reject("원격 nano 요청 형식이 올바르지 않습니다");
        return true;
    }
    let bridge = webview.state::<NanoBridge>();
    if command == PENDING {
        resolver.resolve(bridge.pending(label, document));
        return true;
    }

    let id = |key| -> Result<&str, InvokeError> {
        let value = <&str>::from_command(CommandItem {
            name: command,
            key,
            plugin: None,
            message: &message,
            acl: &acl,
        })?;
        if is_opaque_id(value) {
            Ok(value)
        } else {
            Err(InvokeError::from("원격 nano 식별자가 올바르지 않습니다"))
        }
    };
    let offer_id = match id("offerId") {
        Ok(id) => id,
        Err(error) => {
            resolver.invoke_error(error);
            return true;
        }
    };
    match command {
        CLAIM => resolver.resolve(bridge.claim(label, document, offer_id)),
        MOUNTED => resolver.respond(id("handle").and_then(|handle| {
            if handle != offer_id && bridge.mounted(label, document, offer_id, handle) {
                Ok(())
            } else {
                Err(InvokeError::from("원격 nano 편집 탭의 마운트를 승인할 수 없습니다"))
            }
        })),
        FAILED => {
            // 마운트 응답만 유실된 경우에는 이미 승인된 탭을 유지한다.
            bridge.claim_delivery_failed(label, document, offer_id);
            resolver.resolve(());
        }
        _ => unreachable!(),
    }
    true
}

fn is_opaque_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::is_opaque_id;

    #[test]
    fn opaque_ids_are_exact_lowercase_hex() {
        assert!(is_opaque_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_opaque_id("0123456789abcdef0123456789abcde"));
        assert!(!is_opaque_id("0123456789abcdef0123456789abcdeF"));
        assert!(!is_opaque_id("/123456789abcdef0123456789abcdef"));
    }
}
