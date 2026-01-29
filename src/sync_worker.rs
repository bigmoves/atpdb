use crate::app::AppState;
use crate::repos::RepoStatus;
use crate::sync::sync_repo;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

const RETRY_BASE_SECS: u64 = 60;
const RETRY_MAX_SECS: u64 = 3600;

pub struct SyncWorker {
    app: Arc<AppState>,
    queue_rx: mpsc::Receiver<String>,
    parallelism: u32,
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

pub fn start_sync_worker(app: Arc<AppState>) -> SyncHandle {
    let config = app.config();
    let (queue_tx, queue_rx) = mpsc::channel(1000);

    let worker = SyncWorker {
        app,
        queue_rx,
        parallelism: config.sync_parallelism,
    };

    tokio::spawn(worker.run());

    SyncHandle { queue_tx }
}

impl SyncWorker {
    async fn run(mut self) {
        // Spawn retry checker
        let app_clone = Arc::clone(&self.app);
        tokio::spawn(async move {
            retry_loop(app_clone).await;
        });

        // Process queue
        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.parallelism as usize));

        while let Some(did) = self.queue_rx.recv().await {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let app = Arc::clone(&self.app);

            tokio::spawn(async move {
                sync_one_repo(&app, &did).await;
                drop(permit);
            });
        }
    }
}

async fn sync_one_repo(app: &AppState, did: &str) {
    // Update status to syncing
    if let Ok(Some(mut state)) = app.repos.get(did) {
        state.status = RepoStatus::Syncing;
        state.error = None;
        if let Err(e) = app.repos.put(&state) {
            eprintln!("Failed to update repo status for {}: {}", did, e);
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
        sync_repo(&did_owned, &store, &collections, &indexes, search.as_ref(), &search_fields)
    })
        .await;

    // Update state based on result
    if let Ok(Some(mut state)) = app.repos.get(did) {
        match result {
            Ok(Ok(sync_result)) => {
                state.status = RepoStatus::Synced;
                state.rev = sync_result.rev;
                state.record_count = sync_result.record_count as u64;
                state.last_sync = Some(now_secs());
                state.error = None;
                state.retry_count = 0;
                state.next_retry = None;
            }
            Ok(Err(e)) => {
                state.status = RepoStatus::Error;
                state.error = Some(format!("{}", e));
                state.retry_count += 1;
                state.next_retry = Some(calculate_next_retry(state.retry_count));
            }
            Err(e) => {
                state.status = RepoStatus::Error;
                state.error = Some(format!("task panicked: {}", e));
                state.retry_count += 1;
                state.next_retry = Some(calculate_next_retry(state.retry_count));
            }
        }
        if let Err(e) = app.repos.put(&state) {
            eprintln!("Failed to update repo state for {}: {}", did, e);
        }
    }
}

async fn retry_loop(app: Arc<AppState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        let now = now_secs();
        if let Ok(errored) = app.repos.list_by_status(RepoStatus::Error) {
            for repo in errored {
                if let Some(next_retry) = repo.next_retry {
                    if next_retry <= now {
                        // Reset to pending for retry
                        let mut state = repo.clone();
                        state.status = RepoStatus::Pending;
                        if let Err(e) = app.repos.put(&state) {
                            eprintln!("Failed to reset repo {} for retry: {}", repo.did, e);
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
