use std::{sync::Arc, time::Duration};

use reqwest::{Client, RequestBuilder, Response};
use tokio::{
    sync::Mutex,
    time::{sleep, Instant},
};

pub struct RateLimitedHttpClient {
    client: Client,
    min_wait: Duration,
    retry_limit: usize,
    next_allowed: Arc<Mutex<Instant>>,
}

impl RateLimitedHttpClient {
    pub fn new(min_wait: Duration) -> Self {
        Self {
            client: Client::new(),
            min_wait,
            retry_limit: 3,
            next_allowed: Arc::new(Mutex::new(Instant::now())),
        }
    }

    pub fn raw(&self) -> &Client {
        &self.client
    }

    pub async fn send(
        &self,
        request: RequestBuilder,
    ) -> Result<Response, Box<dyn std::error::Error>> {
        let request = request.build()?;
        let mut attempt = 0usize;

        loop {
            self.wait_turn().await;
            let cloned = request
                .try_clone()
                .ok_or("failed to clone request for retry")?;
            let response = self.client.execute(cloned).await?;

            if should_retry(response.status()) && attempt < self.retry_limit {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_retry_after_seconds)
                    .unwrap_or_else(|| backoff_for_attempt(attempt));
                sleep(retry_after).await;
                attempt += 1;
                continue;
            }

            return Ok(response);
        }
    }

    async fn wait_turn(&self) {
        let mut next_allowed = self.next_allowed.lock().await;
        let now = Instant::now();
        if *next_allowed > now {
            sleep(*next_allowed - now).await;
        }
        *next_allowed = Instant::now() + self.min_wait;
    }
}

fn should_retry(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::BAD_GATEWAY
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        || status == reqwest::StatusCode::GATEWAY_TIMEOUT
}

fn parse_retry_after_seconds(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

fn backoff_for_attempt(attempt: usize) -> Duration {
    let seconds = match attempt {
        0 => 2,
        1 => 5,
        _ => 10,
    };
    Duration::from_secs(seconds)
}
