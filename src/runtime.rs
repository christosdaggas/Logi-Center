//! Background foreground-window observation. No compositor calls on GTK's thread.
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct ContextWatcher {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl ContextWatcher {
    pub fn start() -> (Self, mpsc::Receiver<crate::app_context::Detection>) {
        let (tx, rx) = mpsc::sync_channel(1);
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let worker = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut last = None;
            loop {
                if *worker.0.lock().unwrap_or_else(|e| e.into_inner()) {
                    break;
                }
                let detection = crate::app_context::detect();
                let interval = if detection.app_id.is_some() {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs(5)
                };
                let signature = (
                    detection.app_id.clone(),
                    detection.backend.clone(),
                    detection.detail.clone(),
                );
                // Opening our editor/ring must not replace the profile belonging
                // to the application for which that editor/ring was opened.
                let own_window = detection.app_id.as_deref().is_some_and(is_own_application);
                if !own_window && last.as_ref() != Some(&signature) {
                    match tx.try_send(detection) {
                        Ok(()) => last = Some(signature),
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                        Err(mpsc::TrySendError::Full(_)) => {}
                    }
                }
                let guard = worker.0.lock().unwrap_or_else(|e| e.into_inner());
                if *guard {
                    break;
                }
                drop(worker.1.wait_timeout(guard, interval));
            }
        });
        (
            Self {
                stop,
                thread: Some(thread),
            },
            rx,
        )
    }

    pub fn stop(&mut self) {
        *self.stop.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.stop.1.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ContextWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn is_own_application(id: &str) -> bool {
    let id = id.trim_end_matches(".desktop");
    id.eq_ignore_ascii_case(crate::APPLICATION_ID) || id.eq_ignore_ascii_case("logicenter")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn our_editor_and_ring_do_not_reset_the_target_app_profile() {
        assert!(is_own_application("com.chrisdaggas.logicenter.desktop"));
        assert!(is_own_application("LogiCenter"));
        assert!(!is_own_application("org.gnome.Terminal.desktop"));
        assert!(!is_own_application("logicenter-other"));
    }
}
