//! Cooperative cancellation. Only the executable installs signal handlers.
use anyhow::Result;
use std::{
    future::Future,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Default)]
pub struct StopToken(Arc<(Mutex<bool>, Condvar)>);
impl StopToken {
    /// Returns true only for the first interrupt, and wakes retry waiters.
    pub fn interrupt(&self) -> bool {
        let mut stopped = self.0.0.lock().unwrap_or_else(|p| p.into_inner());
        let first = !*stopped;
        *stopped = true;
        self.0.1.notify_all();
        first
    }
    pub fn is_interrupted(&self) -> bool {
        *self.0.0.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub fn check(&self) -> Result<()> {
        anyhow::ensure!(!self.is_interrupted(), "Interrupted");
        Ok(())
    }
    /// True when the full delay elapsed without interruption.
    pub fn wait(&self, delay: Duration) -> bool {
        let stopped = self.0.0.lock().unwrap_or_else(|p| p.into_inner());
        let (stopped, _) = self
            .0
            .1
            .wait_timeout_while(stopped, delay, |stopped| !*stopped)
            .unwrap_or_else(|p| p.into_inner());
        !*stopped
    }
    /// Drop an in-flight network future promptly, including stalled body reads.
    pub(crate) async fn run<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        let mut future = std::pin::pin!(future);
        loop {
            self.check()?;
            if let Ok(result) =
                tokio::time::timeout(Duration::from_millis(80), future.as_mut()).await
            {
                return result;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupt_is_idempotent_and_wakes_a_long_retry_wait() {
        let token = StopToken::default();
        let waiting = token.clone();
        let start = std::time::Instant::now();
        let worker = std::thread::spawn(move || waiting.wait(Duration::from_secs(60)));
        assert!(token.interrupt());
        assert!(!token.interrupt());
        assert!(!worker.join().unwrap());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(token.check().is_err());
    }
}
