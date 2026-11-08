//! Cooperative control shared by the reader and its worker threads.
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

#[derive(Default)]
pub struct Control {
    cancelled: AtomicBool,
    paused: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
    low_priority: bool,
}

impl Control {
    pub fn new(low_priority: bool) -> Self {
        Self { low_priority, ..Default::default() }
    }
    pub fn cancel(&self) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    pub fn set_paused(&self, pause: bool) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.paused.store(pause, Ordering::Release);
        if !pause {
            self.wake.notify_all();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn checkpoint(&self) -> io::Result<()> {
        if self.low_priority {
            thread_local! {static LOWERED:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
            LOWERED.with(|lowered| {
                if !lowered.replace(true) {
                    // Workers are dedicated to one operation; their threads exit afterwards.
                    unsafe {
                        use windows_sys::Win32::System::Threading::*;
                        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                }
            });
        }
        if self.is_paused() && !self.is_cancelled() {
            let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            while self.is_paused() && !self.is_cancelled() {
                guard = self.wake.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
        }
        if self.is_cancelled() {
            Err(io::Error::new(io::ErrorKind::Interrupted, crate::error::Error::new("operation cancelled")))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn cancellation_wakes_paused_workers() {
        let c = Arc::new(Control::default());
        c.set_paused(true);
        let (tx, rx) = mpsc::channel();
        let worker = c.clone();
        let h = std::thread::spawn(move || tx.send(worker.checkpoint().unwrap_err().kind()).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        c.cancel();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), io::ErrorKind::Interrupted);
        h.join().unwrap();
    }
    #[test]
    fn resume_releases_workers_without_cancelling() {
        let c = Arc::new(Control::default());
        c.set_paused(true);
        let (tx, rx) = mpsc::channel();
        let worker = c.clone();
        let h = std::thread::spawn(move || tx.send(worker.checkpoint().is_ok()).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        c.set_paused(false);
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap());
        h.join().unwrap();
    }
}
