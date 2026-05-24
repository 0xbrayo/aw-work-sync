use anyhow::Result;
use aw_models::Event;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct CacheEntry {
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
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
    #[serde(serialize_with = "serialize_entries_descending")]
    entries: HashMap<String, CacheEntry>,
}

fn serialize_entries_descending<S>(
    entries: &HashMap<String, CacheEntry>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    let mut sorted: Vec<_> = entries.iter().collect();
    sorted.sort_by(|a, b| b.0.cmp(a.0)); // Sort descending by key (newer first)

    let mut map = serializer.serialize_map(Some(sorted.len()))?;
    for (k, v) in sorted {
        map.serialize_entry(k, v)?;
    }
    map.end()
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
        let mut store = if path.exists() {
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
            {
                Some(s) => s,
                None => {
                    log::warn!("Warning: cache file corrupt or unreadable, starting fresh.");
                    CacheStore::default()
                }
            }
        } else {
            CacheStore::default()
        };

        let mut migrated = false;
        for entry in store.entries.values_mut() {
            if entry.duration.is_none() {
                if let Some(res) = &entry.result {
                    let events: Vec<Event> =
                        serde_json::from_value(res["events"].clone()).unwrap_or_default();
                    let secs =
                        crate::working_hours::generous_approx(&events, Duration::seconds(10 * 60))
                            .num_seconds() as f64;
                    entry.duration = Some(secs);
                    migrated = true;
                }
            }
            if entry.result.is_some() {
                entry.result = None;
                migrated = true;
            }
        }

        let mut cache = Self { path, store };
        cache.evict_expired(Utc::now());
        if migrated {
            if let Err(e) = cache.save() {
                log::warn!("Failed to save migrated cache: {}", e);
            }
        }
        Ok(cache)
    }

    fn evict_expired(&mut self, now: DateTime<Utc>) {
        self.store.entries.retain(|_, entry| !entry.is_expired(now));
    }

    pub fn get(&self, key: &str) -> Option<&CacheEntry> {
        self.store.entries.get(key)
    }

    pub fn insert(&mut self, key: String, duration: f64, ttl_secs: i64) {
        self.store.entries.insert(
            key,
            CacheEntry {
                duration: Some(duration),
                result: None,
                cached_at: Utc::now(),
                ttl_secs,
            },
        );
    }

    pub fn save(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.store)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn test_migrate_and_sort_cache() {
        if let Some(config_dir) = crate::logging::get_config_dir() {
            let host = crate::hostname();
            let path = config_dir.join(format!("query_cache_{}.json", host));
            if path.exists() {
                let cache = QueryCache::load(path).unwrap();
                cache.save().unwrap();
                println!("Saved cache sorted!");
            }
        }
    }
}
