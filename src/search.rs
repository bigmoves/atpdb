use metrics::{counter, histogram};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, TEXT,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::{Index, IndexWriter, ReloadPolicy, TantivyDocument, Term};
use thiserror::Error;

use crate::config::SearchFieldConfig;

const NGRAM_TOKENIZER: &str = "ngram3";

#[derive(Error, Debug)]
pub enum SearchError {
    #[error("tantivy error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("query parse error: {0}")]
    QueryParse(#[from] tantivy::query::QueryParserError),
}

/// Manages Tantivy search indexes for configured collections
#[derive(Clone)]
pub struct SearchIndex {
    index: Index,
    path: PathBuf,
    writer: Arc<RwLock<IndexWriter>>,
    uri_field: Field,
    collection_field: Field,
    content_field: Field,
    field_name_field: Field,
    // Commit batching
    pending_count: Arc<RwLock<usize>>,
    last_commit: Arc<RwLock<Instant>>,
}

const COMMIT_BATCH_SIZE: usize = 1000;
const COMMIT_INTERVAL: Duration = Duration::from_secs(5);

impl SearchIndex {
    /// Open or create a search index at the given path
    pub fn open(path: &Path) -> Result<Self, SearchError> {
        let schema = Self::build_schema();

        // Check if this is an existing Tantivy index by looking for meta.json
        let meta_path = path.join("meta.json");
        let index = if meta_path.exists() {
            Index::open_in_dir(path)?
        } else {
            std::fs::create_dir_all(path).map_err(|e| {
                tantivy::TantivyError::SystemError(format!("Failed to create index dir: {}", e))
            })?;
            Index::create_in_dir(path, schema.clone())?
        };

        // Register the n-gram tokenizer for fuzzy matching
        let ngram_tokenizer = TextAnalyzer::builder(NgramTokenizer::all_ngrams(3, 3).unwrap())
            .filter(LowerCaser)
            .build();
        index
            .tokenizers()
            .register(NGRAM_TOKENIZER, ngram_tokenizer);

        let writer = index.writer(50_000_000)?; // 50MB heap

        let uri_field = schema.get_field("uri").unwrap();
        let collection_field = schema.get_field("collection").unwrap();
        let content_field = schema.get_field("content").unwrap();
        let field_name_field = schema.get_field("field_name").unwrap();

        Ok(SearchIndex {
            index,
            path: path.to_path_buf(),
            writer: Arc::new(RwLock::new(writer)),
            uri_field,
            collection_field,
            content_field,
            field_name_field,
            pending_count: Arc::new(RwLock::new(0)),
            last_commit: Arc::new(RwLock::new(Instant::now())),
        })
    }

    fn build_schema() -> Schema {
        let mut schema_builder = Schema::builder();
        // URI is stored for retrieval, not indexed for search
        schema_builder.add_text_field("uri", STORED);
        // Collection for filtering
        schema_builder.add_text_field("collection", TEXT | STORED);
        // The actual searchable content - uses n-gram tokenizer for fuzzy matching
        let content_indexing = TextFieldIndexing::default()
            .set_tokenizer(NGRAM_TOKENIZER)
            .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions);
        let content_options = TextOptions::default().set_indexing_options(content_indexing);
        schema_builder.add_text_field("content", content_options);
        // Which field this content came from (for field-specific search)
        schema_builder.add_text_field("field_name", TEXT | STORED);
        schema_builder.build()
    }

    /// Index a record's searchable fields
    pub fn index_record(
        &self,
        uri: &str,
        collection: &str,
        value: &serde_json::Value,
        search_fields: &[SearchFieldConfig],
    ) -> Result<(), SearchError> {
        let fields_for_collection: Vec<_> = search_fields
            .iter()
            .filter(|sf| sf.collection == collection)
            .collect();

        if fields_for_collection.is_empty() {
            return Ok(()); // No search fields configured for this collection
        }

        let writer = self.writer.write().unwrap();

        for sf in fields_for_collection {
            if let Some(text) = extract_text_field(value, &sf.field) {
                // Normalize: lowercase and strip non-alphanumeric for consistent trigram matching
                let normalized: String = text
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();

                let mut doc = TantivyDocument::new();
                doc.add_text(self.uri_field, uri);
                doc.add_text(self.collection_field, collection);
                doc.add_text(self.content_field, &normalized);
                doc.add_text(self.field_name_field, &sf.field);
                writer.add_document(doc)?;
            }
        }

        drop(writer);
        self.maybe_commit()?;
        Ok(())
    }

    /// Delete all documents for a URI
    pub fn delete_record(&self, uri: &str) -> Result<(), SearchError> {
        let writer = self.writer.write().unwrap();
        let term = tantivy::Term::from_field_text(self.uri_field, uri);
        writer.delete_term(term);
        drop(writer);
        self.maybe_commit()?;
        Ok(())
    }

    /// Check if we should commit based on batch size or time
    fn maybe_commit(&self) -> Result<(), SearchError> {
        let mut count = self.pending_count.write().unwrap();
        *count += 1;

        let last = *self.last_commit.read().unwrap();
        let should_commit = *count >= COMMIT_BATCH_SIZE || last.elapsed() >= COMMIT_INTERVAL;

        if should_commit {
            let mut writer = self.writer.write().unwrap();
            writer.commit()?;
            *count = 0;
            *self.last_commit.write().unwrap() = Instant::now();
        }

        Ok(())
    }

    /// Force a commit (useful for shutdown)
    pub fn commit(&self) -> Result<(), SearchError> {
        let mut writer = self.writer.write().unwrap();
        writer.commit()?;
        *self.pending_count.write().unwrap() = 0;
        *self.last_commit.write().unwrap() = Instant::now();
        Ok(())
    }

    /// Returns the disk space usage of the search index in bytes
    pub fn disk_space(&self) -> u64 {
        fn dir_size(path: &Path) -> u64 {
            let mut size = 0;
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        size += dir_size(&path);
                    } else if let Ok(meta) = entry.metadata() {
                        size += meta.len();
                    }
                }
            }
            size
        }
        dir_size(&self.path)
    }

    /// Search for records matching a query in a specific field
    /// Uses trigram matching for fuzzy search
    pub fn search(
        &self,
        collection: &str,
        field: &str,
        query_text: &str,
        limit: usize,
        _fuzzy_distance: u8, // Kept for API compatibility, trigrams handle fuzziness
    ) -> Result<Vec<String>, SearchError> {
        let start = Instant::now();
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        // Helper to split on dots and non-alphanumeric chars (matching default tokenizer)
        fn tokenize(s: &str) -> impl Iterator<Item = &str> {
            s.split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
        }

        // Generate trigrams from text (only for strings with 3+ chars)
        // Strips spaces/punctuation for normalized matching ("oh sees" = "ohsees")
        fn trigrams(s: &str) -> Vec<String> {
            let normalized: String = s
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect();
            let chars: Vec<char> = normalized.chars().collect();
            if chars.len() < 3 {
                // Too short for trigrams - skip
                return vec![];
            }
            chars
                .windows(3)
                .map(|w| w.iter().collect::<String>())
                .collect()
        }

        // Build filter queries - require all tokens to match (TEXT fields are tokenized)
        let collection_queries: Vec<(Occur, Box<dyn Query>)> = tokenize(collection)
            .map(|token| {
                let term = Term::from_field_text(self.collection_field, &token.to_lowercase());
                (
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>,
                )
            })
            .collect();

        let field_queries: Vec<(Occur, Box<dyn Query>)> = tokenize(field)
            .map(|token| {
                let term = Term::from_field_text(self.field_name_field, &token.to_lowercase());
                (
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>,
                )
            })
            .collect();

        // Build content query using trigrams
        // Require at least half the trigrams to match (prevents loose matches like "Geese" for "osees")
        let mut all_trigrams: Vec<String> = Vec::new();
        for word in query_text.split_whitespace() {
            all_trigrams.extend(trigrams(word));
        }

        if all_trigrams.is_empty() {
            return Ok(Vec::new());
        }

        // Deduplicate trigrams while preserving order
        let mut seen = std::collections::HashSet::new();
        all_trigrams.retain(|tg| seen.insert(tg.clone()));

        // Require at least half (rounded up) of trigrams to match
        let min_required = all_trigrams.len().div_ceil(2);

        let content_query: Box<dyn Query> = if all_trigrams.len() == 1 {
            // Single trigram - must match
            let term = Term::from_field_text(self.content_field, &all_trigrams[0]);
            Box::new(TermQuery::new(term, IndexRecordOption::Basic))
        } else {
            // Split into required (first half) and optional (rest)
            // Required trigrams ensure minimum similarity
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

            for (i, tg) in all_trigrams.iter().enumerate() {
                let term = Term::from_field_text(self.content_field, tg);
                let query =
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>;

                if i < min_required {
                    clauses.push((Occur::Must, query));
                } else {
                    clauses.push((Occur::Should, query));
                }
            }

            Box::new(BooleanQuery::new(clauses))
        };

        // Build final query: all collection tokens AND all field tokens AND content
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        clauses.extend(collection_queries);
        clauses.extend(field_queries);
        clauses.push((Occur::Must, content_query));

        let query = BooleanQuery::new(clauses);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        let mut uris = Vec::with_capacity(top_docs.len());
        for (_score, doc_address) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_address)?;
            if let Some(uri) = doc.get_first(self.uri_field).and_then(|v| v.as_str()) {
                uris.push(uri.to_string());
            }
        }

        counter!("search_queries_total").increment(1);
        histogram!("search_query_seconds").record(start.elapsed().as_secs_f64());

        Ok(uris)
    }

    /// Reindex all records from the store for configured search fields
    pub fn reindex_from_store(
        &self,
        store: &crate::storage::Store,
        search_fields: &[SearchFieldConfig],
    ) -> Result<usize, SearchError> {
        let mut count = 0;
        let mut errors = 0;

        // Get unique collections from search fields
        let collections: std::collections::HashSet<_> = search_fields
            .iter()
            .map(|sf| sf.collection.as_str())
            .collect();

        for collection in collections {
            eprintln!("Indexing collection: {}", collection);
            let collection_nsid: crate::types::Nsid = match collection.parse() {
                Ok(n) => n,
                Err(_) => continue,
            };

            // Get all records at once - no pagination needed for reindex
            let records = match store.scan_all_collection(&collection_nsid) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Error scanning {}: {}", collection, e);
                    errors += 1;
                    continue;
                }
            };

            eprintln!("  found {} records to index", records.len());

            for record in records {
                if let Err(e) =
                    self.index_record(&record.uri, collection, &record.value, search_fields)
                {
                    eprintln!("Error indexing {}: {}", record.uri, e);
                    errors += 1;
                    continue;
                }
                count += 1;
                counter!("search_reindex_records_total").increment(1);

                if count % 50000 == 0 {
                    eprintln!("  indexed {} records...", count);
                }
            }
        }

        // Force commit after reindex
        self.commit()?;

        if errors > 0 {
            eprintln!("Completed with {} errors", errors);
        }

        Ok(count)
    }
}

/// Extract text from a JSON value at a dot-separated field path
/// Supports array indexing: "artists.0.artistName" gets artists[0].artistName
/// Also supports wildcard array access: "artists.*.artistName" joins all artist names
fn extract_text_field(value: &serde_json::Value, field_path: &str) -> Option<String> {
    let parts: Vec<&str> = field_path.split('.').collect();
    extract_text_recursive(value, &parts)
}

fn extract_text_recursive(value: &serde_json::Value, parts: &[&str]) -> Option<String> {
    if parts.is_empty() {
        return value.as_str().map(|s| s.to_string());
    }

    let part = parts[0];
    let rest = &parts[1..];

    // Handle wildcard array access - join all matching values
    if part == "*" {
        if let Some(arr) = value.as_array() {
            let texts: Vec<String> = arr
                .iter()
                .filter_map(|item| extract_text_recursive(item, rest))
                .collect();
            if texts.is_empty() {
                return None;
            }
            return Some(texts.join(" "));
        }
        return None;
    }

    // Try numeric index for arrays
    if let Ok(idx) = part.parse::<usize>() {
        if let Some(arr) = value.as_array() {
            return arr.get(idx).and_then(|v| extract_text_recursive(v, rest));
        }
    }

    // Standard object field access
    value
        .get(part)
        .and_then(|v| extract_text_recursive(v, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_search_index_create() {
        let dir = tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        assert!(index.commit().is_ok());
    }

    #[test]
    fn test_extract_text_field() {
        let value = serde_json::json!({
            "text": "hello world",
            "nested": {
                "field": "nested value"
            },
            "artists": [
                {"artistName": "Good Kid"},
                {"artistName": "Bad Kid"}
            ]
        });

        assert_eq!(
            extract_text_field(&value, "text"),
            Some("hello world".to_string())
        );
        assert_eq!(
            extract_text_field(&value, "nested.field"),
            Some("nested value".to_string())
        );
        assert_eq!(extract_text_field(&value, "missing"), None);

        // Array index access
        assert_eq!(
            extract_text_field(&value, "artists.0.artistName"),
            Some("Good Kid".to_string())
        );
        assert_eq!(
            extract_text_field(&value, "artists.1.artistName"),
            Some("Bad Kid".to_string())
        );

        // Wildcard array access - joins all values
        assert_eq!(
            extract_text_field(&value, "artists.*.artistName"),
            Some("Good Kid Bad Kid".to_string())
        );
    }
}
