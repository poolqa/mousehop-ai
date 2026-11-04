//! A transfer window has its own process and IPC connection, independent of preferences.
use crate::{BuildInfo, GtkError, i18n::tr};
use adw::prelude::*;
use gtk::{gio, glib};
use mousehop_ipc::{
    FilePasteConflict, FilePasteConflictAction, FilePastePhase, FrontendEvent, FrontendRequest,
    FrontendRequestWriter,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
};

pub fn run_transfer(_info: BuildInfo, encoded: &str) -> Result<(), GtkError> {
    let Some(id) = mousehop_ipc::decode_transfer_id(encoded) else {
        log::error!("Invalid transfer identifier");
        return Ok(());
    };
    #[cfg(target_os = "macos")]
    crate::configure_macos_bundle_environment();
    let app = adw::Application::builder()
        .application_id("com.mousehop.Mousehop.Transfer")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    #[cfg(target_os = "macos")]
    app.connect_startup(|_| crate::macos_status_item::configure_accessory());
    app.connect_activate(move |app| {
        let (mut reader, writer) = match mousehop_ipc::connect_once() {
            Ok(connection) => connection,
            Err(error) => {
                log::error!("Transfer UI: {error}");
                app.quit();
                return;
            }
        };
        let writer = Rc::new(RefCell::new(writer));
        let active = Rc::new(Cell::new(true));
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(16);
        content.set_margin_bottom(16);
        content.set_margin_start(24);
        content.set_margin_end(24);
        let label = gtk::Label::new(Some(tr("Receiving files…")));
        label.set_wrap(true);
        let progress = gtk::ProgressBar::new();
        progress.set_show_text(true);
        let cancel = gtk::Button::with_label(tr("Cancel"));
        content.append(&label);
        content.append(&progress);
        content.append(&cancel);
        let window = adw::Window::builder()
            .application(app)
            .title(tr("Copying files"))
            .default_width(420)
            // AdwWindow requests a 200px minimum by default; allow this
            // compact popup to shrink to its content's measured height.
            .height_request(0)
            // Let GTK's content minimum determine height, including wrapped errors.
            .default_height(1)
            .resizable(false)
            .content(&content)
            .build();
        let close_writer = writer.clone();
        let close_active = active.clone();
        window.connect_close_request(move |_| {
            if close_active.get() {
                let _ = close_writer
                    .borrow_mut()
                    .request(FrontendRequest::CancelFilePaste { id });
            }
            glib::Propagation::Proceed
        });
        let weak = window.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(window) = weak.upgrade() {
                window.close();
            }
        });
        let (tx, rx) = async_channel::bounded(16);
        gio::spawn_blocking(move || {
            while let Some(Ok(event)) = reader.next_event() {
                if tx.send_blocking(event).is_err() {
                    return;
                }
            }
        });
        let _ = writer.borrow_mut().request(FrontendRequest::Sync);
        let weak = window.downgrade();
        let pending_dialog = Rc::new(RefCell::new(None::<gtk::MessageDialog>));
        let dialog_on_close = pending_dialog.clone();
        window.connect_close_request(move |_| {
            let dialog = dialog_on_close.borrow_mut().take();
            if let Some(dialog) = dialog {
                dialog.close();
            }
            glib::Propagation::Proceed
        });
        glib::spawn_future_local(async move {
            let mut seen_questions = HashSet::new();
            while let Ok(event) = rx.recv().await {
                let Some(window) = weak.upgrade() else {
                    break;
                };
                match event {
                    FrontendEvent::ProtocolVersion(version)
                        if version != mousehop_ipc::LOCAL_PROTOCOL_VERSION =>
                    {
                        active.set(false);
                        label.set_text("Update Mousehop UI and background together.");
                        cancel.set_label(tr("Close"));
                        break;
                    }
                    FrontendEvent::RuntimeState(mousehop_ipc::RuntimeState::Stopped) => {
                        active.set(false);
                        window.close();
                        break;
                    }
                    FrontendEvent::FilePasteConflict(question)
                        if question.id == id && seen_questions.insert(question.request_id) =>
                    {
                        show_conflict(&window, question, writer.clone(), pending_dialog.clone());
                    }
                    FrontendEvent::FilePasteProgress(update) if update.id == id => {
                        let running = matches!(
                            update.phase,
                            FilePastePhase::Receiving
                                | FilePastePhase::Saving
                                | FilePastePhase::WaitingForConflict
                        );
                        active.set(running);
                        if !running {
                            let dialog = pending_dialog.borrow_mut().take();
                            if let Some(dialog) = dialog {
                                dialog.close();
                            }
                        }
                        let status = tr(match update.phase {
                            FilePastePhase::WaitingForConflict => {
                                "Choose how to handle the existing item."
                            }
                            FilePastePhase::Receiving => "Receiving files…",
                            FilePastePhase::Saving => "Saving files…",
                            FilePastePhase::Complete => "Files pasted successfully.",
                            FilePastePhase::Cancelled => {
                                "File paste cancelled. Copy the files again to retry."
                            }
                            FilePastePhase::Failed => {
                                "File paste failed. Copy the files again to retry."
                            }
                        });
                        label.set_text(&update.message.map_or_else(
                            || status.to_owned(),
                            |message| format!("{status}\n{message}"),
                        ));
                        progress.set_fraction(if update.phase == FilePastePhase::Complete {
                            1.0
                        } else if update.total == 0 {
                            0.0
                        } else {
                            (update.received as f64 / update.total as f64).clamp(0.0, 1.0)
                        });
                        progress.set_text(Some(&format!(
                            "{} / {}",
                            glib::format_size(update.received),
                            glib::format_size(update.total)
                        )));
                        cancel.set_label(tr(if running { "Cancel" } else { "Close" }));
                        if update.phase == FilePastePhase::Complete {
                            window.close();
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let dialog = pending_dialog.borrow_mut().take();
            if let Some(dialog) = dialog {
                dialog.close();
            }
            if let Some(window) = weak.upgrade() {
                if active.replace(false) {
                    label.set_text("Mousehop background disconnected.");
                    cancel.set_label(tr("Close"));
                }
                // Disconnecting the observer does not cancel the transfer.
                let _ = window;
            }
        });
        window.present();
        #[cfg(target_os = "macos")]
        crate::macos_status_item::activate_window();
    });
    let status = app.run_with_args::<&str>(&[]);
    if status == glib::ExitCode::SUCCESS {
        Ok(())
    } else {
        Err(GtkError::NonZeroExitCode(status.into()))
    }
}

type RequestWriter = Rc<RefCell<FrontendRequestWriter>>;
type PendingDialog = Rc<RefCell<Option<gtk::MessageDialog>>>;

fn resolve_conflict(
    writer: &RequestWriter,
    question: &FilePasteConflict,
    action: FilePasteConflictAction,
) {
    if let Err(error) = writer
        .borrow_mut()
        .request(FrontendRequest::ResolveFilePasteConflict {
            id: question.id,
            request_id: question.request_id,
            action,
        })
    {
        log::error!("Resolve file conflict: {error}");
    }
}

fn show_conflict(
    parent: &adw::Window,
    question: FilePasteConflict,
    writer: RequestWriter,
    pending: PendingDialog,
) {
    let body = tr("An item named “{name}” already exists. Replace removes the existing file or folder entirely.")
        .replace("{name}", &question.name);
    let dialog = gtk::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .message_type(gtk::MessageType::Question)
        .text(tr("File already exists"))
        .secondary_text(body.as_str())
        .build();
    dialog.add_button(tr("Cancel"), gtk::ResponseType::Cancel);
    dialog.add_button(tr("Rename"), gtk::ResponseType::Other(2));
    dialog
        .add_button(tr("Replace"), gtk::ResponseType::Other(1))
        .add_css_class("destructive-action");
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let parent = parent.downgrade();
    let next = pending.clone();
    let answered = Cell::new(false);
    dialog.connect_response(move |dialog, response| {
        if answered.replace(true) {
            return;
        }
        next.borrow_mut().take();
        // close() emits another response (DeleteEvent), which would send
        // Cancel before the user's Replace/Rename decision reaches Core.
        dialog.destroy();
        match response {
            gtk::ResponseType::Other(1) => {
                resolve_conflict(&writer, &question, FilePasteConflictAction::Replace)
            }
            gtk::ResponseType::Other(2) => {
                if let Some(parent) = parent.upgrade() {
                    show_rename(&parent, question.clone(), writer.clone(), next.clone());
                } else {
                    resolve_conflict(&writer, &question, FilePasteConflictAction::Cancel);
                }
            }
            _ => resolve_conflict(&writer, &question, FilePasteConflictAction::Cancel),
        }
    });
    *pending.borrow_mut() = Some(dialog.clone());
    dialog.present();
}

fn show_rename(
    parent: &adw::Window,
    question: FilePasteConflict,
    writer: RequestWriter,
    pending: PendingDialog,
) {
    let dialog = gtk::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .message_type(gtk::MessageType::Question)
        .text(tr("Rename incoming item"))
        .secondary_text(tr("Enter a new name to keep both items."))
        .build();
    let entry = gtk::Entry::builder()
        .text(question.suggested_name.as_str())
        .activates_default(true)
        .build();
    dialog.content_area().append(&entry);
    dialog.add_button(tr("Cancel"), gtk::ResponseType::Cancel);
    dialog.add_button(tr("Rename"), gtk::ResponseType::Other(1));
    dialog.set_default_response(gtk::ResponseType::Other(1));
    dialog.set_response_sensitive(
        gtk::ResponseType::Other(1),
        mousehop_ipc::valid_paste_name(entry.text().as_str()),
    );
    let weak = dialog.downgrade();
    entry.connect_changed(move |entry| {
        if let Some(dialog) = weak.upgrade() {
            dialog.set_response_sensitive(
                gtk::ResponseType::Other(1),
                mousehop_ipc::valid_paste_name(entry.text().as_str()),
            );
        }
    });
    let next = pending.clone();
    let response_entry = entry.clone();
    let answered = Cell::new(false);
    dialog.connect_response(move |dialog, response| {
        if answered.replace(true) {
            return;
        }
        let action = if response == gtk::ResponseType::Other(1) {
            FilePasteConflictAction::Rename(response_entry.text().to_string())
        } else {
            FilePasteConflictAction::Cancel
        };
        next.borrow_mut().take();
        dialog.destroy();
        resolve_conflict(&writer, &question, action);
    });
    *pending.borrow_mut() = Some(dialog.clone());
    dialog.present();
    entry.grab_focus();
    entry.select_region(0, -1);
}
