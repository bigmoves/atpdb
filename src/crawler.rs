use crate::app::AppState;
use crate::config::Mode;
use crate::repos::RepoState;
use crate::sync_worker::SyncHandle;
use serde::Deserialize;
use std::sync::Arc;
use thiserror::Error;

/// Ensure URL has https:// scheme for HTTP API calls
fn to_https_url(url: &str) -> String {
    if url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with("http://") {
        url.to_string()
    } else if url.starts_with("wss://") {
        format!("https://{}", &url[6..])
    } else if url.starts_with("ws://") {
        format!("http://{}", &url[5..])
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
}

impl Crawler {
    pub fn new(app: Arc<AppState>, sync_handle: Arc<SyncHandle>) -> Self {
        Crawler {
            app,
            sync_handle,
            client: reqwest::blocking::Client::new(),
        }
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
            let url = match &cursor {
                Some(c) if !c.is_empty() => format!(
                    "{}/xrpc/com.atproto.sync.listRepos?limit=1000&cursor={}",
                    relay_base, c
                ),
                _ => format!(
                    "{}/xrpc/com.atproto.sync.listRepos?limit=1000",
                    relay_base
                ),
            };

            println!("Crawling: {}", url);
            let response: ListReposResponse = self.client.get(&url).send()?.json()?;

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

            // Save cursor for resumability
            if let Some(ref c) = response.cursor {
                let _ = self.app.set_cursor(&cursor_key, c);
                cursor = Some(c.clone());
            } else {
                // Done - clear cursor for next full run
                let _ = self.app.set_cursor(&cursor_key, "");
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

            println!("Crawling by collection: {}", url);
            let response: ListReposByCollectionResponse = self.client.get(&url).send()?.json()?;

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
                let _ = self.app.set_cursor(&cursor_key, "");
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
                println!("Crawler: Manual mode, skipping enumeration");
                Ok(0)
            }
            Mode::Signal => {
                if let Some(ref collection) = config.signal_collection {
                    println!("Crawler: Signal mode, enumerating by collection: {}", collection);
                    self.enumerate_by_collection(collection)
                } else {
                    println!("Crawler: Signal mode but no signal_collection configured");
                    Ok(0)
                }
            }
            Mode::FullNetwork => {
                println!("Crawler: Full network mode, enumerating all repos");
                self.enumerate_all()
            }
        }
    }
}

/// Start crawler in background thread
pub fn start_crawler(app: Arc<AppState>, sync_handle: Arc<SyncHandle>) {
    std::thread::spawn(move || {
        let crawler = Crawler::new(app, sync_handle);
        match crawler.run_based_on_mode() {
            Ok(count) => println!("Crawler: discovered {} new repos", count),
            Err(e) => eprintln!("Crawler error: {}", e),
        }
    });
}
