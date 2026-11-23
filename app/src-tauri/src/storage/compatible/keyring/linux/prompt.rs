use std::ffi::CStr;

use gtk::{Dialog, DialogFlags, Entry, InputPurpose, Label, ResponseType, glib::translate::ToGlibPtr, prelude::*};
use tokio::sync::{oneshot, watch};
use zeroize::Zeroizing;

use super::super::{KeyAccessError, encode_payload};

pub(crate) async fn prompt<R: tauri::Runtime>(
    window: tauri::Window<R>,
    description: String,
    mut closed: watch::Receiver<bool>,
) -> Result<Zeroizing<Vec<u8>>, KeyAccessError> {
    let owner = window.clone();
    let (mut answer, result) = oneshot::channel();
    window.run_on_main_thread(move || {
        let Ok(parent) = owner.gtk_window() else { return; };
        gtk::glib::MainContext::default().spawn_local(async move {
            if *closed.borrow() || answer.is_closed() {
                let _ = answer.send(Err(KeyAccessError::Cancelled));
                return;
            }
            let dialog = Dialog::with_buttons(
                Some("S3 호환 키 등록"), Some(&parent),
                DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
                &[("취소", ResponseType::Cancel), ("등록", ResponseType::Accept)],
            );
            dialog.set_default_response(ResponseType::Cancel);
            let access = masked_entry();
            let secret = masked_entry();
            let area = dialog.content_area();
            let details = Label::new(Some(&description));
            details.set_line_wrap(true);
            details.set_max_width_chars(72);
            area.add(&details);
            for (name, entry) in [("_Access Key ID", &access), ("_Secret Access Key", &secret)] {
                let label = Label::with_mnemonic(name);
                label.set_mnemonic_widget(Some(entry));
                area.add(&label);
                area.add(entry);
            }
            dialog.show_all();
            let response = tokio::select! {
                biased;
                _ = closed.wait_for(|value| *value) => ResponseType::Cancel,
                _ = answer.closed() => ResponseType::Cancel,
                response = dialog.run_future() => response,
            };
            let payload = if response == ResponseType::Accept && !*closed.borrow() {
                let access = access.buffer();
                let secret = secret.buffer();
                // GTK가 보관한 문자열은 인코더 호출 동안만 빌린다.
                unsafe {
                    let access = CStr::from_ptr(gtk::ffi::gtk_entry_buffer_get_text(access.to_glib_none().0));
                    let secret = CStr::from_ptr(gtk::ffi::gtk_entry_buffer_get_text(secret.to_glib_none().0));
                    match (access.to_str(), secret.to_str()) {
                        (Ok(access), Ok(secret)) => encode_payload(access, secret),
                        _ => Err(KeyAccessError::Invalid),
                    }
                }
            } else {
                Err(KeyAccessError::Cancelled)
            };
            access.set_text("");
            secret.set_text("");
            dialog.close();
            let _ = answer.send(payload);
        });
    }).map_err(|_| KeyAccessError::Unavailable)?;
    result.await.map_err(|_| KeyAccessError::Unavailable)?
}

fn masked_entry() -> Entry {
    let entry = Entry::new();
    entry.set_visibility(false);
    entry.set_input_purpose(InputPurpose::Password);
    entry.set_input_hints(gtk::InputHints::NO_SPELLCHECK);
    entry
}
