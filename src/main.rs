mod firehose;
mod indexer;
mod query;
mod storage;
mod types;

use firehose::{Event, FirehoseClient, Operation};
use query::Query;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

enum CommandResult {
    Continue,
    Exit,
    StartStreaming(String),
}

fn handle_command(
    line: &str,
    store: &Arc<storage::Store>,
    running: &Arc<AtomicBool>,
) -> CommandResult {
    match line {
        ".quit" | ".exit" => CommandResult::Exit,

        ".help" => {
            println!("Commands:");
            println!("  .connect <relay>  Connect to firehose (default: bsky.network)");
            println!("  .disconnect       Stop firehose");
            println!("  .stats            Show statistics");
            println!("  .dids             List unique DIDs");
            println!("  .collections      List unique collections");
            println!("  .quit             Exit");
            println!();
            println!("Queries:");
            println!("  at://did/collection/rkey   Get single record");
            println!("  at://did/collection/*      Get all records in collection");
            CommandResult::Continue
        }

        ".stats" => {
            match store.count() {
                Ok(count) => println!("Records: {}", count),
                Err(e) => println!("Error: {}", e),
            }
            CommandResult::Continue
        }

        ".dids" => {
            match store.unique_dids() {
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
            match store.unique_collections() {
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

        ".disconnect" => {
            running.store(false, Ordering::Relaxed);
            println!("Disconnecting...");
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
            match Query::parse(line) {
                Ok(q) => {
                    let start = std::time::Instant::now();
                    match query::execute(&q, store) {
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

fn start_streaming(relay: String, store: Arc<storage::Store>, running: Arc<AtomicBool>) {
    running.store(true, Ordering::Relaxed);

    thread::spawn(move || {
        match FirehoseClient::connect(&relay) {
            Ok(mut client) => {
                println!("Connected to {}", relay);

                while running.load(Ordering::Relaxed) {
                    match client.next_event() {
                        Ok(Some(Event::Commit { did: _, operations })) => {
                            for op in operations {
                                match op {
                                    Operation::Create { uri, cid, value } => {
                                        let record = storage::Record {
                                            uri: uri.to_string(),
                                            cid,
                                            value,
                                            indexed_at: SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap()
                                                .as_secs(),
                                        };
                                        if let Err(e) = store.put(&uri, &record) {
                                            eprintln!("Storage error: {}", e);
                                        }
                                    }
                                    Operation::Delete { uri } => {
                                        if let Err(e) = store.delete(&uri) {
                                            eprintln!("Storage error: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Some(Event::Unknown)) => {}
                        Ok(None) => {
                            println!("Connection closed");
                            break;
                        }
                        Err(e) => {
                            eprintln!("Firehose error: {}", e);
                            break;
                        }
                    }
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
    let args: Vec<String> = std::env::args().collect();

    // Non-interactive mode: run command from args
    if args.len() > 1 {
        let store = match storage::Store::open(Path::new("./atpdb.data")) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("Failed to open storage: {}", e);
                return;
            }
        };
        let running = Arc::new(AtomicBool::new(false));

        let command = args[1..].join(" ");
        handle_command(&command, &store, &running);
        return;
    }

    // Interactive mode
    println!(
        r#"
   ___  ______ ___  ___  ___
  / _ |/_  __// _ \/ _ \/ _ )
 / __ | / /  / ___/ // / _  |
/_/ |_|/_/  /_/  /____/____/
"#
    );

    let store = match storage::Store::open(Path::new("./atpdb.data")) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("Failed to open storage: {}", e);
            return;
        }
    };

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

                match handle_command(line, &store, &running) {
                    CommandResult::Continue => {}
                    CommandResult::Exit => break,
                    CommandResult::StartStreaming(relay) => {
                        start_streaming(relay, Arc::clone(&store), Arc::clone(&running));
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
