# aw-work-sync

Syncs daily work hours tracked by [ActivityWatch](https://activitywatch.net) to a spreadsheet. It queries ActivityWatch for window activity matching a configurable regex, approximates worked hours using a break-gap flood algorithm, and appends or updates a row per day in a dedicated worksheet.

Three spreadsheet backends are supported:

- **Google Sheets** — via the Sheets API v4
- **Local Excel files** — `.xlsx` files on disk
- **Excel Online** — workbooks on OneDrive/SharePoint via the Microsoft Graph API

You can sync to multiple destinations at once. Each machine gets its own worksheet named `worked-<hostname>` with two columns: date (`YYYY-MM-DD`) and hours worked.

## How it works

1. Connects to a local ActivityWatch instance on `localhost:5600`
2. Queries window/AFK buckets using `DesktopQueryParams` from `aw-client-rust`, filtering events to those matching a work regex (e.g. `"work|github|jira"`)
3. Computes worked hours via a generous approximation that floods short gaps (under 10 minutes) between events
4. Writes results to the configured backend(s), batching all per-day updates into a single request and reusing auth tokens across sync cycles

The backend for each entry is inferred from its sheet key:

| Sheet key looks like        | Backend       |
| --------------------------- | ------------- |
| ends with `.xlsx` / `.xls`  | Local Excel   |
| contains `/`                | Excel Online (`<drive-id>/<item-id>`) |
| anything else               | Google Sheets |

The query building is based on the `DesktopQueryParams` example in [`aw-client-rust/src/queries.rs`](https://github.com/ActivityWatch/aw-server-rust/blob/master/aw-client-rust/src/queries.rs).

## Setup

In every case, create a worksheet named `worked-<your-hostname>` (e.g. `worked-mymachine`) inside the target spreadsheet. For local Excel files the worksheet and the file itself are created automatically if missing.

### Google Sheets

Follow the [gspread service account guide](https://gspread.readthedocs.io/en/latest/oauth2.html#for-bots-using-service-account) to create a service account and download its JSON key. Place it at:

- **Linux/macOS:** `~/.config/gspread/service_account.json`
- **Windows:** `%APPDATA%\..\Roaming\.config\gspread\service_account.json`

Share your spreadsheet with the service account email. The sheet key is the long ID in the spreadsheet URL.

### Excel Online (Microsoft Graph)

Register an [Azure AD app](https://learn.microsoft.com/en-us/graph/auth-register-app-v2) and grant it the `Files.ReadWrite.All` **application** permission (client-credentials / daemon flow). Create a client secret, then store the credentials at `~/.config/msgraph/client_credentials.json`:

```json
{
  "tenant_id": "<tenant-id>",
  "client_id": "<client-id>",
  "client_secret": "<client-secret>"
}
```

The sheet key is `<drive-id>/<item-id>`, identifying the workbook's drive and drive-item in OneDrive/SharePoint.

### Local Excel

No credentials needed. The sheet key is a path to an `.xlsx` file (a leading `~` is expanded), e.g. `~/work-hours.xlsx`.

### Configuration

On first run, a config file is created at:

- **Linux/macOS:** `~/.config/activitywatch/aw-work-sync/config.yaml`
- **Windows:** `%APPDATA%\activitywatch\aw-work-sync\config.yaml`

Edit it to set your values. A single destination:

```yaml
sheet_key: your-spreadsheet-id
regex: "work|github|yourcompany"
```

Or multiple destinations of any backend (inferred from each `sheet_key`):

```yaml
sheets:
  - sheet_key: your-google-spreadsheet-id        # Google Sheets
    regex: "work|github|yourcompany"
  - sheet_key: ~/work-hours.xlsx                  # Local Excel
    regex: "work|github"
  - sheet_key: b!driveId.../01ABC...ITEMID        # Excel Online
    regex: "work"
```

Alternatively, pass a single destination directly on the command line (config is saved automatically):

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
