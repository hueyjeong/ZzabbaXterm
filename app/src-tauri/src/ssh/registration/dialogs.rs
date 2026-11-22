use std::path::PathBuf;
use tauri::Window;

use super::Rejection;

pub(super) fn pick(window: &Window) -> Result<Option<PathBuf>, Rejection> {
    #[cfg(target_os = "linux")]
    return on_gtk(window, pick_gtk);
    #[cfg(not(target_os = "linux"))]
    Ok(rfd::FileDialog::new().set_parent(window)
        .set_title("SSH 서버 공개 키 선택").add_filter("OpenSSH 공개 키", &["pub"]).pick_file())
}

pub(super) fn confirm(window: &Window, title: &'static str, description: String) -> Result<bool, Rejection> {
    #[cfg(target_os = "linux")]
    return on_gtk(window, move |parent| confirm_gtk(parent, title, &description));
    #[cfg(not(target_os = "linux"))]
    Ok(rfd::MessageDialog::new().set_parent(window)
        .set_title(title).set_description(description)
        .set_buttons(rfd::MessageButtons::OkCancel).show() == rfd::MessageDialogResult::Ok)
}

#[cfg(target_os = "linux")]
fn on_gtk<T: Send + 'static>(window: &Window, show: impl FnOnce(&gtk::ApplicationWindow) -> T + Send + 'static) -> Result<T, Rejection> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let parent = window.clone();
    window.run_on_main_thread(move || {
        let _ = tx.send(parent.gtk_window().map(|parent| show(&parent)));
    }).map_err(|_| Rejection::StoreUnavailable)?;
    rx.recv().map_err(|_| Rejection::StoreUnavailable)?.map_err(|_| Rejection::StoreUnavailable)
}

#[cfg(target_os = "linux")]
use gtk::prelude::*;

#[cfg(target_os = "linux")]
fn key_dialog(parent: &gtk::ApplicationWindow) -> gtk::FileChooserDialog {
    let dialog = gtk::FileChooserDialog::with_buttons(
        Some("SSH 서버 공개 키 선택"), Some(parent), gtk::FileChooserAction::Open,
        &[("취소", gtk::ResponseType::Cancel), ("선택", gtk::ResponseType::Accept)],
    );
    dialog.set_modal(true);
    dialog.set_destroy_with_parent(true);
    dialog.set_local_only(true);
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("OpenSSH 공개 키"));
    filter.add_pattern("*.[pP][uU][bB]");
    dialog.add_filter(filter);
    dialog
}

#[cfg(target_os = "linux")]
fn confirmation_dialog(parent: &gtk::ApplicationWindow, title: &str, description: &str) -> gtk::MessageDialog {
    let dialog = gtk::MessageDialog::new(
        Some(parent), gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Question, gtk::ButtonsType::OkCancel, description,
    );
    dialog.set_title(title);
    dialog.set_default_response(gtk::ResponseType::Cancel);
    dialog
}

#[cfg(target_os = "linux")]
fn pick_gtk(parent: &gtk::ApplicationWindow) -> Option<PathBuf> {
    let dialog = key_dialog(parent);
    let path = (dialog.run() == gtk::ResponseType::Accept).then(|| dialog.filename()).flatten();
    dialog.close();
    path
}

#[cfg(target_os = "linux")]
fn confirm_gtk(parent: &gtk::ApplicationWindow, title: &str, description: &str) -> bool {
    let dialog = confirmation_dialog(parent, title, description);
    let approved = dialog.run() == gtk::ResponseType::Ok;
    dialog.close();
    approved
}

#[cfg(all(test, target_os = "linux"))]
#[test]
#[ignore = "격리 X11 디스플레이 필요"]
fn native_dialogs_are_modal_transients_of_the_same_window() {
    gtk::init().unwrap();
    let app = gtk::Application::new(None, Default::default());
    app.register(None::<&gtk::gio::Cancellable>).unwrap();
    let parent = gtk::ApplicationWindow::new(&app);
    let picker = key_dialog(&parent);
    let confirm = confirmation_dialog(&parent, super::HOST_CONFIRM_TITLE, "dev.example.invalid:2222");
    let parent: gtk::Window = parent.upcast();
    for dialog in [picker.upcast::<gtk::Window>(), confirm.upcast::<gtk::Window>()] {
        assert_eq!(dialog.transient_for().as_ref(), Some(&parent));
        assert!(dialog.is_modal());
        assert!(dialog.must_destroy_with_parent());
        dialog.close();
    }
}
