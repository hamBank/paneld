//! `type = "http_poll"`: fetch a JSON URL on an interval and write values
//! picked out with JSON paths (DESIGN.md §6.3).
//!
//! Every poll writes all `extract` keys in one [`StoreWriter::apply`] batch.
//! In a **successful** response, a path that is missing or `null` *removes*
//! its key (widgets then show their placeholder); a failed poll (transport
//! error, non-2xx, 429, body not JSON) writes nothing and keeps the old
//! values. Values are converted by [`crate::extract::to_value`]: numbers and
//! numeric strings become `Number` (with the configured `unit`), booleans
//! `Bool`, other strings `Text`, arrays and objects `Json`.
//!
//! Secrets (an `Authorization` header, an API key) come only from
//! environment variables named in `header_env`; they are marked sensitive
//! and never appear in `Debug` output, logs or errors. Errors are logged
//! without the URL (API keys often live in query strings).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value as Json;

use super::ha::read_secret;
use super::poll::{
    check_http_url, fetch_body, is_env_var_name, run_poll_loop, same_host_redirects, Fetch, Limits,
    Outcome, Timing,
};
use super::{Source, SourceCtx, SourceFuture};
use crate::config::HttpPollConfig;
use crate::extract::{to_value, JsonPath};
use crate::store::{Change, StoreWriter};

/// Header names that carry credentials: they must use `header_env`.
fn is_secret_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["auth", "cookie", "key", "token", "secret", "password"]
        .iter()
        .any(|w| n.contains(w))
}

pub fn validate(c: &HttpPollConfig, at: &str, errs: &mut Vec<String>) {
    if let Err(e) = check_http_url(&c.url) {
        errs.push(format!("{at}: {e}"));
    }
    if !(c.interval_s.is_finite() && (1.0..=86_400.0).contains(&c.interval_s)) {
        errs.push(format!(
            "{at}: interval_s must be 1 to 86400 (got {})",
            c.interval_s
        ));
    }
    if !(c.timeout_s.is_finite() && (1.0..=120.0).contains(&c.timeout_s)) {
        errs.push(format!(
            "{at}: timeout_s must be 1 to 120 (got {})",
            c.timeout_s
        ));
    }
    if c.extract.is_empty() {
        errs.push(format!(
            "{at}: extract must list at least one key, e.g. {{ \"weather:temp\" = \"$.current.temp\" }}"
        ));
    }
    for (key, spec) in &c.extract {
        if !crate::condition::is_valid_key(key) {
            errs.push(format!(
                "{at}: extract key {key:?} is not a valid key; keys look like `source:name`"
            ));
        }
        if let Err(e) = JsonPath::parse(spec.path()) {
            errs.push(format!(
                "{at}: extract {key:?}: invalid path {:?}: {e}",
                spec.path()
            ));
        }
    }
    for (name, value) in &c.headers {
        if HeaderName::from_bytes(name.as_bytes()).is_err() {
            errs.push(format!(
                "{at}: headers: {name:?} is not a valid header name"
            ));
        } else if is_secret_header(name) {
            errs.push(format!(
                "{at}: headers: {name:?} looks like a credential; put it in header_env (the name of an environment variable) instead"
            ));
        }
        if HeaderValue::from_str(value).is_err() {
            errs.push(format!(
                "{at}: headers: the value of {name:?} is not a valid header value"
            ));
        }
    }
    for (name, var) in &c.header_env {
        if HeaderName::from_bytes(name.as_bytes()).is_err() {
            errs.push(format!(
                "{at}: header_env: {name:?} is not a valid header name"
            ));
        }
        if !is_env_var_name(var) {
            errs.push(format!(
                "{at}: header_env: the value for {name:?} must be the NAME of an environment variable (letters, digits and '_'), not the secret itself"
            ));
        }
        if c.headers.keys().any(|h| h.eq_ignore_ascii_case(name)) {
            errs.push(format!(
                "{at}: header {name:?} is set in both headers and header_env"
            ));
        }
    }
}

/// Fail early when a `header_env` variable is not set. Names the variable.
pub fn check_environment(c: &HttpPollConfig) -> Result<(), String> {
    for (name, var) in &c.header_env {
        read_secret(var, &format!("the value of the {name} header"))?;
    }
    Ok(())
}

/// Build the request headers: plain ones, then secrets from the environment
/// (marked sensitive so reqwest and `Debug` redact them).
fn build_headers(c: &HttpPollConfig) -> anyhow::Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in &c.headers {
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).context("invalid header name")?,
            HeaderValue::from_str(value).context("invalid header value")?,
        );
    }
    for (name, var) in &c.header_env {
        let secret = read_secret(var, &format!("the value of the {name} header"))
            .map_err(anyhow::Error::msg)?;
        // The error must not echo the secret, so say nothing about the value.
        let mut v = HeaderValue::from_str(secret.expose()).map_err(|_| {
            anyhow::anyhow!("environment variable {var} is not a valid header value")
        })?;
        v.set_sensitive(true);
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).context("invalid header name")?,
            v,
        );
    }
    Ok(map)
}

/// One extract rule, parsed.
#[derive(Debug, Clone)]
pub struct Rule {
    pub key: String,
    pub path: JsonPath,
    pub unit: Option<String>,
}

/// The store changes for one successful response: a `Set` for every key
/// whose path yields a value, a `Remove` for every other key.
pub fn changes_for(rules: &[Rule], root: &Json) -> Vec<Change> {
    rules
        .iter()
        .map(|r| {
            match r
                .path
                .find(root)
                .and_then(|v| to_value(v, r.unit.as_deref()))
            {
                Some(v) => Change::Set(r.key.clone(), v),
                None => Change::Remove(r.key.clone()),
            }
        })
        .collect()
}

pub struct HttpPollSource {
    url: String,
    rules: Vec<Rule>,
    headers: HeaderMap,
    timeout: Duration,
    timing: Timing,
    limits: Limits,
}

// Not derived: `headers` may hold secrets.
impl std::fmt::Debug for HttpPollSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpPollSource")
            .field("rules", &self.rules.len())
            .field("headers", &self.headers) // sensitive values print as "Sensitive"
            .finish_non_exhaustive()
    }
}

impl HttpPollSource {
    /// Reads the `header_env` variables now; the error names a variable,
    /// never a value.
    pub fn new(c: &HttpPollConfig) -> anyhow::Result<Self> {
        let rules = c
            .extract
            .iter()
            .map(|(key, spec)| {
                Ok(Rule {
                    key: key.clone(),
                    path: JsonPath::parse(spec.path())
                        .map_err(|e| anyhow::anyhow!("extract {key:?}: {e}"))?,
                    unit: spec.unit().map(str::to_owned),
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(HttpPollSource {
            url: c.url.clone(),
            rules,
            headers: build_headers(c)?,
            timeout: Duration::from_secs_f64(c.timeout_s),
            timing: Timing::new(c.interval_s),
            limits: Limits::default(),
        })
    }

    #[cfg(test)]
    pub fn with_timing(mut self, timing: Timing, limits: Limits) -> Self {
        self.timing = timing;
        self.limits = limits;
        self
    }
}

async fn poll_once(client: &reqwest::Client, s: &HttpPollSource, store: &StoreWriter) -> Outcome {
    let req = client.get(&s.url).headers(s.headers.clone());
    match fetch_body(req, &s.limits).await.reject_as_failure() {
        Fetch::Body(body) => match serde_json::from_slice::<Json>(&body) {
            Ok(root) => {
                // One batch: a snapshot never shows half of a poll.
                store.apply(changes_for(&s.rules, &root));
                Outcome::Ok { problems: 0 }
            }
            Err(_) => Outcome::Failed("response is not JSON".into()),
        },
        Fetch::RateLimited(d) => Outcome::RateLimited(d),
        Fetch::Failed(m) => Outcome::Failed(m),
        Fetch::Rejected(..) => unreachable!("reject_as_failure"),
    }
}

impl Source for HttpPollSource {
    fn run(self: Box<Self>, mut ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let client = super::http_client()
                .timeout(self.timeout)
                .redirect(same_host_redirects())
                .build()
                .context("building HTTP client")?;
            let this = Arc::new(*self);
            let timing = this.timing;
            let store = ctx.store.clone();
            run_poll_loop(&mut ctx, "http_poll", timing, || {
                let (client, this, store) = (client.clone(), this.clone(), store.clone());
                async move { poll_once(&client, &this, &store).await }
            })
            .await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
