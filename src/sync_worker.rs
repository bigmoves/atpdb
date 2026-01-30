use crate::app::AppState;
use crate::repos::RepoStatus;
use crate::resync_buffer::BufferedOperation;
use crate::storage::Record;
use crate::sync::{cleanup_pds_backoff, sync_repo, SyncError};
use crate::types::AtUri;
use metrics::{counter, gauge};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const RETRY_BASE_SECS: u64 = 60;
const RETRY_MAX_SECS: u64 = 3600;
/// Short retry delay for rate-limited repos (15 seconds)
const RATE_LIMIT_RETRY_SECS: u64 = 15;

pub struct SyncWorker {
    app: Arc<AppState>,
    queue_rx: mpsc::Receiver<String>,
    parallelism: u32,
    running: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct SyncHandle {
    queue_tx: mpsc::Sender<String>,
}

impl SyncHandle {
    pub async fn queue(&self, did: String) -> Result<(), mpsc::error::SendError<String>> {
        self.queue_tx.send(did).await
    }

    pub fn queue_blocking(&self, did: String) -> Result<(), mpsc::error::SendError<String>> {
        self.queue_tx.blocking_send(did)
    }
}

pub fn start_sync_worker(app: Arc<AppState>, running: Arc<AtomicBool>) -> SyncHandle {
    let config = app.config();
    let (queue_tx, queue_rx) = mpsc::channel(1000);

    let handle = SyncHandle {
        queue_tx: queue_tx.clone(),
    };

    let worker = SyncWorker {
        app: Arc::clone(&app),
        queue_rx,
        parallelism: config.sync_parallelism,
        running,
    };

    // Pass handle clone to worker for retry_loop
    let retry_handle = handle.clone();
    tokio::spawn(worker.run(retry_handle));

    // Re-queue any repos that were pending or syncing from a previous run
    let resume_handle = handle.clone();
    tokio::spawn(async move {
        resume_incomplete_syncs(&app, &resume_handle).await;
    });

    handle
}

async fn resume_incomplete_syncs(app: &AppState, handle: &SyncHandle) {
    let mut count = 0;

    // Resume repos stuck in "syncing" (were in-progress when we crashed)
    if let Ok(syncing) = app.repos.list_by_status(RepoStatus::Syncing) {
        for repo in syncing {
            if handle.queue(repo.did.clone()).await.is_ok() {
                count += 1;
            }
        }
    }

    // Resume repos in "pending" (were queued but not started)
    if let Ok(pending) = app.repos.list_by_status(RepoStatus::Pending) {
        for repo in pending {
            if handle.queue(repo.did.clone()).await.is_ok() {
                count += 1;
            }
        }
    }

    if count > 0 {
        info!(count, "Resumed incomplete syncs from previous run");
    }
}

impl SyncWorker {
    async fn run(mut self, retry_handle: SyncHandle) {
        // Spawn retry checker with handle for re-queuing
        let app_clone = Arc::clone(&self.app);
        let running_clone = Arc::clone(&self.running);
        tokio::spawn(async move {
            retry_loop(app_clone, retry_handle, running_clone).await;
        });

        // Process queue
        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.parallelism as usize));

        while let Some(did) = self.queue_rx.recv().await {
            gauge!("sync_queue_depth").set(self.queue_rx.len() as f64);
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            gauge!("sync_workers_active").increment(1.0);
            let app = Arc::clone(&self.app);

            tokio::spawn(async move {
                sync_one_repo(&app, &did).await;
                gauge!("sync_workers_active").decrement(1.0);
                drop(permit);
            });
        }
    }
}

async fn sync_one_repo(app: &AppState, did: &str) {
    // Check if this is a fresh sync (never synced before)
    let is_fresh_sync = match app.repos.get(did) {
        Ok(Some(state)) => state.last_sync.is_none(),
        _ => true, // Assume fresh if we can't read state
    };

    // Update status to syncing
    if let Ok(Some(mut state)) = app.repos.get(did) {
        state.status = RepoStatus::Syncing;
        state.error = None;
        if let Err(e) = app.repos.put(&state) {
            warn!(did, error = %e, "Failed to update repo status");
        }
    }

    let config = app.config();
    let store = Arc::new(app.store.clone());
    let search = app.search.clone();

    // Run sync in blocking task
    let did_owned = did.to_string();
    let collections = config.collections.clone();
    let indexes = config.indexes.clone();
    let search_fields = config.search_fields.clone();

    let result = tokio::task::spawn_blocking(move || {
        sync_repo(
            &did_owned,
            &store,
            &collections,
            &indexes,
            search.as_ref(),
            &search_fields,
            is_fresh_sync,
        )
    })
    .await;

    // Update state based on result
    if let Ok(Some(mut state)) = app.repos.get(did) {
        match result {
            Ok(Ok(sync_result)) => {
                counter!("sync_completed_total", "status" => "success").increment(1);
                counter!("sync_records_total").increment(sync_result.record_count as u64);
                state.status = RepoStatus::Synced;
                state.rev = sync_result.rev;
                state.record_count = sync_result.record_count as u64;
                state.last_sync = Some(now_secs());
                state.error = None;
                state.retry_count = 0;
                state.next_retry = None;
                // Store handle if resolved
                if let Some(handle) = sync_result.handle {
                    let _ = app.set_handle(did, &handle);
                }

                // Drain and process any buffered firehose commits
                drain_resync_buffer(app, did);
            }
            Ok(Err(SyncError::RateLimited)) => {
                // Rate limited - retry soon without counting as an error
                counter!("sync_completed_total", "status" => "rate_limited").increment(1);
                debug!(did, "Repo sync rate limited, will retry shortly");
                state.status = RepoStatus::Pending;
                state.next_retry = Some(now_secs() + RATE_LIMIT_RETRY_SECS);
                // Don't increment retry_count - this isn't a real error
            }
            Ok(Err(e)) => {
                counter!("sync_completed_total", "status" => "error").increment(1);
                state.status = RepoStatus::Error;
                state.error = Some(format!("{}", e));
                state.retry_count += 1;
                state.next_retry = Some(calculate_next_retry(state.retry_count));
            }
            Err(e) => {
                counter!("sync_completed_total", "status" => "panic").increment(1);
                state.status = RepoStatus::Error;
                state.error = Some(format!("task panicked: {}", e));
                state.retry_count += 1;
                state.next_retry = Some(calculate_next_retry(state.retry_count));
            }
        }
        if let Err(e) = app.repos.put(&state) {
            warn!(did, error = %e, "Failed to update repo state");
        }
    }
}

async fn retry_loop(app: Arc<AppState>, handle: SyncHandle, running: Arc<AtomicBool>) {
    let mut tick_count: u32 = 0;

    while running.load(Ordering::Relaxed) {
        // 5 second interval for quick rate-limit recovery
        tokio::time::sleep(Duration::from_secs(5)).await;
        if !running.load(Ordering::Relaxed) {
            break;
        }
        tick_count = tick_count.wrapping_add(1);

        // Clean up PDS backoff map every ~60 seconds (12 ticks)
        if tick_count.is_multiple_of(12) {
            cleanup_pds_backoff();
        }

        let now = now_secs();

        // Check Error repos ready for retry
        if let Ok(errored) = app.repos.list_by_status(RepoStatus::Error) {
            for repo in errored {
                if let Some(next_retry) = repo.next_retry {
                    if next_retry <= now {
                        // Reset to pending and re-queue
                        let mut state = repo.clone();
                        state.status = RepoStatus::Pending;
                        state.next_retry = None;
                        if let Err(e) = app.repos.put(&state) {
                            warn!(did = repo.did, error = %e, "Failed to reset repo for retry");
                            continue;
                        }
                        if let Err(e) = handle.queue(repo.did.clone()).await {
                            warn!(did = repo.did, error = %e, "Failed to re-queue repo for retry");
                        } else {
                            debug!(did = repo.did, "Re-queued error repo for retry");
                        }
                    }
                }
            }
        }

        // Check Pending repos with next_retry set (rate-limited repos)
        if let Ok(pending) = app.repos.list_by_status(RepoStatus::Pending) {
            for repo in pending {
                if let Some(next_retry) = repo.next_retry {
                    if next_retry <= now {
                        // Clear next_retry and re-queue
                        let mut state = repo.clone();
                        state.next_retry = None;
                        if let Err(e) = app.repos.put(&state) {
                            warn!(did = repo.did, error = %e, "Failed to update repo state");
                            continue;
                        }
                        if let Err(e) = handle.queue(repo.did.clone()).await {
                            warn!(did = repo.did, error = %e, "Failed to re-queue rate-limited repo");
                        } else {
                            debug!(did = repo.did, "Re-queued rate-limited repo");
                        }
                    }
                }
            }
        }
    }
}

fn calculate_next_retry(retry_count: u32) -> u64 {
    let delay = RETRY_BASE_SECS * 2u64.pow(retry_count.min(6));
    let delay = delay.min(RETRY_MAX_SECS);
    now_secs() + delay
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Drain and process any buffered firehose commits for a DID
fn drain_resync_buffer(app: &AppState, did: &str) {
    let commits = match app.resync_buffer.drain(did) {
        Ok(c) => c,
        Err(e) => {
            warn!(did, error = %e, "Failed to drain resync buffer");
            return;
        }
    };

    if commits.is_empty() {
        return;
    }

    let config = app.config();
    let mut replayed = 0;

    for commit in commits {
        for op in commit.operations {
            match op {
                BufferedOperation::Create { uri, cid, value }
                | BufferedOperation::Update { uri, cid, value } => {
                    let Ok(parsed_uri) = uri.parse::<AtUri>() else {
                        continue;
                    };

                    let record = Record {
                        uri: uri.clone(),
                        cid,
                        value: value.clone(),
                        indexed_at: now_secs(),
                    };

                    if let Err(e) =
                        app.store
                            .put_with_indexes(&parsed_uri, &record, &config.indexes)
                    {
                        warn!(uri, error = %e, "Failed to replay buffered record");
                        continue;
                    }

                    // Index for search
                    if let Some(ref search) = app.search {
                        let _ = search.index_record(
                            &uri,
                            parsed_uri.collection.as_str(),
                            &value,
                            &config.search_fields,
                        );
                    }

                    replayed += 1;
                }
                BufferedOperation::Delete { uri } => {
                    let Ok(parsed_uri) = uri.parse::<AtUri>() else {
                        continue;
                    };

                    if let Err(e) = app.store.delete_with_indexes(&parsed_uri, &config.indexes) {
                        warn!(uri, error = %e, "Failed to replay buffered delete");
                        continue;
                    }

                    // Remove from search index
                    if let Some(ref search) = app.search {
                        let _ = search.delete_record(&uri);
                    }

                    replayed += 1;
                }
            }
        }
    }

    if replayed > 0 {
        info!(did, replayed, "Replayed buffered firehose commits");
    }
}
