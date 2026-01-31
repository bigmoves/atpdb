use crate::app::AppState;
use crate::config::Mode;
use crate::repos::RepoState;
use crate::sync_worker::SyncHandle;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, error, info, warn};

/// Ensure URL has https:// scheme for HTTP API calls
fn to_https_url(url: &str) -> String {
    if url.starts_with("https://") || url.starts_with("http://") {
        url.to_string()
    } else if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{}", rest)
    } else if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{}", rest)
    } else {
        format!("https://{}", url)
    }
}

#[derive(Error, Debug)]
pub enum CrawlerError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("storage error: {0}")]
    Storage(#[from] fjall::Error),
}

// Response for com.atproto.sync.listRepos
#[derive(Debug, Deserialize)]
pub struct ListReposResponse {
    pub cursor: Option<String>,
    pub repos: Vec<RepoInfo>,
}

#[derive(Debug, Deserialize)]
pub struct RepoInfo {
    pub did: String,
    #[allow(dead_code)]
    pub head: String,
    #[serde(default)]
    pub active: Option<bool>,
}

// Response for com.atproto.sync.listReposByCollection (different structure - only has did)
#[derive(Debug, Deserialize)]
pub struct ListReposByCollectionResponse {
    pub cursor: Option<String>,
    pub repos: Vec<RepoByCollection>,
}

#[derive(Debug, Deserialize)]
pub struct RepoByCollection {
    pub did: String,
}

pub struct Crawler {
    app: Arc<AppState>,
    sync_handle: Arc<SyncHandle>,
    client: reqwest::blocking::Client,
    running: Arc<AtomicBool>,
}

impl Crawler {
    pub fn new(app: Arc<AppState>, sync_handle: Arc<SyncHandle>, running: Arc<AtomicBool>) -> Self {
        Crawler {
            app,
            sync_handle,
            client: reqwest::blocking::Client::builder()
                .user_agent("atpdb/0.1")
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            running,
        }
    }

    /// Check if we should continue running
    fn should_continue(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Enumerate all repos using com.atproto.sync.listRepos
    pub fn enumerate_all(&self) -> Result<usize, CrawlerError> {
        let config = self.app.config();
        let relay = to_https_url(&config.relay);
        let relay_base = relay.trim_end_matches('/');
        let cursor_key = format!("listRepos:{}", relay_base);

        let mut cursor = self.app.get_cursor(&cursor_key);
        let mut total_discovered = 0;

        loop {
            // Check for shutdown signal
            if !self.should_continue() {
                info!("Crawler shutdown requested, stopping enumeration");
                break;
            }

            let url = match &cursor {
                Some(c) if !c.is_empty() => format!(
                    "{}/xrpc/com.atproto.sync.listRepos?limit=1000&cursor={}",
                    relay_base, c
                ),
                _ => format!("{}/xrpc/com.atproto.sync.listRepos?limit=1000", relay_base),
            };

            debug!(url, "Crawling repos");
            let response: ListReposResponse = match self.client.get(&url).send() {
                Ok(r) => r.json()?,
                Err(_) if !self.should_continue() => {
                    info!("Crawler shutdown during request");
                    return Ok(total_discovered);
                }
                Err(e) => return Err(e.into()),
            };

            for repo in &response.repos {
                // Skip inactive repos
                if repo.active == Some(false) {
                    continue;
                }

                // Add if not already tracked
                if !self.app.repos.contains(&repo.did).unwrap_or(true) {
                    let state = RepoState::new(repo.did.clone());
                    let _ = self.app.repos.put(&state);
                    let _ = self.sync_handle.queue_blocking(repo.did.clone());
                    total_discovered += 1;
                }
            }

            // Save cursor for resumability (keep it even when done, like indigo)
            if let Some(ref c) = response.cursor {
                let _ = self.app.set_cursor(&cursor_key, c);
                cursor = Some(c.clone());
            } else {
                // Done - keep cursor so we don't re-enumerate everything next run
                break;
            }

            // Rate limit
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        Ok(total_discovered)
    }

    /// Enumerate repos by collection using com.atproto.sync.listReposByCollection
    pub fn enumerate_by_collection(&self, collection: &str) -> Result<usize, CrawlerError> {
        let config = self.app.config();
        let relay = to_https_url(&config.relay);
        let relay_base = relay.trim_end_matches('/');
        let cursor_key = format!("listReposByCollection:{}:{}", relay_base, collection);

        let mut cursor = self.app.get_cursor(&cursor_key);
        let mut total_discovered = 0;

        loop {
            // Check for shutdown signal
            if !self.should_continue() {
                info!("Crawler shutdown requested, stopping enumeration");
                break;
            }

            let url = match &cursor {
                Some(c) if !c.is_empty() => format!(
                    "{}/xrpc/com.atproto.sync.listReposByCollection?collection={}&limit=1000&cursor={}",
                    relay_base, collection, c
                ),
                _ => format!(
                    "{}/xrpc/com.atproto.sync.listReposByCollection?collection={}&limit=1000",
                    relay_base, collection
                ),
            };

            debug!(url, "Crawling by collection");
            let response: ListReposByCollectionResponse = match self.client.get(&url).send() {
                Ok(r) => r.json()?,
                Err(_) if !self.should_continue() => {
                    info!("Crawler shutdown during request");
                    return Ok(total_discovered);
                }
                Err(e) => return Err(e.into()),
            };

            for repo in &response.repos {
                if !self.app.repos.contains(&repo.did).unwrap_or(true) {
                    let state = RepoState::new(repo.did.clone());
                    let _ = self.app.repos.put(&state);
                    let _ = self.sync_handle.queue_blocking(repo.did.clone());
                    total_discovered += 1;
                }
            }

            if let Some(ref c) = response.cursor {
                if c.is_empty() {
                    break;
                }
                let _ = self.app.set_cursor(&cursor_key, c);
                cursor = Some(c.clone());
            } else {
                // Done - keep cursor so we don't re-enumerate everything next run
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        Ok(total_discovered)
    }

    /// Run crawler based on current config mode
    pub fn run_based_on_mode(&self) -> Result<usize, CrawlerError> {
        let config = self.app.config();

        match config.mode {
            Mode::Manual => {
                info!("Crawler: Manual mode, skipping enumeration");
                Ok(0)
            }
            Mode::Signal => {
                if let Some(ref collection) = config.signal_collection {
                    info!(
                        collection,
                        "Crawler: Signal mode, enumerating by collection"
                    );
                    self.enumerate_by_collection(collection)
                } else {
                    warn!("Crawler: Signal mode but no signal_collection configured");
                    Ok(0)
                }
            }
            Mode::FullNetwork => {
                info!("Crawler: Full network mode, enumerating all repos");
                self.enumerate_all()
            }
        }
    }
}

/// How often to re-run the crawler (5 minutes)
const CRAWLER_INTERVAL_SECS: u64 = 300;

/// Start crawler in background thread (loops continuously)
pub fn start_crawler(app: Arc<AppState>, sync_handle: Arc<SyncHandle>, running: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let crawler = Crawler::new(app, sync_handle, running.clone());

        while running.load(std::sync::atomic::Ordering::Relaxed) {
            match crawler.run_based_on_mode() {
                Ok(count) => info!(count, "Crawler: discovered new repos"),
                Err(e) => error!(error = %e, "Crawler error"),
            }

            // Wait before next run (check running flag periodically)
            for _ in 0..(CRAWLER_INTERVAL_SECS / 5) {
                if !running.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }

        info!("Crawler: shutdown complete");
    });
}
