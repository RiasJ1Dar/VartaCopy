//! Cooperative pause/cancel shared by CLI and GUI workers.
use crate::{Error, Result};
use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
pub struct Control {
    cancelled: AtomicBool,
    paused: Mutex<bool>,
    wake: Condvar,
}
impl Control {
    pub fn cancel(&self) {
        // The same mutex orders cancellation with Condvar::wait, preventing a
        // notification from being lost between the condition check and sleep.
        match self.paused.lock() {
            Ok(_guard) => {
                self.cancelled.store(true, Ordering::Relaxed);
                self.wake.notify_all();
            }
            Err(error) => {
                self.cancelled.store(true, Ordering::Relaxed);
                self.wake.notify_all();
                eprintln!("vartacopy: pause state poisoned during cancellation: {error}");
            }
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    pub fn set_paused(&self, paused: bool) -> Result<()> {
        *self
            .paused
            .lock()
            .map_err(|_| Error::Journal("pause state poisoned".into()))? = paused;
        self.wake.notify_all();
        Ok(())
    }
    pub fn checkpoint(&self) -> Result<()> {
        let mut paused = self
            .paused
            .lock()
            .map_err(|_| Error::Journal("pause state poisoned".into()))?;
        while *paused && !self.is_cancelled() {
            paused = self
                .wake
                .wait(paused)
                .map_err(|_| Error::Journal("pause state poisoned".into()))?;
        }
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}
