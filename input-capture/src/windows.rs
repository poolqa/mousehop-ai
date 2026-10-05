use async_trait::async_trait;
use core::task::{Context, Poll};
use event_thread::EventThread;
use futures::Stream;
use std::pin::Pin;

use std::task::ready;
use tokio::sync::mpsc::{Receiver, channel};

use input_event::CrossingModifier;

use super::{Capture, CaptureError, CaptureEvent, Position};

mod display_util;
mod event_thread;

pub struct WindowsInputCapture {
    event_rx: Receiver<(Position, CaptureEvent)>,
    event_thread: Option<EventThread>,
    remote: Option<std::sync::Arc<mousehop_ipc::desktop::Link>>,
    remote_task: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait]
impl Capture for WindowsInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        if let Some(remote) = &self.remote {
            remote.capture_create(edge(pos));
        } else if let Some(thread) = &self.event_thread {
            thread.create(pos);
        }
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        if let Some(remote) = &self.remote {
            remote.capture_destroy(edge(pos));
        } else if let Some(thread) = &self.event_thread {
            thread.destroy(pos);
        }
        Ok(())
    }

    async fn release(&mut self, _warp_target: Option<(i32, i32)>) -> Result<(), CaptureError> {
        if let Some(remote) = &self.remote {
            let _ = remote
                .complete(mousehop_ipc::desktop::Request::Release)
                .await;
        } else if let Some(thread) = &self.event_thread {
            if !thread.release_capture().await {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Windows hook did not confirm capture release",
                )
                .into());
            }
        }
        Ok(())
    }

    async fn set_crossing_modifier(
        &mut self,
        pos: Position,
        modifier: Option<CrossingModifier>,
    ) -> Result<(), CaptureError> {
        if let Some(remote) = &self.remote {
            remote.modifier(edge(pos), modifier);
        } else if let Some(thread) = &self.event_thread {
            thread.set_crossing_modifier(pos, modifier);
        }
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Self {
        // Deep buffer: the low-level hook procs feed this and must
        // never block (a blocked WH_MOUSE_LL/WH_KEYBOARD_LL proc
        // freezes input system-wide and the OS then removes the
        // hook). A capacity of 10 filled almost immediately under
        // any consumer stall; 1024 rides out brief stalls so the
        // hooks' try_send path rarely has to drop an event.
        let (event_tx, event_rx) = channel(1024);
        if let Some(remote) = mousehop_ipc::desktop::current() {
            let mut rx = remote.subscribe_capture();
            let generation_link = remote.clone();
            let task = tokio::task::spawn_local(async move {
                while let Some((generation, edge, event)) = rx.recv().await {
                    if generation_link.generation() != generation {
                        continue;
                    }
                    use mousehop_ipc::desktop::Capture as C;
                    let event = match event {
                        C::Begin => CaptureEvent::Begin {
                            cursor: None,
                            normalized_cursor: None,
                        },
                        C::Input(input) => CaptureEvent::Input(input.into_event()),
                        C::AutoRelease => CaptureEvent::AutoRelease,
                        C::Locked(locked) => CaptureEvent::HostInputState(if locked {
                            super::HostInputState::Locked
                        } else {
                            super::HostInputState::Unlocked
                        }),
                    };
                    let Some(pos) = position(edge) else {
                        continue;
                    };
                    if event_tx.send((pos, event)).await.is_err() {
                        break;
                    }
                }
            });
            Self {
                event_thread: None,
                event_rx,
                remote: Some(remote),
                remote_task: Some(task),
            }
        } else {
            Self {
                event_thread: Some(EventThread::new(event_tx)),
                event_rx,
                remote: None,
                remote_task: None,
            }
        }
    }
}

impl Stream for WindowsInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}

fn edge(pos: Position) -> u8 {
    match pos {
        Position::Left => 0,
        Position::Right => 1,
        Position::Top => 2,
        Position::Bottom => 3,
    }
}
fn position(edge: u8) -> Option<Position> {
    match edge {
        0 => Some(Position::Left),
        1 => Some(Position::Right),
        2 => Some(Position::Top),
        3 => Some(Position::Bottom),
        _ => None,
    }
}
impl Drop for WindowsInputCapture {
    fn drop(&mut self) {
        if let Some(task) = self.remote_task.take() {
            task.abort();
        }
    }
}
