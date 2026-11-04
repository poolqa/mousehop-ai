//! SNI tray without a GTK application or hidden preferences window.
use crate::agent::TrayCmd;
use std::sync::mpsc::{self, Receiver, Sender};
struct Item {
    tx: Sender<TrayCmd>,
    state: Option<mousehop_ipc::RuntimeState>,
}
impl ksni::Tray for Item {
    fn id(&self) -> String {
        "mousehop".into()
    }
    fn title(&self) -> String {
        "Mousehop".into()
    }
    fn icon_name(&self) -> String {
        "com.mousehop.Mousehop".into()
    }
    fn activate(&mut self, _: i32, _: i32) {
        let _ = self.tx.send(TrayCmd::TogglePresent);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: crate::agent::state_label(self.state).into(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open Mousehop".into(),
                activate: Box::new(|item: &mut Item| {
                    let _ = item.tx.send(TrayCmd::TogglePresent);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Pause / Resume sharing".into(),
                activate: Box::new(|item: &mut Item| {
                    let _ = item.tx.send(TrayCmd::ToggleSharing);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "View diagnostic logs".into(),
                activate: Box::new(|item: &mut Item| {
                    let _ = item.tx.send(TrayCmd::Logs);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit Mousehop".into(),
                activate: Box::new(|item: &mut Item| {
                    let _ = item.tx.send(TrayCmd::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}
pub(crate) struct Tray {
    rx: Receiver<TrayCmd>,
    handle: ksni::Handle<Item>,
}
impl Tray {
    pub fn new() -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let service = ksni::TrayService::new(Item { tx, state: None });
        let handle = service.handle();
        service.spawn();
        Ok(Self { rx, handle })
    }
    pub fn set_state(&self, state: Option<mousehop_ipc::RuntimeState>) {
        self.handle.update(|item| item.state = state);
    }
    pub fn poll(&self) -> Option<TrayCmd> {
        self.rx.try_recv().ok()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}
