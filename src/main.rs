#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

mod app;
mod cbor;
mod config;
mod crawler;
mod firehose;
mod indexer;
mod query;
mod repos;
mod search;
mod server;
mod storage;
mod sync;
mod sync_worker;
mod types;

use app::AppState;
use config::Mode;
use firehose::{Event, FirehoseClient, Operation};
use query::Query;
use repos::RepoState;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

enum CommandResult {
    Continue,
    Exit,
    StartStreaming(String),
}

/// Fetch handle for a DID from PLC directory or did:web endpoint
fn fetch_handle_from_plc(did: &str) -> Result<Option<String>, String> {
    let url = if let Some(domain) = did.strip_prefix("did:web:") {
        // did:web DIDs: fetch from https://{domain}/.well-known/did.json
        let domain = domain.replace("%3A", ":").replace("%2F", "/");
        format!("https://{}/.well-known/did.json", domain)
    } else {
        // did:plc DIDs: fetch from PLC directory
        format!("https://plc.directory/{}", did)
    };

    let client = reqwest::blocking::Client::new();
    let response = client
        .get(&url)
        .send()
        .map_err(|e| format!("HTTP error: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let body: serde_json::Value = response
        .json()
        .map_err(|e| format!("JSON parse error: {}", e))?;

    // Extract handle from alsoKnownAs field
    // Format: ["at://handle.example.com"]
    if let Some(also_known_as) = body.get("alsoKnownAs").and_then(|v| v.as_array()) {
        for aka in also_known_as {
            if let Some(s) = aka.as_str() {
                if let Some(handle) = s.strip_prefix("at://") {
                    return Ok(Some(handle.to_string()));
                }
            }
        }
    }

    Ok(None)
}

fn handle_command(line: &str, app: &Arc<AppState>, running: &Arc<AtomicBool>) -> CommandResult {
    match line {
        ".quit" | ".exit" => CommandResult::Exit,

        ".help" => {
            println!("Commands:");
            println!("  .connect <relay>  Connect to firehose (default: bsky.network)");
            println!("  .disconnect       Stop firehose");
            println!("  .stats            Show statistics");
            println!("  .dids             List unique DIDs");
            println!("  .collections      List unique collections");
            println!();
            println!("Repo management:");
            println!("  .repos                    List tracked repos");
            println!("  .repos add <did> [...]    Add repos to track");
            println!("  .repos remove <did>       Remove tracked repo");
            println!("  .repos status <did>       Show repo status");
            println!("  .repos errors             List errored repos");
            println!("  .repos resync <did>       Force resync");
            println!("  .repos resync --errors    Resync all errored");
            println!();
            println!("Config:");
            println!("  .config                   Show current config");
            println!("  .config mode <mode>       Set mode (manual|signal|full-network)");
            println!("  .config signal <nsid>     Set signal collection");
            println!("  .config collections <..>  Set collection filters");
            println!("  .config relay <hostname>  Set relay hostname");
            println!("  .config indexes <...>     Set indexes (col:field:type,...)");
            println!("  .config search list       List search fields");
            println!("  .config search add <...>  Add search field (col:field)");
            println!("  .config search remove <.> Remove search field (col:field)");
            println!();
            println!("Indexes:");
            println!("  .index list               List configured indexes");
            println!("  .index rebuild <spec>     Rebuild index (col:field:type)");
            println!("  .index rebuild-all        Rebuild all configured indexes");
            println!("  .index rebuild-ts <col>   Rebuild __ts__ index for collection");
            println!();
            println!("Search:");
            println!("  .reindex search           Reindex all records for search");
            println!();
            println!("Handles:");
            println!(
                "  .handles sync             Fetch handles for all known DIDs from PLC directory"
            );
            println!("  .handles get <did>        Get stored handle for a DID");
            println!("  .handles fetch <did>      Fetch and store handle from PLC directory");
            println!();
            println!("Counts:");
            println!("  .count <collection>       Show collection count");
            println!("  .count rebuild <col>      Rebuild count for collection");
            println!("  .count rebuild-all        Rebuild counts for all collections");
            println!();
            println!("Queries:");
            println!("  at://did/collection/rkey   Get single record");
            println!("  at://did/collection/*      Get all records in collection for DID");
            println!("  at://*/collection/*        Get all records in collection (all DIDs)");
            println!("  at://did?sync              Sync a user's repo from their PDS");
            println!();
            println!("  .quit             Exit");
            CommandResult::Continue
        }

        ".stats" => {
            match app.store.count() {
                Ok(count) => println!("Records: {}", count),
                Err(e) => println!("Error: {}", e),
            }

            match app.repos.list() {
                Ok(repos) => println!("Tracked repos: {}", repos.len()),
                Err(e) => println!("Error: {}", e),
            }

            let config = app.config();
            println!("Mode: {}", config.mode);

            match app.list_cursors() {
                Ok(cursors) => {
                    if cursors.is_empty() {
                        println!("Crawler cursors: (none)");
                    } else {
                        println!("Crawler cursors:");
                        for (key, value) in cursors {
                            let display = if value.is_empty() {
                                "(complete)"
                            } else {
                                &value
                            };
                            println!("  {}: {}", key, display);
                        }
                    }
                }
                Err(e) => println!("Error: {}", e),
            }

            CommandResult::Continue
        }

        ".dids" => {
            match app.store.unique_dids() {
                Ok(dids) => {
                    println!("Unique DIDs ({}):", dids.len());
                    for did in dids {
                        println!("  {}", did);
                    }
                }
                Err(e) => println!("Error: {}", e),
            }
            CommandResult::Continue
        }

        ".collections" => {
            match app.store.unique_collections() {
                Ok(cols) => {
                    println!("Unique collections ({}):", cols.len());
                    for col in cols {
                        println!("  {}", col);
                    }
                }
                Err(e) => println!("Error: {}", e),
            }
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".debug ") => {
            let rest = cmd.strip_prefix(".debug ").unwrap().trim();
            let parts: Vec<&str> = rest.split_whitespace().collect();

            match parts.first().copied() {
                Some("keys") => {
                    let prefix = parts.get(1).copied().unwrap_or("");
                    let limit: usize = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);

                    println!("Keys with prefix '{}' (limit {}):", prefix, limit);
                    let mut count = 0;
                    for item in app.store.records_keyspace().prefix(prefix.as_bytes()) {
                        if count >= limit {
                            println!("  ... (more keys exist)");
                            break;
                        }
                        if let Ok(key) = item.key() {
                            let key_str = String::from_utf8_lossy(&key);
                            println!("  {}", key_str);
                        }
                        count += 1;
                    }
                    println!("Showed {} keys", count);
                }
                Some("count") => {
                    let prefix = parts.get(1).copied().unwrap_or("");
                    let count = app
                        .store
                        .records_keyspace()
                        .prefix(prefix.as_bytes())
                        .count();
                    println!("Keys with prefix '{}': {}", prefix, count);
                }
                Some("get") => {
                    if let Some(key) = parts.get(1) {
                        match app.store.records_keyspace().get(key.as_bytes()) {
                            Ok(Some(val)) => {
                                // Try to parse as JSON
                                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&val)
                                {
                                    println!("{}", serde_json::to_string_pretty(&json).unwrap());
                                } else {
                                    println!(
                                        "Raw ({} bytes): {:?}",
                                        val.len(),
                                        String::from_utf8_lossy(&val)
                                    );
                                }
                            }
                            Ok(None) => println!("Key not found"),
                            Err(e) => println!("Error: {}", e),
                        }
                    } else {
                        println!("Usage: .debug get <key>");
                    }
                }
                _ => {
                    println!("Debug commands:");
                    println!(
                        "  .debug keys [prefix] [limit]  List keys by prefix (default limit 20)"
                    );
                    println!("  .debug count [prefix]         Count keys by prefix");
                    println!("  .debug get <key>              Get value for exact key");
                    println!();
                    println!("Common prefixes:");
                    println!("  at://                         Records");
                    println!("  idx:                          Index entries");
                    println!("  idx:a:__ts__:                 Timestamp indexes (for counts)");
                }
            }
            CommandResult::Continue
        }

        ".disconnect" => {
            running.store(false, Ordering::Relaxed);
            println!("Disconnecting...");
            CommandResult::Continue
        }

        ".config" => {
            let config = app.config();
            println!("Mode: {}", config.mode);
            if let Some(signal) = &config.signal_collection {
                println!("Signal collection: {}", signal);
            }
            if !config.collections.is_empty() {
                println!("Collection filters: {}", config.collections.join(", "));
            }
            println!("Relay: {}", config.relay);
            println!("Sync parallelism: {}", config.sync_parallelism);
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".config ") => {
            let parts: Vec<&str> = cmd
                .strip_prefix(".config ")
                .unwrap()
                .splitn(2, ' ')
                .collect();
            if parts.is_empty() {
                println!("Usage: .config <key> <value>");
                return CommandResult::Continue;
            }

            match parts[0] {
                "mode" => {
                    if parts.len() < 2 {
                        println!("Usage: .config mode <manual|signal|full-network>");
                        return CommandResult::Continue;
                    }
                    let mode = match parts[1] {
                        "manual" => Mode::Manual,
                        "signal" => Mode::Signal,
                        "full-network" => Mode::FullNetwork,
                        _ => {
                            println!("Invalid mode. Use: manual, signal, full-network");
                            return CommandResult::Continue;
                        }
                    };
                    if let Err(e) = app.update_config(|c| c.mode = mode) {
                        println!("Error: {}", e);
                    } else {
                        println!("Mode set to {}", mode);
                    }
                }
                "signal" => {
                    if parts.len() < 2 {
                        println!("Usage: .config signal <collection>");
                        return CommandResult::Continue;
                    }
                    let signal = parts[1].to_string();
                    if let Err(e) =
                        app.update_config(|c| c.signal_collection = Some(signal.clone()))
                    {
                        println!("Error: {}", e);
                    } else {
                        println!("Signal collection set to {}", signal);
                    }
                }
                "collections" => {
                    if parts.len() < 2 {
                        println!("Usage: .config collections <col1,col2,...>");
                        return CommandResult::Continue;
                    }
                    let collections: Vec<String> =
                        parts[1].split(',').map(|s| s.trim().to_string()).collect();
                    if let Err(e) = app.update_config(|c| c.collections = collections.clone()) {
                        println!("Error: {}", e);
                    } else {
                        println!("Collection filters set to: {}", collections.join(", "));
                    }
                }
                "relay" => {
                    if parts.len() < 2 {
                        println!("Usage: .config relay <hostname>");
                        return CommandResult::Continue;
                    }
                    let relay = parts[1].to_string();
                    if let Err(e) = app.update_config(|c| c.relay = relay.clone()) {
                        println!("Error: {}", e);
                    } else {
                        println!("Relay set to {}", relay);
                    }
                }
                "indexes" => {
                    if parts.len() < 2 {
                        println!("Usage: .config indexes <col:field:type,...>");
                        return CommandResult::Continue;
                    }
                    let indexes: Vec<config::IndexConfig> = parts[1]
                        .split(',')
                        .filter_map(|s| config::IndexConfig::parse(s.trim()))
                        .collect();
                    if let Err(e) = app.update_config(|c| c.indexes = indexes.clone()) {
                        println!("Error: {}", e);
                    } else {
                        let idx_strs: Vec<_> =
                            indexes.iter().map(|i| i.to_config_string()).collect();
                        println!("Indexes set to: {}", idx_strs.join(", "));
                    }
                }
                "search" => {
                    if parts.len() < 2 {
                        println!("Usage: .config search <list|add|remove> [collection:field]");
                        return CommandResult::Continue;
                    }
                    let search_parts: Vec<&str> = parts[1].splitn(2, ' ').collect();
                    match search_parts[0] {
                        "list" => {
                            let config = app.config();
                            if config.search_fields.is_empty() {
                                println!("No search fields configured");
                            } else {
                                println!("Search fields:");
                                for sf in &config.search_fields {
                                    println!("  {}:{}", sf.collection, sf.field);
                                }
                            }
                        }
                        "add" => {
                            if search_parts.len() < 2 {
                                println!("Usage: .config search add collection:field");
                                return CommandResult::Continue;
                            }
                            let spec = search_parts[1].trim();
                            if let Some(sf) = config::SearchFieldConfig::parse(spec) {
                                if let Err(e) = app.update_config(|c| {
                                    if !c.search_fields.contains(&sf) {
                                        c.search_fields.push(sf.clone());
                                    }
                                }) {
                                    println!("Error: {}", e);
                                } else {
                                    println!("Added search field: {}", spec);
                                    println!("Note: Run .reindex search to index existing records");
                                }
                            } else {
                                println!(
                                    "Invalid format. Use: .config search add collection:field"
                                );
                            }
                        }
                        "remove" => {
                            if search_parts.len() < 2 {
                                println!("Usage: .config search remove collection:field");
                                return CommandResult::Continue;
                            }
                            let spec = search_parts[1].trim();
                            if let Some(sf) = config::SearchFieldConfig::parse(spec) {
                                if let Err(e) = app.update_config(|c| {
                                    c.search_fields.retain(|s| s != &sf);
                                }) {
                                    println!("Error: {}", e);
                                } else {
                                    println!("Removed search field: {}", spec);
                                }
                            } else {
                                println!(
                                    "Invalid format. Use: .config search remove collection:field"
                                );
                            }
                        }
                        _ => {
                            println!("Unknown search subcommand: {}", search_parts[0]);
                            println!("Usage: .config search <list|add|remove> [collection:field]");
                        }
                    }
                }
                _ => {
                    println!("Unknown config key: {}", parts[0]);
                }
            }
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".index ") => {
            let subcmd = cmd.strip_prefix(".index ").unwrap().trim();
            let parts: Vec<&str> = subcmd.split_whitespace().collect();

            match parts.first().copied() {
                Some("list") => {
                    let config = app.config();
                    if config.indexes.is_empty() {
                        println!("No indexes configured");
                    } else {
                        println!("Configured indexes:");
                        for idx in &config.indexes {
                            println!("  {}", idx.to_config_string());
                        }
                    }
                }
                Some("rebuild") => {
                    if let Some(spec) = parts.get(1) {
                        if let Some(index) = config::IndexConfig::parse(spec) {
                            println!("Rebuilding index {}...", spec);
                            match app.store.rebuild_index(&index) {
                                Ok(count) => println!("Rebuilt index with {} entries", count),
                                Err(e) => println!("Error: {}", e),
                            }
                        } else {
                            println!("Invalid index spec. Format: collection:field:type");
                            println!("Types: datetime, integer");
                        }
                    } else {
                        println!("Usage: .index rebuild <collection:field:type>");
                    }
                }
                Some("rebuild-all") => {
                    let config = app.config();
                    if config.indexes.is_empty() {
                        println!("No indexes configured");
                    } else {
                        for index in &config.indexes {
                            println!("Rebuilding {}...", index.to_config_string());
                            match app.store.rebuild_index(index) {
                                Ok(count) => println!("  {} entries", count),
                                Err(e) => println!("  Error: {}", e),
                            }
                        }
                    }
                }
                Some("rebuild-ts") => {
                    if let Some(collection) = parts.get(1) {
                        println!("Rebuilding __ts__ index for {}...", collection);
                        match app.store.rebuild_indexed_at(collection) {
                            Ok(count) => println!("Built __ts__ index with {} entries", count),
                            Err(e) => println!("Error: {}", e),
                        }
                    } else {
                        println!("Usage: .index rebuild-ts <collection>");
                    }
                }
                _ => {
                    println!("Usage:");
                    println!("  .index list               List configured indexes");
                    println!("  .index rebuild <spec>     Rebuild index (col:field:type)");
                    println!("  .index rebuild-all        Rebuild all configured indexes");
                }
            }
            CommandResult::Continue
        }

        ".reindex search" => {
            if let Some(ref search) = app.search {
                let config = app.config();
                println!("Reindexing search fields...");
                match search.reindex_from_store(&app.store, &config.search_fields) {
                    Ok(count) => println!("Reindexed {} records", count),
                    Err(e) => eprintln!("Reindex error: {}", e),
                }
            } else {
                println!("Search is not enabled");
            }
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".handles") => {
            let subcmd = cmd.strip_prefix(".handles").unwrap().trim();

            match subcmd {
                "sync" => {
                    println!("Fetching handles from PLC directory...");
                    let mut count = 0;
                    let mut errors = 0;

                    // Get all DIDs from repos
                    let dids: Vec<String> = app
                        .repos
                        .list()
                        .unwrap_or_default()
                        .iter()
                        .map(|r| r.did.clone())
                        .collect();

                    println!("Found {} DIDs to look up", dids.len());

                    for did in &dids {
                        match fetch_handle_from_plc(did) {
                            Ok(Some(handle)) => {
                                if let Err(e) = app.set_handle(did, &handle) {
                                    eprintln!("Error storing handle for {}: {}", did, e);
                                    errors += 1;
                                } else {
                                    count += 1;
                                    if count % 100 == 0 {
                                        println!("  fetched {} handles...", count);
                                    }
                                }
                            }
                            Ok(None) => {
                                // No handle found (tombstoned or missing)
                            }
                            Err(e) => {
                                eprintln!("Error fetching {}: {}", did, e);
                                errors += 1;
                            }
                        }
                        // Small delay to be nice to PLC directory
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }

                    println!("Done. Fetched {} handles, {} errors", count, errors);
                }
                _ if subcmd.starts_with("get ") => {
                    let did = subcmd.strip_prefix("get ").unwrap().trim();
                    match app.get_handle(did) {
                        Some(handle) => println!("{} -> {}", did, handle),
                        None => println!("No handle stored for {}", did),
                    }
                }
                _ if subcmd.starts_with("fetch ") => {
                    let did = subcmd.strip_prefix("fetch ").unwrap().trim();
                    match fetch_handle_from_plc(did) {
                        Ok(Some(handle)) => {
                            if let Err(e) = app.set_handle(did, &handle) {
                                println!("Error storing handle: {}", e);
                            } else {
                                println!("{} -> {}", did, handle);
                            }
                        }
                        Ok(None) => println!("No handle found for {}", did),
                        Err(e) => println!("Error: {}", e),
                    }
                }
                _ => {
                    println!("Usage: .handles sync | .handles get <did> | .handles fetch <did>");
                }
            }
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".count ") => {
            let subcmd = cmd.strip_prefix(".count ").unwrap().trim();
            let parts: Vec<&str> = subcmd.split_whitespace().collect();

            match parts.first().copied() {
                Some("rebuild") => {
                    if let Some(collection) = parts.get(1) {
                        println!("Rebuilding count for {}...", collection);
                        match app.store.rebuild_count(collection) {
                            Ok(count) => println!("Count rebuilt: {}", count),
                            Err(e) => println!("Error: {}", e),
                        }
                    } else {
                        println!("Usage: .count rebuild <collection>");
                    }
                }
                Some("rebuild-all") => match app.store.unique_collections() {
                    Ok(collections) => {
                        for collection in collections {
                            print!("Rebuilding count for {}... ", collection);
                            match app.store.rebuild_count(&collection) {
                                Ok(count) => println!("{}", count),
                                Err(e) => println!("Error: {}", e),
                            }
                        }
                    }
                    Err(e) => println!("Error: {}", e),
                },
                Some(collection) => match app.store.count_collection(collection) {
                    Ok(count) => println!("{}: {}", collection, count),
                    Err(e) => println!("Error: {}", e),
                },
                None => {
                    println!("Usage:");
                    println!("  .count <collection>       Show collection count");
                    println!("  .count rebuild <col>      Rebuild count for collection");
                    println!("  .count rebuild-all        Rebuild counts for all collections");
                }
            }
            CommandResult::Continue
        }

        ".repos" => {
            match app.repos.list() {
                Ok(repos) => {
                    if repos.is_empty() {
                        println!("No tracked repos");
                    } else {
                        println!("Tracked repos ({}):", repos.len());
                        for repo in repos {
                            println!(
                                "  {} [{}] {} records",
                                repo.did, repo.status, repo.record_count
                            );
                        }
                    }
                }
                Err(e) => println!("Error: {}", e),
            }
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".repos ") => {
            let rest = cmd.strip_prefix(".repos ").unwrap();
            let parts: Vec<&str> = rest.split_whitespace().collect();

            match parts.first().copied() {
                Some("add") => {
                    for did in &parts[1..] {
                        let state = RepoState::new(did.to_string());
                        match app.repos.put(&state) {
                            Ok(()) => println!("Added {} (pending sync)", did),
                            Err(e) => println!("Error adding {}: {}", did, e),
                        }
                    }
                }
                Some("remove") => {
                    if parts.len() < 2 {
                        println!("Usage: .repos remove <did>");
                    } else {
                        let did = parts[1];
                        match app.repos.delete(did) {
                            Ok(()) => println!("Removed {}", did),
                            Err(e) => println!("Error: {}", e),
                        }
                    }
                }
                Some("status") => {
                    if parts.len() < 2 {
                        println!("Usage: .repos status <did>");
                    } else {
                        let did = parts[1];
                        match app.repos.get(did) {
                            Ok(Some(state)) => {
                                println!("DID: {}", state.did);
                                println!("Status: {}", state.status);
                                if let Some(rev) = &state.rev {
                                    println!("Rev: {}", rev);
                                }
                                println!("Records: {}", state.record_count);
                                if let Some(ts) = state.last_sync {
                                    println!("Last sync: {}", ts);
                                }
                                if let Some(err) = &state.error {
                                    println!("Error: {}", err);
                                    println!("Retry count: {}", state.retry_count);
                                }
                            }
                            Ok(None) => println!("Repo not found"),
                            Err(e) => println!("Error: {}", e),
                        }
                    }
                }
                Some("errors") => match app.repos.list_by_status(repos::RepoStatus::Error) {
                    Ok(repos) => {
                        if repos.is_empty() {
                            println!("No errored repos");
                        } else {
                            println!("Errored repos ({}):", repos.len());
                            for repo in repos {
                                println!("  {} - {}", repo.did, repo.error.unwrap_or_default());
                            }
                        }
                    }
                    Err(e) => println!("Error: {}", e),
                },
                Some("resync") => {
                    if parts.len() < 2 {
                        println!("Usage: .repos resync <did> or .repos resync --errors");
                    } else if parts[1] == "--errors" {
                        match app.repos.list_by_status(repos::RepoStatus::Error) {
                            Ok(repos) => {
                                for mut repo in repos {
                                    repo.status = repos::RepoStatus::Pending;
                                    repo.error = None;
                                    if let Err(e) = app.repos.put(&repo) {
                                        eprintln!("Error updating {}: {}", repo.did, e);
                                    } else {
                                        println!("Queued {} for resync", repo.did);
                                    }
                                }
                            }
                            Err(e) => println!("Error: {}", e),
                        }
                    } else {
                        let did = parts[1];
                        match app.repos.get(did) {
                            Ok(Some(mut state)) => {
                                state.status = repos::RepoStatus::Pending;
                                state.error = None;
                                if let Err(e) = app.repos.put(&state) {
                                    eprintln!("Error updating {}: {}", did, e);
                                } else {
                                    println!("Queued {} for resync", did);
                                }
                            }
                            Ok(None) => println!("Repo not found"),
                            Err(e) => println!("Error: {}", e),
                        }
                    }
                }
                _ => {
                    println!("Unknown repos command. Try .repos add/remove/status/errors/resync");
                }
            }
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".connect") => {
            let relay = cmd
                .strip_prefix(".connect")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("bsky.network");

            if running.load(Ordering::Relaxed) {
                println!("Already connected. Use .disconnect first.");
                return CommandResult::Continue;
            }

            CommandResult::StartStreaming(relay.to_string())
        }

        line if line.starts_with("at://") => {
            if line.contains("?sync") {
                let uri_part = line.strip_prefix("at://").unwrap();
                let did = uri_part.split('?').next().unwrap();

                println!("Syncing {}...", did);
                let start = std::time::Instant::now();
                let store = Arc::new(app.store.clone());
                let config = app.config();
                match sync::sync_repo(
                    did,
                    &store,
                    &config.collections,
                    &config.indexes,
                    app.search.as_ref(),
                    &config.search_fields,
                ) {
                    Ok(result) => {
                        let elapsed = start.elapsed();
                        // Store handle if resolved
                        if let Some(handle) = &result.handle {
                            let _ = app.set_handle(did, handle);
                        }
                        println!("Synced {} records ({:.2?})", result.record_count, elapsed);
                    }
                    Err(e) => println!("Sync error: {}", e),
                }
                return CommandResult::Continue;
            }

            match Query::parse(line) {
                Ok(q) => {
                    let start = std::time::Instant::now();
                    match query::execute(&q, &app.store) {
                        Ok(records) => {
                            let elapsed = start.elapsed();
                            for record in &records {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&record.value).unwrap()
                                );
                                println!("---");
                            }
                            println!("({} records, {:.2?})", records.len(), elapsed);
                        }
                        Err(e) => println!("Query error: {}", e),
                    }
                }
                Err(e) => println!("Parse error: {}", e),
            }
            CommandResult::Continue
        }

        _ => {
            println!("Unknown command. Type .help for help.");
            CommandResult::Continue
        }
    }
}

fn start_streaming(relay: String, app: Arc<AppState>, running: Arc<AtomicBool>) {
    running.store(true, Ordering::Relaxed);

    thread::spawn(move || {
        // Load cursor for recovery
        let cursor = app.get_firehose_cursor(&relay);
        if let Some(seq) = cursor {
            println!("Resuming from cursor: {}", seq);
        }

        match FirehoseClient::connect(&relay, cursor) {
            Ok(mut client) => {
                println!("Connected to {}", relay);
                let mut last_seq: i64 = cursor.unwrap_or(0);
                let mut event_count: u64 = 0;

                while running.load(Ordering::Relaxed) {
                    match client.next_event() {
                        Ok(Some(Event::Commit {
                            seq, operations, ..
                        })) => {
                            last_seq = seq;
                            event_count += 1;
                            if event_count.is_multiple_of(1000) {
                                let _ = app.set_firehose_cursor(&relay, seq);
                            }
                            for op in operations {
                                match op {
                                    Operation::Create { uri, cid, value }
                                    | Operation::Update { uri, cid, value } => {
                                        let config = app.config();
                                        let record = storage::Record {
                                            uri: uri.to_string(),
                                            cid,
                                            value,
                                            indexed_at: SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap()
                                                .as_secs(),
                                        };
                                        if let Err(e) = app.store.put_with_indexes(
                                            &uri,
                                            &record,
                                            &config.indexes,
                                        ) {
                                            eprintln!("Storage error: {}", e);
                                        }
                                        // Index for search
                                        if let Some(ref search) = app.search {
                                            if let Err(e) = search.index_record(
                                                &uri.to_string(),
                                                uri.collection.as_str(),
                                                &record.value,
                                                &config.search_fields,
                                            ) {
                                                eprintln!("Search index error: {}", e);
                                            }
                                        }
                                    }
                                    Operation::Delete { uri } => {
                                        let config = app.config();
                                        if let Err(e) =
                                            app.store.delete_with_indexes(&uri, &config.indexes)
                                        {
                                            eprintln!("Storage error: {}", e);
                                        }
                                        // Remove from search index
                                        if let Some(ref search) = app.search {
                                            if let Err(e) = search.delete_record(&uri.to_string()) {
                                                eprintln!("Search delete error: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Some(Event::Identity { seq, did, handle })) => {
                            last_seq = seq;
                            if let Some(h) = handle {
                                if let Err(e) = app.set_handle(did.as_str(), &h) {
                                    eprintln!("Handle update error: {}", e);
                                }
                            }
                        }
                        Ok(Some(Event::Unknown { .. })) => {}
                        Ok(None) => {
                            // Timeout - continue to check running flag
                            continue;
                        }
                        Err(e) => {
                            eprintln!("Firehose error: {}", e);
                            break;
                        }
                    }
                }

                // Save cursor on exit
                if last_seq > 0 {
                    let _ = app.set_firehose_cursor(&relay, last_seq);
                    println!("Saved cursor: {}", last_seq);
                }
            }
            Err(e) => {
                eprintln!("Failed to connect: {}", e);
            }
        }
        running.store(false, Ordering::Relaxed);
    });
}

fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atpdb=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args: Vec<String> = std::env::args().collect();

    // Check for serve subcommand
    if args.len() > 1 && args[1] == "serve" {
        let mut port: u16 = 3000;
        // Check env var first, then command line can override
        let mut relay: Option<String> = std::env::var("ATPDB_RELAY").ok();

        for arg in &args[2..] {
            if let Some(p) = arg.strip_prefix("--port=") {
                port = p.parse().unwrap_or(3000);
            } else if arg == "--connect" {
                relay = Some("bsky.network".to_string());
            } else if let Some(r) = arg.strip_prefix("--connect=") {
                relay = Some(r.to_string());
            }
        }

        let app = match AppState::open(Path::new("./atpdb.data")) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                eprintln!("Failed to open storage: {}", e);
                return;
            }
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(server::run(app, port, relay));
        return;
    }

    // Non-interactive mode
    if args.len() > 1 {
        let app = match AppState::open(Path::new("./atpdb.data")) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                eprintln!("Failed to open storage: {}", e);
                return;
            }
        };
        let running = Arc::new(AtomicBool::new(false));

        let command = args[1..].join(" ");
        handle_command(&command, &app, &running);
        return;
    }

    // Interactive mode
    let app = match AppState::open(Path::new("./atpdb.data")) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("Failed to open storage: {}", e);
            return;
        }
    };

    println!("ATPDB version {}", env!("CARGO_PKG_VERSION"));
    println!("Enter \".help\" for usage hints.");
    println!("Example: at://*/app.bsky.feed.post/*");
    println!();

    let running = Arc::new(AtomicBool::new(false));
    let mut rl = DefaultEditor::new().unwrap();

    loop {
        let prompt = if running.load(Ordering::Relaxed) {
            "atp> (streaming) "
        } else {
            "atp> "
        };

        match rl.readline(prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let _ = rl.add_history_entry(line);

                match handle_command(line, &app, &running) {
                    CommandResult::Continue => {}
                    CommandResult::Exit => break,
                    CommandResult::StartStreaming(relay) => {
                        start_streaming(relay, Arc::clone(&app), Arc::clone(&running));
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("^C");
            }
            Err(ReadlineError::Eof) => {
                break;
            }
            Err(e) => {
                println!("Error: {:?}", e);
                break;
            }
        }
    }

    running.store(false, Ordering::Relaxed);
    println!("Goodbye!");
}
