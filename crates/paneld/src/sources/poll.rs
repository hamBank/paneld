//! Helpers shared by the polling sources (`aircraft`, `http_poll`,
//! `prometheus`): one bounded GET, `Retry-After`, exponential backoff, a
//! redirect policy that never carries credentials to another host, and the
//! generic poll loop.
//!
//! Errors from here never contain a URL: API keys often live in query
//! strings, and reqwest embeds the URL in its errors (`without_url`).

use std::future::Future;
use std::time::Duration;

use reqwest::header::{HeaderValue, RETRY_AFTER};
use reqwest::StatusCode;

use super::SourceCtx;

#[cfg(test)]
pub mod testutil;

/// Largest response body any poll source accepts.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Delay after the `failures`-th consecutive error: the interval doubled per
/// failure, up to `cap` (never below the interval itself).
pub fn backoff_delay(interval: Duration, failures: u32, cap: Duration) -> Duration {
    let scaled = interval.saturating_mul(1u32 << failures.min(16));
    scaled.min(cap).max(interval)
}

/// How a 429's `Retry-After` and the body size are handled.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Wait after a 429 without a usable `Retry-After` (seconds form only).
    pub retry_after_default: Duration,
    /// Upper bound on any `Retry-After` we honour.
    pub retry_after_max: Duration,
    pub max_body: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            retry_after_default: Duration::from_secs(60),
            retry_after_max: Duration::from_secs(3600),
            max_body: MAX_BODY_BYTES,
        }
    }
}

/// Result of one bounded GET.
#[derive(Debug)]
pub enum Fetch {
    /// 2xx with a body within the size cap.
    Body(Vec<u8>),
    /// Any other status except 429, with its (capped) body. Prometheus puts
    /// its error message in the body of a 4xx.
    Rejected(StatusCode, Vec<u8>),
    /// HTTP 429: wait this long (already clamped to the limits).
    RateLimited(Duration),
    /// Transport error, oversized body or similar; the text has no URL.
    Failed(String),
}

impl Fetch {
    /// Treat a non-2xx status as a plain failure (`HTTP 500 ...`).
    pub fn reject_as_failure(self) -> Fetch {
        match self {
            Fetch::Rejected(s, _) => Fetch::Failed(format!("HTTP {s}")),
            other => other,
        }
    }
}

fn retry_after(h: Option<&HeaderValue>, l: &Limits) -> Duration {
    h.and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(l.retry_after_default)
        .min(l.retry_after_max)
}

/// Send `req` and read the body, at most `l.max_body` bytes.
pub async fn fetch_body(req: reqwest::RequestBuilder, l: &Limits) -> Fetch {
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return Fetch::Failed(e.without_url().to_string()),
    };
    let status = resp.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Fetch::RateLimited(retry_after(resp.headers().get(RETRY_AFTER), l));
    }
    if resp.content_length().is_some_and(|n| n > l.max_body as u64) {
        return Fetch::Failed("response too large".into());
    }
    let body = match resp.bytes().await {
        Ok(b) if b.len() <= l.max_body => b.to_vec(),
        Ok(_) => return Fetch::Failed("response too large".into()),
        Err(e) => return Fetch::Failed(e.without_url().to_string()),
    };
    if status.is_success() {
        Fetch::Body(body)
    } else {
        Fetch::Rejected(status, body)
    }
}

/// Follow up to 5 redirects, but only within the same host and port: a
/// redirect elsewhere would carry our `Authorization`/API-key headers with
/// it. A redirect that is not followed shows up as an `HTTP 30x` failure.
pub fn same_host_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let same = attempt.previous().last().is_some_and(|prev| {
            let next = attempt.url();
            prev.host_str() == next.host_str()
                && prev.port_or_known_default() == next.port_or_known_default()
        });
        if attempt.previous().len() >= 5 {
            attempt.error("too many redirects")
        } else if same {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// `http://` or `https://` with a host, no whitespace and no
/// `user:password@` (credentials go in `*_env` settings).
pub fn check_http_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or("url must start with http:// or https://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || url.contains(char::is_whitespace) {
        return Err("url must have a host and no spaces".into());
    }
    if authority.contains('@') {
        return Err(
            "url must not contain user:password@; put secrets in an environment variable (header_env / bearer_env)".into(),
        );
    }
    Ok(())
}

/// A valid environment variable *name* (so a pasted secret is rejected).
pub fn is_env_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Timing of a poll loop.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub interval: Duration,
    pub backoff_cap: Duration,
}

impl Timing {
    pub fn new(interval_s: f64) -> Self {
        Timing {
            interval: Duration::from_secs_f64(interval_s),
            backoff_cap: Duration::from_secs(300),
        }
    }
}

/// What one poll achieved.
#[derive(Debug)]
pub enum Outcome {
    /// Data was written. `problems` counts keys that could not be updated
    /// (each is logged by the poller and counted with `ctx.error()` here).
    Ok { problems: usize },
    /// HTTP 429: wait at least this long.
    RateLimited(Duration),
    /// Nothing was written; back off.
    Failed(String),
}

/// Poll until shutdown: never faster than `timing.interval`, honouring
/// `Retry-After`, with exponential backoff (capped) after failures. Health
/// is reported with `ctx.ok()` / `ctx.error()`. `label` names the source
/// type in logs.
pub async fn run_poll_loop<F, Fut>(ctx: &mut SourceCtx, label: &str, timing: Timing, mut poll: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Outcome>,
{
    let mut failures = 0u32;
    let mut next_at = tokio::time::Instant::now();
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(next_at) => {}
            () = ctx.shutdown.cancelled() => return,
        }
        let outcome = tokio::select! {
            o = poll() => o,
            () = ctx.shutdown.cancelled() => return,
        };
        let done = tokio::time::Instant::now();
        match outcome {
            Outcome::Ok { problems } => {
                if failures > 0 {
                    tracing::info!(source = %ctx.name, "{label} source recovered");
                }
                failures = 0;
                if problems == 0 {
                    ctx.ok();
                } else {
                    for _ in 0..problems {
                        ctx.error();
                    }
                }
                next_at = done + timing.interval;
            }
            Outcome::RateLimited(wait) => {
                ctx.error();
                let wait = wait.max(timing.interval);
                tracing::warn!(source = %ctx.name, retry_in = ?wait, "{label} rate limit (HTTP 429)");
                next_at = done + wait;
            }
            Outcome::Failed(msg) => {
                failures = failures.saturating_add(1);
                ctx.error();
                let wait = backoff_delay(timing.interval, failures, timing.backoff_cap);
                tracing::warn!(source = %ctx.name, error = %msg, retry_in = ?wait, "{label} poll failed");
                next_at = done + wait;
            }
        }
    }
}
