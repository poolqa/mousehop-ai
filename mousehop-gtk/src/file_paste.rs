//! Preferences only displays offer metadata; the desktop agent owns native paste.
use crate::{
    i18n::{text, tr},
    window::Window,
};
use adw::prelude::*;
use mousehop_ipc::FilePasteOffer;
#[derive(Clone)]
pub(crate) struct FilePastePanel {
    pub widget: gtk::Box,
    label: gtk::Label,
}
impl FilePastePanel {
    pub fn new(_: &Window) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let title = gtk::Label::new(Some(tr("Remote file paste")));
        title.set_xalign(0.0);
        title.add_css_class("heading");
        widget.append(&title);
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_wrap(true);
        widget.append(&label);
        let panel = Self { widget, label };
        panel.available(None);
        panel
    }
    pub fn available(&self, offer: Option<FilePasteOffer>) {
        self.label.set_text(&match offer {
            Some(copy) => text(
                "Ready from {source}: {count} items ({size}). Use Paste in Finder or Explorer.",
                &[
                    ("source", copy.source),
                    ("count", copy.item_count.to_string()),
                    ("size", gtk::glib::format_size(copy.total_bytes).to_string()),
                ],
            ),
            None => tr("Copy files on another computer, then use Paste in Finder or Explorer.")
                .to_owned(),
        });
    }
}
