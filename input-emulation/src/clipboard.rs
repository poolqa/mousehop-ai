use arboard::{Clipboard, ImageData};
use input_event::{ClipboardEvent, ClipboardFileLease};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::task::spawn_blocking;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("Failed to access clipboard: {0}")]
    Access(String),
    #[error("Failed to set clipboard: {0}")]
    Set(String),
}

/// Clipboard emulation that sets clipboard content
#[derive(Clone)]
pub struct ClipboardEmulation {
    // Use Arc<Mutex<>> to share clipboard across threads
    clipboard: Arc<Mutex<Option<Clipboard>>>,
    owned_file_root: Arc<Mutex<Option<ClipboardFileLease>>>,
}

impl ClipboardEmulation {
    pub fn new() -> Result<Self, ClipboardError> {
        // Try to create initial clipboard instance
        let clipboard = match Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("Failed to create clipboard instance: {e}");
                None
            }
        };

        Ok(Self {
            clipboard: Arc::new(Mutex::new(clipboard)),
            owned_file_root: Arc::new(Mutex::new(None)),
        })
    }

    /// Set clipboard content from a clipboard event
    pub async fn set(&self, event: ClipboardEvent) -> Result<(), ClipboardError> {
        let owned_file_root = self.owned_file_root.clone();
        match event {
            ClipboardEvent::Text(text) => {
                let clipboard_arc = self.clipboard.clone();

                spawn_blocking(move || {
                    let mut clipboard_guard = clipboard_arc.lock().unwrap();

                    // Try to get or create clipboard
                    let clipboard = match clipboard_guard.as_mut() {
                        Some(c) => c,
                        None => {
                            // Try to create a new clipboard instance
                            match Clipboard::new() {
                                Ok(c) => {
                                    *clipboard_guard = Some(c);
                                    clipboard_guard.as_mut().unwrap()
                                }
                                Err(e) => {
                                    return Err(ClipboardError::Access(format!("{e}")));
                                }
                            }
                        }
                    };

                    // Set clipboard text
                    clipboard
                        .set_text(text.clone())
                        .map_err(|e| ClipboardError::Set(format!("{e}")))?;

                    replace_owned_root(&owned_file_root, None);

                    log::debug!("Clipboard set, length: {} bytes", text.len());
                    Ok(())
                })
                .await
                .map_err(|e| ClipboardError::Access(format!("Task join error: {e}")))?
            }
            ClipboardEvent::ImageRgba {
                width,
                height,
                bytes,
            } => {
                let clipboard_arc = self.clipboard.clone();
                spawn_blocking(move || {
                    validate_rgba_len(width, height, bytes.len())?;
                    let mut clipboard_guard = clipboard_arc.lock().unwrap();
                    let clipboard = match clipboard_guard.as_mut() {
                        Some(clipboard) => clipboard,
                        None => {
                            *clipboard_guard = Some(
                                Clipboard::new()
                                    .map_err(|e| ClipboardError::Access(format!("{e}")))?,
                            );
                            clipboard_guard.as_mut().expect("clipboard was inserted")
                        }
                    };
                    clipboard
                        .set_image(ImageData {
                            width: width as usize,
                            height: height as usize,
                            bytes: Cow::Owned(bytes),
                        })
                        .map_err(|e| ClipboardError::Set(format!("{e}")))?;
                    replace_owned_root(&owned_file_root, None);
                    log::debug!("Clipboard image set: {width}x{height}");
                    Ok(())
                })
                .await
                .map_err(|e| ClipboardError::Access(format!("Task join error: {e}")))?
            }
            ClipboardEvent::Files { paths, owned_root } => {
                let clipboard_arc = self.clipboard.clone();
                spawn_blocking(move || {
                    if paths.is_empty() {
                        return Err(ClipboardError::Set("file list is empty".into()));
                    }
                    let mut clipboard_guard = clipboard_arc.lock().unwrap();
                    let clipboard = match clipboard_guard.as_mut() {
                        Some(clipboard) => clipboard,
                        None => {
                            *clipboard_guard = Some(
                                Clipboard::new()
                                    .map_err(|e| ClipboardError::Access(format!("{e}")))?,
                            );
                            clipboard_guard.as_mut().expect("clipboard was inserted")
                        }
                    };
                    clipboard
                        .set()
                        .file_list(&paths)
                        .map_err(|e| ClipboardError::Set(format!("{e}")))?;
                    replace_owned_root(&owned_file_root, owned_root);
                    log::debug!("Clipboard file list set: {} item(s)", paths.len());
                    Ok(())
                })
                .await
                .map_err(|e| ClipboardError::Access(format!("Task join error: {e}")))?
            }
        }
    }

    /// Get current clipboard content (for testing/verification)
    pub async fn get(&self) -> Result<String, ClipboardError> {
        let clipboard_arc = self.clipboard.clone();

        spawn_blocking(move || {
            let mut clipboard_guard = clipboard_arc.lock().unwrap();

            let clipboard = match clipboard_guard.as_mut() {
                Some(c) => c,
                None => match Clipboard::new() {
                    Ok(c) => {
                        *clipboard_guard = Some(c);
                        clipboard_guard.as_mut().unwrap()
                    }
                    Err(e) => {
                        return Err(ClipboardError::Access(format!("{e}")));
                    }
                },
            };

            clipboard
                .get_text()
                .map_err(|e| ClipboardError::Access(format!("{e}")))
        })
        .await
        .map_err(|e| ClipboardError::Access(format!("Task join error: {e}")))?
    }
}

fn replace_owned_root(slot: &Mutex<Option<ClipboardFileLease>>, next: Option<ClipboardFileLease>) {
    *slot.lock().unwrap() = next;
}

fn validate_rgba_len(width: u32, height: u32, actual: usize) -> Result<(), ClipboardError> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| ClipboardError::Set("image dimensions overflow".into()))?;
    if actual != expected {
        return Err(ClipboardError::Set(format!(
            "invalid RGBA image length: got {actual}, expected {expected}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_rgba_buffer_length() {
        assert!(validate_rgba_len(2, 3, 24).is_ok());
        assert!(matches!(
            validate_rgba_len(2, 3, 23),
            Err(ClipboardError::Set(_))
        ));
    }
}
