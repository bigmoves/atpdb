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

fn main() {
    println!(r#"
   ___  ______ ___  ___  ___
  / _ |/_  __// _ \/ _ \/ _ )
 / __ | / /  / ___/ // / _  |
/_/ |_|/_/  /_/  /____/____/
"#);

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

                match line {
                    ".quit" | ".exit" => break,

                    ".help" => {
                        println!("Commands:");
                        println!("  .connect <relay>  Connect to firehose (default: bsky.network)");
                        println!("  .disconnect       Stop firehose");
                        println!("  .stats            Show statistics");
                        println!("  .quit             Exit");
                        println!();
                        println!("Queries:");
                        println!("  at://did/collection/rkey   Get single record");
                        println!("  at://did/collection/*      Get all records in collection");
                    }

                    ".stats" => {
                        match store.count() {
                            Ok(count) => println!("Records: {}", count),
                            Err(e) => println!("Error: {}", e),
                        }
                    }

                    ".disconnect" => {
                        running.store(false, Ordering::Relaxed);
                        println!("Disconnecting...");
                    }

                    cmd if cmd.starts_with(".connect") => {
                        let relay = cmd
                            .strip_prefix(".connect")
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .unwrap_or("bsky.network");

                        if running.load(Ordering::Relaxed) {
                            println!("Already connected. Use .disconnect first.");
                            continue;
                        }

                        let store_clone = Arc::clone(&store);
                        let running_clone = Arc::clone(&running);
                        let relay = relay.to_string();

                        running.store(true, Ordering::Relaxed);

                        thread::spawn(move || {
                            match FirehoseClient::connect(&relay) {
                                Ok(mut client) => {
                                    println!("Connected to {}", relay);

                                    while running_clone.load(Ordering::Relaxed) {
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
                                                            if let Err(e) = store_clone.put(&uri, &record) {
                                                                eprintln!("Storage error: {}", e);
                                                            }
                                                        }
                                                        Operation::Delete { uri } => {
                                                            if let Err(e) = store_clone.delete(&uri) {
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
                            running_clone.store(false, Ordering::Relaxed);
                        });
                    }

                    line if line.starts_with("at://") => {
                        match Query::parse(line) {
                            Ok(q) => {
                                let start = std::time::Instant::now();
                                match query::execute(&q, &store) {
                                    Ok(records) => {
                                        let elapsed = start.elapsed();
                                        for record in &records {
                                            println!("{}", serde_json::to_string_pretty(&record.value).unwrap());
                                            println!("---");
                                        }
                                        println!("({} records, {:.2?})", records.len(), elapsed);
                                    }
                                    Err(e) => println!("Query error: {}", e),
                                }
                            }
                            Err(e) => println!("Parse error: {}", e),
                        }
                    }

                    _ => println!("Unknown command. Type .help for help."),
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
