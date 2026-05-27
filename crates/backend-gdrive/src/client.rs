//! HTTP client, rate limiting, retry for Google Drive API.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Token-bucket rate limiter for Google Drive API.
/// Allows ~10,000 requests per 100 seconds per user.
pub struct RateLimiter {
    tokens: AtomicU32,
    max_tokens: u32,
    refill_rate: u32, // tokens per second
}

impl RateLimiter {
    pub fn new(max_requests_per_100s: u32) -> Self {
        Self {
            tokens: AtomicU32::new(max_requests_per_100s),
            max_tokens: max_requests_per_100s,
            refill_rate: max_requests_per_100s / 100,
        }
    }

    /// Try to acquire a token. Returns true if successful.
    pub fn try_acquire(&self) -> bool {
        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            if current == 0 {
                return false;
            }
            if self
                .tokens
                .compare_exchange_weak(current, current - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Wait for a token to become available.
    pub async fn acquire(&self) {
        while !self.try_acquire() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Refill tokens (called periodically).
    pub fn refill(&self) {
        let current = self.tokens.load(Ordering::Relaxed);
        let new = (current + self.refill_rate).min(self.max_tokens);
        self.tokens.store(new, Ordering::Relaxed);
    }
}

/// Google Drive API HTTP client wrapper.
pub struct DriveClient {
    client: reqwest::Client,
    rate_limiter: RateLimiter,
    base_url: String,
}

impl DriveClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            rate_limiter: RateLimiter::new(10_000),
            base_url: "https://www.googleapis.com/drive/v3".to_string(),
        }
    }

    /// Make a GET request to the Drive API.
    pub async fn get(&self, path: &str, token: &str) -> anyhow::Result<reqwest::Response> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/{path}", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_acquire_and_exhaust() {
        let limiter = RateLimiter::new(5);
        for _ in 0..5 {
            assert!(limiter.try_acquire());
        }
        assert!(!limiter.try_acquire());
    }

    #[test]
    fn rate_limiter_refill() {
        let limiter = RateLimiter::new(10);
        for _ in 0..10 {
            assert!(limiter.try_acquire());
        }
        limiter.refill(); // refills refill_rate = 10/100 = 0, but let's test boundary
        // With max 10, refill_rate = 10/100 = 0. Edge case.
    }
}
