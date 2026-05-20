 mod sheets;
mod working_hours;

use anyhow::{Context, Result};
use aw_client_rust::{
    classes::CategorySpec,
    queries::{DesktopQueryParams, QueryParams, QueryParamsBase},
    AwClient,
};
use aw_models::Event;
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration as StdDuration;

const BREAK_TIME_SECS: i64 = 10 * 60;
const DAYS_BACK_ON_NEW: i64 = 30;
const DAY_START_HOUR: u32 = 4;

#[derive(Serialize, Deserialize)]
struct FileConfig {
    sheet_key: String,
    regex: String,
}

struct Config {
    sheet_key: String,
    regex: String,
    interval: StdDuration,
}

fn config_path() -> PathBuf {
    let dir = dirs::config_dir()
        .expect("cannot find config dir")
        .join("activitywatch")
        .join("aw-work-sync");
    std::fs::create_dir_all(&dir).expect("cannot create config dir");
    dir.join("config.yaml")
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
    let cfg = FileConfig { sheet_key: sheet_key.to_string(), regex: regex.to_string() };
    std::fs::write(path, serde_yaml::to_string(&cfg)?).context("failed to write config")?;
    println!("Config written to {}", path.display());
    Ok(())
}

/// Load config when no CLI values are given; exits with guidance if not set up.
fn load_file_config() -> Result<FileConfig> {
    let path = config_path();
    if !path.exists() {
        write_config(&path, "your-sheet-key", "your-work-regex")?;
        eprintln!("Created default config at {}", path.display());
        eprintln!("Please set sheet_key and regex, then re-run.");
        std::process::exit(1);
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let cfg: FileConfig =
        serde_yaml::from_str(&contents).context("failed to parse config.yaml")?;
    if is_placeholder(&cfg) {
        eprintln!("Config at {} still has placeholder values.", path.display());
        eprintln!("Please set sheet_key and regex, then re-run.");
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
            println!("Previous config backed up to {}", backup_path(&path, 1).display());
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
    let mut positional: Vec<String> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--interval" => {
                let val = args.get(i + 1).with_context(|| "--interval requires a value")?;
                interval = parse_duration(val)?;
                i += 2;
            }
            s if !s.starts_with("--") => {
                positional.push(s.to_string());
                i += 1;
            }
            other => {
                eprintln!("Unknown argument: {}", other);
                eprintln!("Usage: aw-work-sync [sheet_key regex] [--interval <5m|2h|30s>]");
                std::process::exit(1);
            }
        }
    }

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
            eprintln!("Usage: aw-work-sync [sheet_key regex] [--interval <5m|2h|30s>]");
            std::process::exit(1);
        }
    };

    Ok(Config { sheet_key, regex, interval })
}

fn hostname() -> String {
    Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
        .replace(".localdomain", "")
        .replace(".local", "")
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

    let values = sheets
        .get_all_values(sheet_key, &worksheet)
        .await
        .with_context(|| {
            format!("Worksheet '{}' not found — create it in your spreadsheet first.", worksheet)
        })?;

    let last_date: Option<NaiveDate> = values
        .last()
        .and_then(|row| row.first())
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

    let last_datetime = last_date.map(|d| {
        let naive =
            NaiveDateTime::new(d, NaiveTime::from_hms_opt(DAY_START_HOUR, 0, 0).unwrap());
        Local.from_local_datetime(&naive).single().unwrap()
    });

    if let Some(ld) = last_date {
        println!("Last entry: {}", ld);
    }

    let days_back = match &last_datetime {
        Some(ldt) => (today.signed_duration_since(*ldt).num_days() + 1).max(1) as usize,
        None => DAYS_BACK_ON_NEW as usize,
    };

    let mut timeperiods: Vec<(DateTime<Local>, DateTime<Local>)> = (0..days_back)
        .map(|i| {
            let i = i as i64;
            (today - Duration::days(i), today + Duration::days(1 - i))
        })
        .collect();
    timeperiods.reverse();

    let timeperiods_utc: Vec<(DateTime<Utc>, DateTime<Utc>)> = timeperiods
        .iter()
        .map(|(s, e)| (s.with_timezone(&Utc), e.with_timezone(&Utc)))
        .collect();

    let query_str = build_query(host, regex);
    println!("Querying ActivityWatch ({} day(s))...", days_back);
    let results = aw_client
        .query(&query_str, timeperiods_utc)
        .await
        .context("Failed to query ActivityWatch (is it running on port 5600?)")?;

    let initial_row_count = values.len();

    for (tp, result) in timeperiods.iter().zip(results.iter()) {
        if tp.0 > now {
            continue;
        }

        let date = tp.0.date_naive();
        let date_str = date.format("%Y-%m-%d").to_string();

        let events: Vec<Event> =
            serde_json::from_value(result["events"].clone()).unwrap_or_default();
        let hours = working_hours::generous_approx(&events, Duration::seconds(BREAK_TIME_SECS))
            .num_seconds() as f64
            / 3600.0;

        match last_date {
            Some(ld) if date == ld => {
                println!("Updating  [{}, {:.4}h]", date_str, hours);
                sheets.update_cell(sheet_key, &worksheet, initial_row_count, hours).await?;
            }
            Some(ld) if date > ld => {
                println!("Appending [{}, {:.4}h]", date_str, hours);
                sheets.append_row(sheet_key, &worksheet, &date_str, hours).await?;
            }
            None => {
                println!("Appending [{}, {:.4}h]", date_str, hours);
                sheets.append_row(sheet_key, &worksheet, &date_str, hours).await?;
            }
            _ => {
                println!("Skipping  [{}, {:.4}h]", date_str, hours);
            }
        }
    }

    println!("Done.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = parse_args()?;

    let host = hostname();
    println!("Hostname:  {}", host);

    let aw_config = aw_server::config::create_config(false, None);
    let server_api_key = aw_config.auth.api_key;
    if server_api_key.is_some() {
        println!("Loaded API key from aw-server-rust config");
    }
    let aw_client =
        AwClient::new_with_api_key("localhost", 5600, "aw-work-sync", server_api_key)
            .map_err(|e| anyhow::anyhow!("Failed to create ActivityWatch client: {}", e))?;

    println!("Running every {}s (Ctrl-C to stop).", config.interval.as_secs());
    loop {
        if let Err(e) = sync_once(&aw_client, &config.sheet_key, &config.regex, &host).await {
            eprintln!("Sync error: {:#}", e);
        }
        tokio::time::sleep(config.interval).await;
    }

    Ok(())
}
