# aw-work-sync

Syncs daily work hours tracked by [ActivityWatch](https://activitywatch.net) to a Google Sheets spreadsheet. It queries ActivityWatch for window activity matching a configurable regex, approximates worked hours using a break-gap flood algorithm, and appends or updates a row per day in a dedicated worksheet.

Each machine gets its own worksheet named `worked-<hostname>` with two columns: date (`YYYY-MM-DD`) and hours worked.

## How it works

1. Connects to a local ActivityWatch instance on `localhost:5600`
2. Queries window/AFK buckets using `DesktopQueryParams` from `aw-client-rust`, filtering events to those matching a work regex (e.g. `"work|github|jira"`)
3. Computes worked hours via a generous approximation that floods short gaps (under 10 minutes) between events
4. Writes results to Google Sheets via the Sheets API v4, authenticated with a service account

The query building is based on the `DesktopQueryParams` example in [`aw-client-rust/src/queries.rs`](https://github.com/ActivityWatch/aw-server-rust/blob/master/aw-client-rust/src/queries.rs).

## Setup

### Google Sheets service account

Follow the [gspread service account guide](https://gspread.readthedocs.io/en/latest/oauth2.html#for-bots-using-service-account) to create a service account and download its JSON key. Place it at:

- **Linux/macOS:** `~/.config/gspread/service_account.json`
- **Windows:** `%APPDATA%\..\Roaming\.config\gspread\service_account.json`

Share your spreadsheet with the service account email.

### Spreadsheet

Create a worksheet named `worked-<your-hostname>` (e.g. `worked-mymachine`) inside your spreadsheet. The sheet key is the long ID in the spreadsheet URL.

### Configuration

On first run, a config file is created at:

- **Linux/macOS:** `~/.config/activitywatch/aw-work-sync/config.yaml`
- **Windows:** `%APPDATA%\activitywatch\aw-work-sync\config.yaml`

Edit it to set your values:

```yaml
sheet_key: your-spreadsheet-id
regex: "work|github|yourcompany"
```

Alternatively, pass them directly on the command line (config is saved automatically):

```
aw-work-sync <sheet_key> <regex>
```

## Usage

```
aw-work-sync [sheet_key regex] [--interval <5m|2h|30s>]
```

Runs in a loop, syncing every 5 minutes by default. Use `--interval` to change the cadence. ActivityWatch must be running on `localhost:5600`.

## Building

```
cargo build --release
```

Binaries for Linux, macOS, and Windows are available on the [releases page](https://github.com/0xbrayo/aw-work-sync/releases).
