// One drawable acquisition per renderer, off the AppKit/JavaScript thread.
// The worker owns no scene: a completed drawable always receives the newest
// scene on the UI thread. Requests and results are bounded to one each.
struct DrawableAcquisition<T: Send + 'static> {
    requests: std::sync::mpsc::SyncSender<Box<dyn FnOnce() -> T + Send>>,
    results: std::sync::mpsc::Receiver<T>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pending: bool,
}

impl<T: Send + 'static> DrawableAcquisition<T> {
    fn new() -> std::io::Result<Self> {
        let (requests, jobs) = std::sync::mpsc::sync_channel::<Box<dyn FnOnce() -> T + Send>>(1);
        let (completed, results) = std::sync::mpsc::sync_channel(1);
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_ready = ready.clone();
        std::thread::Builder::new().name("metal-drawable".into()).spawn(move || {
            while let Ok(acquire) = jobs.recv() {
                let result = acquire();
                worker_ready.store(true, std::sync::atomic::Ordering::Release);
                if completed.send(result).is_err() { break; }
            }
        })?;
        Ok(Self { requests, results, ready, pending: false })
    }

    fn request(&mut self, acquire: impl FnOnce() -> T + Send + 'static) {
        if !self.pending && self.requests.try_send(Box::new(acquire)).is_ok() {
            self.pending = true;
        }
    }

    fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Acquire)
    }

    fn take(&mut self, budget: std::time::Duration) -> Option<T> {
        if !self.pending { return None; }
        let result = self.results.recv_timeout(budget).ok()?;
        self.ready.store(false, std::sync::atomic::Ordering::Release);
        self.pending = false;
        Some(result)
    }
}

// No join on drop: a driver waiting for a drawable must never hold up window
// close. Both channels close; once acquisition returns, the worker exits.

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::{Arc, atomic::{AtomicUsize, Ordering}, mpsc}, time::Duration};

    #[test]
    fn a_blocked_acquisition_keeps_the_caller_free_and_requests_bounded() {
        let mut slot = DrawableAcquisition::new().unwrap();
        let (release, wait) = mpsc::channel();
        let (started, running) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        slot.request(move || { worker_calls.fetch_add(1, Ordering::SeqCst); started.send(()).unwrap(); wait.recv().unwrap(); 42 });
        running.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..100 {
            let unexpected = calls.clone();
            slot.request(move || { unexpected.fetch_add(100, Ordering::SeqCst); 0 });
            assert_eq!(slot.take(Duration::ZERO), None);
            assert!(!slot.is_ready());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        assert_eq!(slot.take(Duration::from_secs(1)), Some(42));
        assert!(!slot.is_ready());
        slot.request(|| 43);
        assert_eq!(slot.take(Duration::from_secs(1)), Some(43));
    }

    #[test]
    fn closing_does_not_wait_for_a_blocked_driver() {
        let mut slot = DrawableAcquisition::new().unwrap();
        let (release, wait) = mpsc::channel();
        let (started, running) = mpsc::channel();
        slot.request(move || { started.send(()).unwrap(); wait.recv().unwrap() });
        running.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(slot);
        release.send(()).unwrap();
    }
}
