//! Desktop liveness must continue while native menus track on the UI thread.
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

pub(crate) struct Heartbeat {
    stop: mpsc::Sender<()>,
    failed: Arc<AtomicBool>,
}
impl Heartbeat {
    pub fn start(send: impl FnMut() -> io::Result<()> + Send + 'static) -> io::Result<Self> {
        Self::with_interval(send, Duration::from_secs(1))
    }
    fn with_interval(
        mut send: impl FnMut() -> io::Result<()> + Send + 'static,
        interval: Duration,
    ) -> io::Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let status = failed.clone();
        thread::Builder::new()
            .name("mousehop-agent-heartbeat".into())
            .spawn(move || {
                while matches!(
                    receiver.recv_timeout(interval),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    if let Err(error) = send() {
                        log::warn!("Desktop Agent heartbeat stopped: {error}");
                        status.store(true, Ordering::Release);
                        break;
                    }
                }
            })?;
        Ok(Self { stop, failed })
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heartbeat_continues_while_the_ui_thread_waits() {
        let (tx, receiver) = mpsc::channel();
        let heartbeat = Heartbeat::with_interval(
            move || {
                tx.send(()).unwrap();
                Ok(())
            },
            Duration::from_millis(10),
        )
        .unwrap();
        // Simulate a native menu owning the calling thread. No UI polling is
        // performed while the background thread must deliver multiple beats.
        for _ in 0..3 {
            receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert!(!heartbeat.failed());
        drop(heartbeat);
        while receiver.recv_timeout(Duration::from_secs(2)).is_ok() {}
    }
}
