mod cache;
mod logging;
mod sheets;
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

#[derive(Serialize, Deserialize)]
struct FileConfig {
    sheet_key: String,
    regex: String,
}

struct Config {
    sheet_key: String,
    regex: String,
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

fn cache_path(host: &str) -> PathBuf {
    config_dir().join(format!("query_cache_{}.json", host))
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
    cfg.sheet_key == "your-sheet-key" || cfg.regex == "your-work-regex"
}

fn write_config(path: &std::path::Path, sheet_key: &str, regex: &str) -> Result<()> {
    let cfg = FileConfig {
        sheet_key: sheet_key.to_string(),
        regex: regex.to_string(),
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
        warn!("Please set sheet_key and regex, then re-run.");
        std::process::exit(1);
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let cfg: FileConfig = serde_yaml::from_str(&contents).context("failed to parse config.yaml")?;
    if is_placeholder(&cfg) {
        error!("Config at {} still has placeholder values.", path.display());
        error!("Please set sheet_key and regex, then re-run.");
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
        if !is_placeholder(&existing) {
            if existing.sheet_key == sheet_key && existing.regex == regex {
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

    let (sheet_key, regex) = match positional.len() {
        0 => {
            let f = load_file_config()?;
            (f.sheet_key, f.regex)
        }
        2 => {
            apply_cli_to_config(&positional[0], &positional[1])?;
            (positional[0].clone(), positional[1].clone())
        }
        _ => {
            eprintln!("Provide both sheet_key and regex, or neither.");
            eprintln!("Usage: aw-work-sync [sheet_key regex] [--interval <5m|2h|30s>] [--verbose] [--testing]");
            std::process::exit(1);
        }
    };

    Ok(Config {
        sheet_key,
        regex,
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

async fn sync_once(aw_client: &AwClient, sheet_key: &str, regex: &str, host: &str) -> Result<()> {
    let mut cache = QueryCache::load(cache_path(host))?;

    let now = Local::now();
    let today_naive = NaiveDateTime::new(
        now.date_naive(),
        NaiveTime::from_hms_opt(DAY_START_HOUR, 0, 0).unwrap(),
    );
    let today = Local
        .from_local_datetime(&today_naive)
        .single()
        .expect("ambiguous local time for today's day-start");

    let sheets = sheets::SheetsClient::from_service_account().await?;
    let worksheet = format!("worked-{}", host);

    let raw_values = sheets
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
                cache.insert(key.clone(), secs, *ttl);
            }
            None => {
                let (s, e) = batch_query[i];
                fresh_map.insert(cache_key(&s, &e), result);
            }
        }
    }
    cache.save()?;

    let initial_row_count = values.len();

    for tp in &timeperiods {
        if tp.0 > now {
            continue;
        }

        let date = tp.0.date_naive();
        let date_str = date.format("%Y-%m-%d").to_string();

        let is_today = date == today.date_naive();

        let hours = if is_today {
            let mut total_secs = 0.0;
            for (bs, be) in &today_blocks {
                let key = cache_key(&bs.with_timezone(&Utc), &be.with_timezone(&Utc));
                if let Some(entry) = cache.get(&key) {
                    total_secs += entry.duration.unwrap_or(0.0);
                }
            }
            let live_evs: Vec<Event> =
                Deserialize::deserialize(&live_result["events"]).unwrap_or_default();
            let live_secs =
                working_hours::generous_approx(&live_evs, Duration::seconds(BREAK_TIME_SECS))
                    .num_seconds() as f64;
            total_secs += live_secs;
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
                sheets
                    .update_cell(sheet_key, &worksheet, initial_row_count, hours)
                    .await?;
            }
            Some(ld) if date > ld => {
                info!("Appending [{}, {:.4}h]", date_str, hours);
                sheets
                    .append_row(sheet_key, &worksheet, &date_str, hours)
                    .await?;
            }
            None => {
                info!("Appending [{}, {:.4}h]", date_str, hours);
                sheets
                    .append_row(sheet_key, &worksheet, &date_str, hours)
                    .await?;
            }
            _ => {
                debug!("Skipping  [{}, {:.4}h]", date_str, hours);
            }
        }
    }

    info!("Done.");
    Ok(())
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

    info!(
        "Running every {}s (Ctrl-C to stop).",
        config.interval.as_secs()
    );
    loop {
        if let Err(e) = sync_once(&aw_client, &config.sheet_key, &config.regex, &host).await {
            error!("Sync error: {:#}", e);
        }
        tokio::time::sleep(config.interval).await;
    }
}
