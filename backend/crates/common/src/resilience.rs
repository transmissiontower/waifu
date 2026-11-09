use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum BreakerError<E> {
    #[error("circuit open")]
    Open,
    #[error(transparent)]
    Inner(E),
}

#[derive(Debug)]
enum State {
    Closed { failures: u32 },
    Open { since: Instant },
    HalfOpen,
}

/// Classic circuit breaker: opens after `threshold` consecutive failures, allows one probe
/// call after `cooldown`.
pub struct CircuitBreaker {
    name: String,
    threshold: u32,
    cooldown: Duration,
    state: Mutex<State>,
}

impl CircuitBreaker {
    pub fn new(name: &str, threshold: u32, cooldown: Duration) -> Self {
        Self { name: name.into(), threshold, cooldown, state: Mutex::new(State::Closed { failures: 0 }) }
    }

    pub fn is_open(&self) -> bool {
        matches!(&*self.state.lock().unwrap(), State::Open { since } if since.elapsed() < self.cooldown)
    }

    fn allow(&self) -> bool {
        let mut s = self.state.lock().unwrap();
        match &*s {
            State::Closed { .. } | State::HalfOpen => true,
            State::Open { since } => {
                if since.elapsed() >= self.cooldown {
                    *s = State::HalfOpen;
                    true
                } else {
                    false
                }
            }
        }
    }

    pub fn record(&self, ok: bool) {
        let mut s = self.state.lock().unwrap();
        if ok {
            if !matches!(&*s, State::Closed { failures: 0 }) {
                tracing::info!(breaker = %self.name, "circuit closed");
            }
            *s = State::Closed { failures: 0 };
            return;
        }
        let failures = match &*s {
            State::Closed { failures } => failures + 1,
            _ => self.threshold,
        };
        if failures >= self.threshold {
            tracing::warn!(breaker = %self.name, "circuit opened");
            *s = State::Open { since: Instant::now() };
        } else {
            *s = State::Closed { failures };
        }
    }

    pub async fn call<T, E, F, Fut>(&self, f: F) -> Result<T, BreakerError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        if !self.allow() {
            return Err(BreakerError::Open);
        }
        match f().await {
            Ok(v) => {
                self.record(true);
                Ok(v)
            }
            Err(e) => {
                self.record(false);
                Err(BreakerError::Inner(e))
            }
        }
    }
}

/// Retry with exponential backoff. `op` is re-created per attempt.
pub async fn retry<T, E, F, Fut>(attempts: u32, base: Duration, mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut delay = base;
    let mut n = 0;
    loop {
        n += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if n >= attempts => return Err(e),
            Err(e) => {
                tracing::debug!(attempt = n, error = %e, "retrying");
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn breaker_opens_and_recovers() {
        let b = CircuitBreaker::new("t", 2, Duration::from_millis(50));
        for _ in 0..2 {
            let _ = b.call(|| async { Err::<(), _>("boom") }).await;
        }
        assert!(matches!(b.call(|| async { Ok::<_, &str>(1) }).await, Err(BreakerError::Open)));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(b.call(|| async { Ok::<_, &str>(1) }).await.is_ok());
        assert!(!b.is_open());
    }

    #[tokio::test]
    async fn retry_eventually_succeeds() {
        let mut n = 0;
        let r = retry(3, Duration::from_millis(1), || {
            n += 1;
            let ok = n == 3;
            async move {
                if ok {
                    Ok(7)
                } else {
                    Err("no")
                }
            }
        })
        .await;
        assert_eq!(r, Ok(7));
    }
}
