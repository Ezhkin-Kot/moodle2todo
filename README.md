# moodle2todo

Turns the events of a Moodle calendar feed (`.ics`) into **tasks** (`VTODO`) in a CalDAV calendar, such as one hosted on Nextcloud.

## Why

Moodle exports assignments and deadlines as calendar events whose start and end are the same moment. In most calendar clients these show up as zero-length events: thin, unlabeled slivers in the week view, and they can't be marked as done.

Subscribing to the feed on the server side doesn't fix this either. Subscribed calendars are read-only and are usually not exposed to other CalDAV clients.

`moodle2todo` solves both problems: it copies every Moodle event into a regular CalDAV calendar as a task with a due date. Any client that syncs with that calendar can display the tasks properly and mark them as completed.

## How it works

On every run the tool:

1. Downloads the Moodle `.ics` feed.
2. Converts each `VEVENT` into a `VTODO`:
   - `DUE` is taken from `DTEND` (or from `DTSTART` if there is no end).
   - `SUMMARY`, `DESCRIPTION`, `CATEGORIES` and `URL` are copied as they are.
3. Writes each task to the target calendar under a deterministic resource name derived from the event `UID` (`<uid>.ics`). Missing tasks are created; existing ones are updated only if the synced fields changed.

Things worth knowing:

- **Completion status is preserved.** Only `SUMMARY`, `DESCRIPTION`, `DUE`, `CATEGORIES` and `URL` are managed by the tool. Fields such as `STATUS`, `COMPLETED` and `PERCENT-COMPLETE`, which clients set when you tick a task off, are never touched.
- **Nothing is deleted.** Events that disappear from the feed stay in the calendar. A Moodle feed only covers a limited period, so automatic deletion would erase old completed tasks.
- **Use a dedicated calendar.** Create an empty calendar just for these tasks. If you ran another sync tool into the same calendar before, clear it first, because resource names may differ and produce duplicates.
- **Client rewrites.** If a client re-serializes the synced fields in its own way, the next run overwrites them with the Moodle values. This is harmless but can change the task's ETag.
- The tool makes one `GET` per event on every run, which is fine for hundreds of events at a 30-minute interval.

## Requirements

- A CalDAV server that supports `VTODO` (Nextcloud calendars do by default).
- Your Moodle calendar export URL: _Calendar → Export calendar → Get calendar URL_. Treat it as a secret, since it contains your auth token.
- A Nextcloud **app password**: _Personal settings → Security → Devices & sessions → Create new app password_.
- Docker with Compose, or a Rust toolchain.

## Configuration

All settings are environment variables.

| Variable             | Required | Description                                                                                                                                                           |
| -------------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `MOODLE_ICS_URL`     | yes      | Moodle calendar export URL                                                                                                                                            |
| `CALENDAR_URL`       | yes      | Full URL of the target calendar, e.g. `https://nextcloud.example.com/remote.php/dav/calendars/USER/moodle-tasks/` (in Nextcloud: calendar menu → _Copy private link_) |
| `NC_USER`            | yes      | Nextcloud login                                                                                                                                                       |
| `NC_APP_PASSWORD`    | yes      | Nextcloud app password (not your main password)                                                                                                                       |
| `SYNC_INTERVAL_SECS` | no       | Repeat the sync every N seconds. If unset, the tool runs once and exits                                                                                               |
| `NC_CA_FILE`         | no       | Path to a PEM root certificate to trust for the CalDAV server (for private CAs)                                                                                       |
| `NC_INSECURE`        | no       | `true` disables certificate verification for the CalDAV connection. Prefer `NC_CA_FILE`                                                                               |

`NC_INSECURE` and `NC_CA_FILE` apply to the connection to the CalDAV server only. Moodle is always contacted with normal certificate verification.

## Usage

### Docker Compose

Put the project in a `moodle2todo/` directory next to your `docker-compose.yml` and add a service:

```yaml
moodle2todo:
  build: ./moodle2todo
  restart: unless-stopped
  environment:
    MOODLE_ICS_URL: "https://moodle.example.com/calendar/export_execute.php?userid=123&authtoken=TOKEN&preset_what=all&preset_time=custom"
    CALENDAR_URL: "https://nextcloud.example.com/remote.php/dav/calendars/USER/moodle-tasks/"
    NC_USER: "USER"
    NC_APP_PASSWORD: "xxxxx-xxxxx-xxxxx-xxxxx-xxxxx"
    SYNC_INTERVAL_SECS: "1800"
```

If the container can't resolve your Nextcloud hostname (for example a name provided by a VPN's DNS), add an `extra_hosts` entry pointing it to the host. Consider moving secrets into a `.env` file.

Start it:

```bash
docker compose up -d --build moodle2todo
docker compose logs -f moodle2todo
```

Each sync prints a summary line:

```
total=42 created=3 updated=1 unchanged=38 errors=0
```

After changing the environment in `docker-compose.yml`, apply it with `docker compose up -d moodle2todo`; a plain `restart` keeps the old values.

### Without Docker

```bash
export MOODLE_ICS_URL=... CALENDAR_URL=... NC_USER=... NC_APP_PASSWORD=...
cargo run --release
```

Without `SYNC_INTERVAL_SECS` this performs a single sync, so it also works well from cron or a systemd timer.

### Client setup

Make sure the new calendar is enabled in your CalDAV client and that the client displays tasks. If you previously subscribed to the Moodle feed in that client, remove the subscription to avoid seeing both events and tasks.
