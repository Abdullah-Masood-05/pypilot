//! PyPI metadata source — the network client and its test-friendly abstraction.
//!
//! Everything downstream (the F3 engine, the solver) is generic over
//! [`MetadataSource`], so tests inject recorded fixtures and never touch the
//! network. The production [`PyPiClient`] layers the disk [`Cache`] over reqwest.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;

use crate::pypi::cache::Cache;
use crate::pypi::metadata::PackageMetadata;

/// Anything that can resolve a package name to its latest-release metadata.
///
/// Uses native `async fn` in traits (stable), so the engine is generic over the
/// source rather than boxing — zero-cost and easy to fake in tests. The engine
/// only ever uses this through concrete generic parameters (never `dyn`), so the
/// missing `Send` bound the lint warns about doesn't apply here.
#[allow(async_fn_in_trait)]
pub trait MetadataSource {
    async fn fetch(&self, name: &str) -> crate::Result<PackageMetadata>;
}

/// Live PyPI client: disk cache first, then `pypi.org/pypi/<name>/json`.
pub struct PyPiClient {
    http: reqwest::Client,
    cache: Cache,
    base_url: String,
}

impl PyPiClient {
    pub fn new() -> PyPiClient {
        PyPiClient::with_cache(Cache::open())
    }

    pub fn with_cache(cache: Cache) -> PyPiClient {
        let http = reqwest::Client::builder()
            .user_agent(concat!("pypilot/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client with rustls should build");
        PyPiClient {
            http,
            cache,
            base_url: "https://pypi.org/pypi".to_string(),
        }
    }
}

impl Default for PyPiClient {
    fn default() -> Self {
        PyPiClient::new()
    }
}

impl MetadataSource for PyPiClient {
    async fn fetch(&self, name: &str) -> crate::Result<PackageMetadata> {
        // 1. Fresh-enough latest pointer? (24h TTL, handled inside the cache.)
        if let Some(meta) = self.cache.get_latest(name) {
            return Ok(meta);
        }

        // 2. Network.
        let url = format!("{}/{}/json", self.base_url, name);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting metadata for `{name}`"))?;

        if !resp.status().is_success() {
            anyhow::bail!("PyPI returned {} for `{name}`", resp.status());
        }

        let meta: PackageMetadata = resp
            .json()
            .await
            .with_context(|| format!("parsing PyPI JSON for `{name}`"))?;

        // 3. Persist into both cache tiers.
        self.cache.put(name, &meta);
        Ok(meta)
    }
}

/// Recorded-fixture source used by tests — no network, ever.
#[derive(Default, Clone)]
pub struct FixtureSource {
    by_name: HashMap<String, PackageMetadata>,
}

impl FixtureSource {
    pub fn new() -> FixtureSource {
        FixtureSource::default()
    }

    /// Insert metadata under a name (normalized to lowercase).
    pub fn insert(&mut self, name: &str, meta: PackageMetadata) {
        self.by_name.insert(name.to_lowercase(), meta);
    }

    /// Load a recorded PyPI JSON document from disk and register it under `name`.
    pub fn load_json(&mut self, name: &str, path: impl AsRef<Path>) -> crate::Result<()> {
        let text = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("reading fixture {}", path.as_ref().display()))?;
        let meta: PackageMetadata = serde_json::from_str(&text)
            .with_context(|| format!("parsing fixture {}", path.as_ref().display()))?;
        self.insert(name, meta);
        Ok(())
    }
}

impl MetadataSource for FixtureSource {
    async fn fetch(&self, name: &str) -> crate::Result<PackageMetadata> {
        self.by_name
            .get(&name.to_lowercase())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no fixture registered for `{name}`"))
    }
}
