use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct CacheEntry {
    pub result: serde_json::Value,
    pub cached_at: DateTime<Utc>,
    pub ttl_secs: i64,
}

impl CacheEntry {
    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        (now - self.cached_at).num_seconds() > self.ttl_secs
    }
}

#[derive(Serialize, Deserialize, Default)]
struct CacheStore {
    entries: HashMap<String, CacheEntry>,
}

pub struct QueryCache {
    path: PathBuf,
    store: CacheStore,
}

pub fn cache_key(start: &DateTime<Utc>, end: &DateTime<Utc>) -> String {
    format!("{}|{}", start.to_rfc3339(), end.to_rfc3339())
}

impl QueryCache {
    pub fn load(path: PathBuf) -> Result<Self> {
        let store = if path.exists() {
            match std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
                Some(s) => s,
                None => {
                    eprintln!("Warning: cache file corrupt or unreadable, starting fresh.");
                    CacheStore::default()
                }
            }
        } else {
            CacheStore::default()
        };

        let mut cache = Self { path, store };
        cache.evict_expired(Utc::now());
        Ok(cache)
    }

    fn evict_expired(&mut self, now: DateTime<Utc>) {
        self.store.entries.retain(|_, entry| !entry.is_expired(now));
    }

    pub fn get(&self, key: &str) -> Option<&CacheEntry> {
        self.store.entries.get(key)
    }

    pub fn insert(&mut self, key: String, result: serde_json::Value, ttl_secs: i64) {
        self.store.entries.insert(key, CacheEntry { result, cached_at: Utc::now(), ttl_secs });
    }

    pub fn save(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.store)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}
