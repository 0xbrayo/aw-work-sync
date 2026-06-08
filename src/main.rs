mod cache;
mod excel_local;
mod excel_online;
mod google_sheets;
mod logging;
mod provider;
mod working_hours;

use anyhow::{Context, Result};
use aw_client_rust::{
    classes::CategorySpec,
    queries::{DesktopQueryParams, QueryParams, QueryParamsBase},
    AwClient,
};
use aw_models::Event;
use cache::{cache_key, QueryCache};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use log::{debug, error, info, warn};
use provider::{SheetProvider, SheetWrite};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration as StdDuration;

const BREAK_TIME_SECS: i64 = 10 * 60;
const DAYS_BACK_ON_NEW: i64 = 30;
const DAY_START_HOUR: u32 = 4;
const CACHE_TTL_TODAY_HOURS_SECS: i64 = 24 * 3600;
const CACHE_TTL_PAST_DAYS_SECS: i64 = 3 * 24 * 3600;
// aw-client flushes events every 10s (commit_interval); 30s gives 3× margin for late-landing events.
const CACHE_GRACE_SECS: i64 = 30;
const CONFIG_GUIDANCE: &str = "Please configure your sheets by editing the 'sheets' list in the config file.\nEach entry requires 'sheet_key' and 'regex' fields. Example:\n  sheets:\n    - sheet_key: \"your-sheet-key\"\n      regex: \"your-regex\"\nNote: if top-level 'sheet_key'/'regex' fields are also present, 'sheets[0]' takes precedence and they will be overwritten.";

enum ProviderType {
    Google,
    ExcelLocal,
    ExcelOnline,
}

fn infer_provider(sheet_key: &str) -> ProviderType {
    let has_xls_ext = |ext: &str| {
        sheet_key.len() > ext.len()
            && sheet_key[sheet_key.len() - ext.len()..].eq_ignore_ascii_case(ext)
    };
    if has_xls_ext(".xlsx") || has_xls_ext(".xls") {
        ProviderType::ExcelLocal
    } else if sheet_key.contains('/') {
        ProviderType::ExcelOnline
    } else {
        ProviderType::Google
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct SheetConfig {
    sheet_key: String,
    regex: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    sheet_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    regex: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sheets: Vec<SheetConfig>,
}

impl FileConfig {
    fn normalize(mut self) -> Self {
        if self.sheets.is_empty() {
            if let (Some(sk), Some(rx)) = (self.sheet_key.take(), self.regex.take()) {
                self.sheets.push(SheetConfig {
                    sheet_key: sk,
                    regex: rx,
                });
            }
        }
        if !self.sheets.is_empty() {
            let first_sheet = &self.sheets[0];
            if let (Some(sheet_key), Some(regex)) = (self.sheet_key.as_ref(), self.regex.as_ref()) {
                if sheet_key != &first_sheet.sheet_key || regex != &first_sheet.regex {
                    warn!(
                        "config contains conflicting legacy sheet_key/regex fields and sheets[0]; using sheets[0] during normalization"
                    );
                }
            }
            self.sheet_key = Some(first_sheet.sheet_key.clone());
            self.regex = Some(first_sheet.regex.clone());
        }
        self
    }
}

struct Config {
    sheets: Vec<SheetConfig>,
    interval: StdDuration,
    verbose: bool,
    testing: bool,
}

fn config_dir() -> PathBuf {
    logging::get_config_dir().expect("cannot find config dir")
}

fn config_path() -> PathBuf {
    logging::get_config_path().expect("cannot find config path")
}

fn cache_path(host: &str, sheet_key: &str) -> PathBuf {
    let sanitized_key: String = sheet_key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let encoded_key = sheet_key
        .as_bytes()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join("");
    let cache_key = if sanitized_key.is_empty() {
        format!("hex_{}", encoded_key)
    } else {
        format!("{}_{}", sanitized_key, encoded_key)
    };
    config_dir().join(format!("query_cache_{}_{}.json", host, cache_key))
}

fn backup_path(base: &std::path::Path, n: u8) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(format!(".{}", n));
    PathBuf::from(s)
}

/// Rotate existing backups (.3→.4, .2→.3, .1→.2, current→.1), dropping .4 if present.
fn rotate_backups(path: &std::path::Path) -> Result<()> {
    if backup_path(path, 4).exists() {
        std::fs::remove_file(backup_path(path, 4))?;
    }
    for n in (1..=3).rev() {
        let from = backup_path(path, n);
        if from.exists() {
            std::fs::rename(&from, backup_path(path, n + 1))?;
        }
    }
    std::fs::rename(path, backup_path(path, 1))?;
    Ok(())
}

fn is_placeholder(cfg: &FileConfig) -> bool {
    if !cfg.sheets.is_empty() {
        return cfg
            .sheets
            .iter()
            .any(|s| s.sheet_key == "your-sheet-key" || s.regex == "your-work-regex");
    }
    // Only flag as placeholder when the default sentinel values are present;
    // missing fields indicate an invalid config, not a placeholder.
    matches!(&cfg.sheet_key, Some(sk) if sk == "your-sheet-key")
        || matches!(&cfg.regex, Some(rx) if rx == "your-work-regex")
}

/// Returns a human-readable reason if the config is incomplete, or `None` if valid.
fn config_validation_error(cfg: &FileConfig) -> Option<String> {
    if cfg.sheets.is_empty() {
        return Some("no sheets configured".into());
    }
    None
}

fn write_config(path: &std::path::Path, sheet_key: &str, regex: &str) -> Result<()> {
    let cfg = FileConfig {
        sheet_key: Some(sheet_key.to_string()),
        regex: Some(regex.to_string()),
        sheets: vec![SheetConfig {
            sheet_key: sheet_key.to_string(),
            regex: regex.to_string(),
        }],
    };
    std::fs::write(path, serde_yaml::to_string(&cfg)?).context("failed to write config")?;
    info!("Config written to {}", path.display());
    Ok(())
}

/// Load config when no CLI values are given; exits with guidance if not set up.
fn load_file_config() -> Result<FileConfig> {
    let path = config_path();
    if !path.exists() {
        write_config(&path, "your-sheet-key", "your-work-regex")?;
        warn!("Created default config at {}", path.display());
        warn!("{}. Then re-run.", CONFIG_GUIDANCE);
        std::process::exit(1);
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let cfg: FileConfig = serde_yaml::from_str(&contents).context("failed to parse config.yaml")?;
    let cfg = cfg.normalize();
    if is_placeholder(&cfg) {
        error!("Config at {} still has placeholder values.", path.display());
        error!("{}.", CONFIG_GUIDANCE);
        std::process::exit(1);
    }
    if let Some(reason) = config_validation_error(&cfg) {
        error!("Config at {} is invalid: {}.", path.display(), reason);
        error!("{}.", CONFIG_GUIDANCE);
        std::process::exit(1);
    }
    Ok(cfg)
}

/// Apply CLI-provided values to config, rotating backups if the values changed.
fn apply_cli_to_config(sheet_key: &str, regex: &str) -> Result<()> {
    let path = config_path();
    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        let existing: FileConfig = serde_yaml::from_str(&contents)?;
        let existing = existing.normalize();
        if !is_placeholder(&existing) {
            if existing.sheets.len() > 1 {
                anyhow::bail!(
                    "Config has multiple sheets; positional CLI args cannot overwrite a multi-sheet config. \
                     Please edit the config file directly: {}",
                    path.display()
                );
            }
            if existing.sheets.len() == 1
                && existing.sheets[0].sheet_key == sheet_key
                && existing.sheets[0].regex == regex
            {
                return Ok(()); // identical — nothing to do
            }
            rotate_backups(&path)?;
            info!(
                "Previous config backed up to {}",
                backup_path(&path, 1).display()
            );
        }
    }
    write_config(&path, sheet_key, regex)
}

fn parse_duration(s: &str) -> Result<StdDuration> {
    let (num_str, unit) = s
        .find(|c: char| c.is_alphabetic())
        .map(|i| s.split_at(i))
        .with_context(|| format!("invalid duration '{}': expected e.g. 5m, 2h, 30s", s))?;
    let n: u64 = num_str
        .parse()
        .with_context(|| format!("invalid duration '{}': not a number", s))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => anyhow::bail!("unknown unit '{}' in '{}': use s, m or h", unit, s),
    };
    Ok(StdDuration::from_secs(secs))
}

fn parse_args() -> Result<Config> {
    let args: Vec<String> = std::env::args().collect();
    let mut interval = parse_duration("5m")?;
    let mut verbose = false;
    let mut testing = false;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--interval" => {
                let val = args
                    .get(i + 1)
                    .with_context(|| "--interval requires a value")?;
                interval = parse_duration(val)?;
                i += 2;
            }
            "--verbose" => {
                verbose = true;
                i += 1;
            }
            "--testing" => {
                testing = true;
                i += 1;
            }
            s if !s.starts_with("--") => {
                positional.push(s.to_string());
                i += 1;
            }
            other => {
                eprintln!("Unknown argument: {}", other);
                eprintln!("Usage: aw-work-sync [sheet_key regex] [--interval <5m|2h|30s>] [--verbose] [--testing]");
                std::process::exit(1);
            }
        }
    }

    // Setup logging using ActivityWatch conventions early, so load_file_config / apply_cli_to_config
    // logging will actually be captured.
    logging::setup_logger("aw-work-sync", testing, verbose)
        .map_err(|e| anyhow::anyhow!("Failed to setup logging: {}", e))?;

    let sheets = match positional.len() {
        0 => {
            let f = load_file_config()?;
            f.sheets
        }
        2 => {
            apply_cli_to_config(&positional[0], &positional[1])?;
            vec![SheetConfig {
                sheet_key: positional[0].clone(),
                regex: positional[1].clone(),
            }]
        }
        _ => {
            eprintln!("Provide both sheet_key and regex, or neither.");
            eprintln!("Usage: aw-work-sync [sheet_key regex] [--interval <5m|2h|30s>] [--verbose] [--testing]");
            std::process::exit(1);
        }
    };

    Ok(Config {
        sheets,
        interval,
        verbose,
        testing,
    })
}

fn hostname() -> String {
    Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
        .replace(".localdomain", "")
        .replace(".local", "")
}

/// Split today into completed 1-hour aligned blocks (cacheable) and a live trailing window.
///
/// A block is cacheable only once its end time is at least CACHE_GRACE_SECS in the past,
/// giving late-landing aw-client events (flushed every 10s) time to arrive.
/// Live window: (end of last cached block, now) — covers the full current partial hour.
/// Never overlaps with cached blocks.
#[allow(clippy::type_complexity)]
fn today_time_blocks(
    now: DateTime<Local>,
    day_start: DateTime<Local>,
) -> (
    Vec<(DateTime<Local>, DateTime<Local>)>,
    (DateTime<Local>, DateTime<Local>),
) {
    let grace = Duration::seconds(CACHE_GRACE_SECS);

    let cached_blocks: Vec<_> =
        std::iter::successors(Some(day_start), |&h| Some(h + Duration::hours(1)))
            .take_while(|&h| h + Duration::hours(1) + grace <= now)
            .map(|h| (h, h + Duration::hours(1)))
            .collect();

    // The live window starts at the end of the last cached block (or day_start if none are cached yet).
    let live_start = cached_blocks
        .last()
        .map(|&(_, end)| end)
        .unwrap_or(day_start);
    (cached_blocks, (live_start, now))
}

fn build_query(hostname: &str, regex: &str) -> String {
    let params = DesktopQueryParams {
        base: QueryParamsBase {
            bid_browsers: vec![],
            classes: vec![(
                vec!["Work".to_string()],
                CategorySpec {
                    spec_type: "regex".to_string(),
                    regex: regex.to_string(),
                    ignore_case: true,
                },
            )],
            filter_classes: vec![vec!["Work".to_string()]],
            filter_afk: true,
            include_audible: false,
        },
        bid_window: format!("aw-watcher-window_{}", hostname),
        bid_afk: format!("aw-watcher-afk_{}", hostname),
        always_active_pattern: None,
    };
    // canonical_events() joins items with ";\n" but some items already end with ";",
    // producing ";;" separators. Collapse them, then ensure a clean terminating ";".
    let canonical = QueryParams::Desktop(params).canonical_events();
    let canonical = canonical.replace(";;", ";");
    let canonical = canonical.trim_end_matches(';').trim_end();
    format!(
        "{canonical};\nduration = sum_durations(events);\nRETURN = {{\"events\": events, \"duration\": duration}};"
    )
}

async fn sync_sheet(
    aw_client: &AwClient,
    provider: &dyn SheetProvider,
    sheet_key: &str,
    regex: &str,
    host: &str,
) -> Result<()> {
    let cache_file = cache_path(host, sheet_key);
    let mut cache = QueryCache::load(cache_file)?;

    let now = Local::now();
    let today_naive = NaiveDateTime::new(
        now.date_naive(),
        NaiveTime::from_hms_opt(DAY_START_HOUR, 0, 0).unwrap(),
    );
    let today = Local
        .from_local_datetime(&today_naive)
        .single()
        .expect("ambiguous local time for today's day-start");

    let worksheet = format!("worked-{}", host);

    let raw_values = provider
        .get_all_values(sheet_key, &worksheet)
        .await
        .with_context(|| {
            format!(
                "Worksheet '{}' not found — create it in your spreadsheet first.",
                worksheet
            )
        })?;

    let values: Vec<Vec<String>> = raw_values
        .into_iter()
        .filter(|row| !row.is_empty() && !row[0].trim().is_empty())
        .collect();

    let last_date: Option<NaiveDate> = values
        .last()
        .and_then(|row| row.first())
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

    let last_datetime = last_date.map(|d| {
        let naive = NaiveDateTime::new(d, NaiveTime::from_hms_opt(DAY_START_HOUR, 0, 0).unwrap());
        Local.from_local_datetime(&naive).single().unwrap()
    });

    if let Some(ld) = last_date {
        info!("Last entry: {}", ld);
    }

    let days_back = match &last_datetime {
        Some(ldt) => (today.signed_duration_since(*ldt).num_days() + 1).max(1) as usize,
        None => DAYS_BACK_ON_NEW as usize,
    };

    let query_days_back = days_back.max(DAYS_BACK_ON_NEW as usize);

    // Day-granular timeperiods (one per day from oldest to today).
    let timeperiods: Vec<(DateTime<Local>, DateTime<Local>)> = (0..query_days_back)
        .rev()
        .map(|i| {
            let i = i as i64;
            (today - Duration::days(i), today + Duration::days(1 - i))
        })
        .collect();

    // Build a batched query covering cache misses + grace-period days + the live window.
    //
    // batch_query: timeperiods to send to AW.
    // batch_cache: per-slot caching intent — Some((key, ttl)) to store, None to skip caching.
    //   None is used for: (a) the live window, and (b) past days still within CACHE_GRACE_SECS
    //   of their end time (query fresh every sync until settled, then lock into 3-day TTL).
    let mut batch_query: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    let mut batch_cache: Vec<Option<(String, i64)>> = Vec::new();

    let now_utc = now.with_timezone(&Utc);

    // Past days (all except the last entry which is today).
    for tp in &timeperiods[..timeperiods.len().saturating_sub(1)] {
        if tp.0 > now {
            continue;
        }
        let (s, e) = (tp.0.with_timezone(&Utc), tp.1.with_timezone(&Utc));
        let key = cache_key(&s, &e);

        if e + Duration::seconds(CACHE_GRACE_SECS) > now_utc {
            // Day ended within the grace window: always query fresh so late-landing events
            // (still in aw-client's commit buffer) are captured before the 3-day cache locks in.
            batch_query.push((s, e));
            batch_cache.push(None);
        } else if cache.get(&key).is_none() {
            // Settled and not yet cached: query and store with 3-day TTL.
            batch_query.push((s, e));
            batch_cache.push(Some((key, CACHE_TTL_PAST_DAYS_SECS)));
        }
        // else: settled cache hit — skip.
    }

    // Today: completed 1-hour blocks (cacheable) + live window (always queried).
    let (today_blocks, live_window) = today_time_blocks(now, today);
    for (bs, be) in &today_blocks {
        let (s, e) = (bs.with_timezone(&Utc), be.with_timezone(&Utc));
        let key = cache_key(&s, &e);
        if cache.get(&key).is_none() {
            batch_query.push((s, e));
            batch_cache.push(Some((key, CACHE_TTL_TODAY_HOURS_SECS)));
        }
    }
    batch_query.push((
        live_window.0.with_timezone(&Utc),
        live_window.1.with_timezone(&Utc),
    ));
    batch_cache.push(None); // live window: never cached

    let query_str = build_query(host, regex);
    info!(
        "Querying ActivityWatch ({} window(s))...",
        batch_query.len()
    );
    let mut fresh_results = aw_client
        .query(&query_str, batch_query.clone())
        .await
        .context("Failed to query ActivityWatch (is it running on port 5600?)")?;

    let live_result = fresh_results.pop().unwrap();

    // Store results that have a cache intent. Collect the rest (grace-period past days)
    // into fresh_map so reconstruction can use them without hitting an empty cache entry.
    let mut fresh_map: HashMap<String, serde_json::Value> = HashMap::new();
    for (i, result) in fresh_results.into_iter().enumerate() {
        match &batch_cache[i] {
            Some((key, ttl)) => {
                let events: Vec<Event> =
                    Deserialize::deserialize(&result["events"]).unwrap_or_default();
                let secs =
                    working_hours::generous_approx(&events, Duration::seconds(BREAK_TIME_SECS))
                        .num_seconds() as f64;
                let bounds = working_hours::event_boundaries(&events);
                cache.insert(
                    key.clone(),
                    secs,
                    bounds.map(|(f, _)| f),
                    bounds.map(|(_, l)| l),
                    *ttl,
                );
            }
            None => {
                let (s, e) = batch_query[i];
                fresh_map.insert(cache_key(&s, &e), result);
            }
        }
    }
    cache.save()?;

    let initial_row_count = values.len();
    let mut changes = Vec::new();
    let mut current_row_count = initial_row_count;

    for tp in &timeperiods {
        if tp.0 > now {
            continue;
        }

        let date = tp.0.date_naive();
        let date_str = date.format("%Y-%m-%d").to_string();

        let is_today = date == today.date_naive();

        let hours = if is_today {
            // Stitch today's cached hourly blocks + live window, filling
            // cross-boundary gaps < BREAK_TIME_SECS that independent
            // per-block generous_approx calls would miss.
            let max_break = Duration::seconds(BREAK_TIME_SECS);

            // Collect (duration_secs, first_event_ts, last_event_end_ts) per segment.
            #[allow(clippy::type_complexity)]
            let mut segments: Vec<(f64, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> = Vec::new();
            for (bs, be) in &today_blocks {
                let key = cache_key(&bs.with_timezone(&Utc), &be.with_timezone(&Utc));
                if let Some(entry) = cache.get(&key) {
                    let secs = entry.duration.unwrap_or(0.0);
                    if secs > 0.0 {
                        segments.push((secs, entry.first_event_ts, entry.last_event_end_ts));
                    }
                }
            }
            // Live window segment.
            let live_evs: Vec<Event> =
                Deserialize::deserialize(&live_result["events"]).unwrap_or_default();
            let live_secs =
                working_hours::generous_approx(&live_evs, max_break).num_seconds() as f64;
            if live_secs > 0.0 {
                let bounds = working_hours::event_boundaries(&live_evs);
                segments.push((live_secs, bounds.map(|(f, _)| f), bounds.map(|(_, l)| l)));
            }

            // Sum durations, adding inter-segment gaps that generous_approx would merge.
            let mut total_secs = 0.0f64;
            let mut prev_end: Option<DateTime<Utc>> = None;
            for (secs, first_ts, last_end) in &segments {
                if let (Some(pe), Some(ft)) = (prev_end, first_ts) {
                    let gap = *ft - pe;
                    if gap > Duration::zero() && gap < max_break {
                        total_secs += gap.num_seconds() as f64;
                    }
                }
                total_secs += secs;
                // Update prev_end to the latest end we've seen so far.
                prev_end = match (prev_end, *last_end) {
                    (Some(pe), Some(le)) => Some(pe.max(le)),
                    (None, le) => le,
                    (pe, None) => pe,
                };
            }
            total_secs / 3600.0
        } else {
            let (s, e) = (tp.0.with_timezone(&Utc), tp.1.with_timezone(&Utc));
            let key = cache_key(&s, &e);
            // Grace-period days are in fresh_map (not yet cached); settled days are in cache.
            if let Some(entry) = cache.get(&key) {
                entry.duration.unwrap_or(0.0) / 3600.0
            } else if let Some(result) = fresh_map.get(&key) {
                let events: Vec<Event> =
                    Deserialize::deserialize(&result["events"]).unwrap_or_default();
                let secs =
                    working_hours::generous_approx(&events, Duration::seconds(BREAK_TIME_SECS))
                        .num_seconds() as f64;
                secs / 3600.0
            } else {
                0.0
            }
        };

        match last_date {
            Some(ld) if date == ld => {
                info!("Updating  [{}, {:.4}h]", date_str, hours);
                changes.push(SheetWrite {
                    row: initial_row_count,
                    date: None,
                    hours,
                });
            }
            Some(ld) if date > ld => {
                info!("Appending [{}, {:.4}h]", date_str, hours);
                current_row_count += 1;
                changes.push(SheetWrite {
                    row: current_row_count,
                    date: Some(date_str),
                    hours,
                });
            }
            None => {
                info!("Appending [{}, {:.4}h]", date_str, hours);
                current_row_count += 1;
                changes.push(SheetWrite {
                    row: current_row_count,
                    date: Some(date_str),
                    hours,
                });
            }
            _ => {
                debug!("Skipping  [{}, {:.4}h]", date_str, hours);
            }
        }
    }

    if !changes.is_empty() {
        provider
            .write_changes(sheet_key, &worksheet, changes)
            .await?;
    }

    Ok(())
}

struct ProviderCache {
    google: tokio::sync::Mutex<Option<std::sync::Arc<google_sheets::SheetsClient>>>,
    excel_online: tokio::sync::Mutex<Option<std::sync::Arc<excel_online::ExcelOnlineProvider>>>,
    excel_local: excel_local::ExcelLocalProvider,
}

impl ProviderCache {
    fn new() -> Self {
        Self {
            google: tokio::sync::Mutex::new(None),
            excel_online: tokio::sync::Mutex::new(None),
            excel_local: excel_local::ExcelLocalProvider,
        }
    }

    async fn get_google(&self) -> Result<std::sync::Arc<google_sheets::SheetsClient>> {
        let mut guard = self.google.lock().await;
        if let Some(ref client) = *guard {
            Ok(client.clone())
        } else {
            let client =
                std::sync::Arc::new(google_sheets::SheetsClient::from_service_account().await?);
            *guard = Some(client.clone());
            Ok(client)
        }
    }

    async fn get_excel_online(&self) -> Result<std::sync::Arc<excel_online::ExcelOnlineProvider>> {
        let mut guard = self.excel_online.lock().await;
        if let Some(ref client) = *guard {
            Ok(client.clone())
        } else {
            let client = std::sync::Arc::new(
                excel_online::ExcelOnlineProvider::from_client_credentials().await?,
            );
            *guard = Some(client.clone());
            Ok(client)
        }
    }
}

async fn sync_once(
    aw_client: &AwClient,
    config: &Config,
    host: &str,
    provider_cache: &ProviderCache,
) -> Result<()> {
    let mut errors: Vec<String> = Vec::new();
    for (idx, sheet) in config.sheets.iter().enumerate() {
        info!(
            "--- Syncing sheet {}/{} [Key: {}, Regex: '{}'] ---",
            idx + 1,
            config.sheets.len(),
            sheet.sheet_key,
            sheet.regex
        );
        let result = match infer_provider(&sheet.sheet_key) {
            ProviderType::Google => match provider_cache.get_google().await {
                Ok(provider) => {
                    sync_sheet(
                        aw_client,
                        provider.as_ref(),
                        &sheet.sheet_key,
                        &sheet.regex,
                        host,
                    )
                    .await
                }
                Err(e) => Err(e),
            },
            ProviderType::ExcelLocal => {
                let provider = &provider_cache.excel_local;
                sync_sheet(aw_client, provider, &sheet.sheet_key, &sheet.regex, host).await
            }
            ProviderType::ExcelOnline => match provider_cache.get_excel_online().await {
                Ok(provider) => {
                    sync_sheet(
                        aw_client,
                        provider.as_ref(),
                        &sheet.sheet_key,
                        &sheet.regex,
                        host,
                    )
                    .await
                }
                Err(e) => Err(e),
            },
        };
        if let Err(e) = result {
            error!("Error syncing sheet '{}': {:#}", sheet.sheet_key, e);
            errors.push(format!("Sheet '{}': {:#}", sheet.sheet_key, e));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Failed to sync {} sheet(s): {}",
            errors.len(),
            errors.join("; ")
        ))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = parse_args()?;

    if config.testing {
        info!("Running in testing mode");
    } else {
        info!("Starting aw-work-sync");
    }
    debug!("Verbose logging enabled: {}", config.verbose);

    let host = hostname();
    info!("Hostname:  {}", host);

    let aw_config = aw_server::config::create_config(false, None);
    let server_api_key = aw_config.auth.api_key;
    if server_api_key.is_some() {
        info!("Loaded API key from aw-server-rust config");
    }
    let port = if config.testing { 5699 } else { 5600 };
    let aw_client = AwClient::new_with_api_key("localhost", port, "aw-work-sync", server_api_key)
        .map_err(|e| anyhow::anyhow!("Failed to create ActivityWatch client: {}", e))?;

    let provider_cache = ProviderCache::new();
    info!(
        "Running every {}s (Ctrl-C to stop).",
        config.interval.as_secs()
    );
    loop {
        if let Err(e) = sync_once(&aw_client, &config, &host, &provider_cache).await {
            error!("Sync error: {:#}", e);
        }
        tokio::time::sleep(config.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_legacy_config() {
        let yaml = r#"
sheet_key: "my-legacy-key"
regex: "my-legacy-regex"
"#;
        let cfg: FileConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.sheet_key, Some("my-legacy-key".to_string()));
        assert_eq!(cfg.regex, Some("my-legacy-regex".to_string()));
        assert!(cfg.sheets.is_empty());

        let cfg = cfg.normalize();
        assert_eq!(cfg.sheets.len(), 1);
        assert_eq!(cfg.sheets[0].sheet_key, "my-legacy-key");
        assert_eq!(cfg.sheets[0].regex, "my-legacy-regex");
        assert_eq!(cfg.sheet_key, Some("my-legacy-key".to_string()));
        assert_eq!(cfg.regex, Some("my-legacy-regex".to_string()));
    }

    #[test]
    fn test_deserialize_multi_config() {
        let yaml = r#"
sheets:
  - sheet_key: "key-1"
    regex: "regex-1"
  - sheet_key: "key-2"
    regex: "regex-2"
"#;
        let cfg: FileConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.sheet_key.is_none());
        assert!(cfg.regex.is_none());
        assert_eq!(cfg.sheets.len(), 2);

        let cfg = cfg.normalize();
        assert_eq!(cfg.sheets.len(), 2);
        assert_eq!(cfg.sheets[0].sheet_key, "key-1");
        assert_eq!(cfg.sheets[0].regex, "regex-1");
        assert_eq!(cfg.sheets[1].sheet_key, "key-2");
        assert_eq!(cfg.sheets[1].regex, "regex-2");
        // Check legacy fields are populated from first sheet for backward compatibility
        assert_eq!(cfg.sheet_key, Some("key-1".to_string()));
        assert_eq!(cfg.regex, Some("regex-1".to_string()));
    }

    #[test]
    fn test_config_infers_mixed_providers() {
        // Mirrors the multi-backend config documented in the README.
        let yaml = r#"
sheets:
  - sheet_key: your-google-spreadsheet-id
    regex: "work"
  - sheet_key: ~/work-hours.xlsx
    regex: "work"
  - sheet_key: b!driveId/01ABCITEMID
    regex: "work"
"#;
        let cfg: FileConfig = serde_yaml::from_str(yaml).unwrap();
        let cfg = cfg.normalize();
        assert_eq!(cfg.sheets.len(), 3);

        assert!(matches!(
            infer_provider(&cfg.sheets[0].sheet_key),
            ProviderType::Google
        ));
        assert!(matches!(
            infer_provider(&cfg.sheets[1].sheet_key),
            ProviderType::ExcelLocal
        ));
        assert!(matches!(
            infer_provider(&cfg.sheets[2].sheet_key),
            ProviderType::ExcelOnline
        ));
    }

    #[test]
    fn test_is_placeholder() {
        let cfg_placeholder_legacy = FileConfig {
            sheet_key: Some("your-sheet-key".to_string()),
            regex: Some("your-work-regex".to_string()),
            sheets: vec![],
        };
        assert!(is_placeholder(&cfg_placeholder_legacy));

        let cfg_placeholder_multi = FileConfig {
            sheet_key: None,
            regex: None,
            sheets: vec![
                SheetConfig {
                    sheet_key: "some-key".to_string(),
                    regex: "some-regex".to_string(),
                },
                SheetConfig {
                    sheet_key: "your-sheet-key".to_string(),
                    regex: "some-regex".to_string(),
                },
            ],
        };
        assert!(is_placeholder(&cfg_placeholder_multi));

        let cfg_valid = FileConfig {
            sheet_key: None,
            regex: None,
            sheets: vec![
                SheetConfig {
                    sheet_key: "key-1".to_string(),
                    regex: "regex-1".to_string(),
                },
                SheetConfig {
                    sheet_key: "key-2".to_string(),
                    regex: "regex-2".to_string(),
                },
            ],
        };
        assert!(!is_placeholder(&cfg_valid));
    }
}
