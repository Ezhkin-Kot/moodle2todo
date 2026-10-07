//! Fetches Moodle's .ics feed and creates/updates the corresponding tasks
//! (VTODO) in the Nextcloud calendar. Completion status set in the client
//! (Calino, etc.) is not overwritten: only the SYNC_KEYS fields are synced.

use std::{env, fs, thread, time::Duration};

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::StatusCode;

/// Fields taken from Moodle. Everything else in the task (STATUS, COMPLETED,
/// PERCENT-COMPLETE, ...) is left untouched.
const SYNC_KEYS: [&str; 5] = ["SUMMARY", "DESCRIPTION", "DUE", "CATEGORIES", "URL"];

struct Config {
    moodle_url: String,
    calendar_url: String,
    user: String,
    pass: String,
}

enum Outcome {
    Created,
    Updated,
    Unchanged,
}

fn env_req(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("environment variable {name} is not set"))
}

fn env_true(name: &str) -> bool {
    matches!(
        env::var(name).as_deref().map(str::to_lowercase).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

// ---------- minimal iCalendar handling ----------

/// Unfolds "folded" lines (a continuation starts with a space/tab).
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.last_mut().unwrap().push_str(&line[1..]);
        } else if !line.is_empty() {
            out.push(line.to_string());
        }
    }
    out
}

/// Folds a line at 75 octets without splitting UTF-8 characters.
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut len = 0usize;
    for ch in line.chars() {
        let n = ch.len_utf8();
        if len + n > 75 {
            out.push_str("\r\n ");
            len = 1;
        }
        out.push(ch);
        len += n;
    }
    out
}

fn serialize(lines: &[String]) -> String {
    let mut s = String::new();
    for l in lines {
        s.push_str(&fold(l));
        s.push_str("\r\n");
    }
    s
}

/// Property name in uppercase: everything before the first ':' or ';'.
fn prop_name(line: &str) -> String {
    line.split(|c: char| c == ':' || c == ';')
        .next()
        .unwrap_or("")
        .to_uppercase()
}

/// Returns the properties of each VEVENT (nested components, e.g. VALARM, are skipped).
fn parse_events(text: &str) -> Vec<Vec<String>> {
    let mut events = Vec::new();
    let mut cur: Option<Vec<String>> = None;
    let mut nested: i32 = 0;
    for line in unfold(text) {
        let up = line.to_uppercase();
        if cur.is_none() {
            if up == "BEGIN:VEVENT" {
                cur = Some(Vec::new());
                nested = 0;
            }
            continue;
        }
        if up == "END:VEVENT" && nested == 0 {
            events.push(cur.take().unwrap());
        } else if up.starts_with("BEGIN:") {
            nested += 1;
        } else if up.starts_with("END:") {
            nested -= 1;
        } else if nested == 0 {
            cur.as_mut().unwrap().push(line);
        }
    }
    events
}

/// Builds the UID and the set of property lines for VTODO from a VEVENT.
/// DUE is taken from DTEND (or DTSTART if DTEND is absent).
fn todo_props(ev: &[String]) -> Result<(String, Vec<String>)> {
    let uid = ev
        .iter()
        .find(|l| prop_name(l) == "UID")
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .context("event has no UID")?;

    let mut props: Vec<String> = ev
        .iter()
        .filter(|l| {
            matches!(
                prop_name(l).as_str(),
                "SUMMARY" | "DESCRIPTION" | "CATEGORIES" | "URL"
            )
        })
        .cloned()
        .collect();

    let due = ev
        .iter()
        .find(|l| prop_name(l) == "DTEND")
        .or_else(|| ev.iter().find(|l| prop_name(l) == "DTSTART"));
    if let Some(l) = due {
        let pos = l.find(|c: char| c == ':' || c == ';').unwrap_or(l.len());
        props.push(format!("DUE{}", &l[pos..]));
    }
    Ok((uid, props))
}

fn new_todo(uid: &str, props: &[String]) -> String {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let mut lines: Vec<String> = vec![
        "BEGIN:VCALENDAR".into(),
        "VERSION:2.0".into(),
        "PRODID:-//moodle2todo//EN".into(),
        "BEGIN:VTODO".into(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{stamp}"),
    ];
    lines.extend(props.iter().cloned());
    lines.push("END:VTODO".into());
    lines.push("END:VCALENDAR".into());
    serialize(&lines)
}

/// Indices of SYNC_KEYS lines located directly in VTODO (not in a nested VALARM).
fn sync_indices(lines: &[String]) -> Vec<usize> {
    let mut idx = Vec::new();
    let mut in_todo = false;
    let mut nested: i32 = 0;
    for (i, l) in lines.iter().enumerate() {
        let up = l.to_uppercase();
        if !in_todo {
            if up == "BEGIN:VTODO" {
                in_todo = true;
                nested = 0;
            }
            continue;
        }
        if up == "END:VTODO" && nested == 0 {
            break;
        }
        if up.starts_with("BEGIN:") {
            nested += 1;
        } else if up.starts_with("END:") {
            nested -= 1;
        } else if nested == 0 && SYNC_KEYS.contains(&prop_name(l).as_str()) {
            idx.push(i);
        }
    }
    idx
}

/// Substitutes fresh SYNC fields into an existing task.
/// Returns None if there is nothing to change.
fn merge(body: &str, props: &[String]) -> Option<String> {
    let lines = unfold(body);
    let idx = sync_indices(&lines);

    let mut old: Vec<&String> = idx.iter().map(|&i| &lines[i]).collect();
    let mut new: Vec<&String> = props.iter().collect();
    old.sort();
    new.sort();
    if old == new {
        return None;
    }

    let mut out: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !idx.contains(i))
        .map(|(_, l)| l.clone())
        .collect();
    let end = out
        .iter()
        .position(|l| l.eq_ignore_ascii_case("END:VTODO"))?;
    for (k, p) in props.iter().enumerate() {
        out.insert(end + k, p.clone());
    }
    Some(serialize(&out))
}

// ---------- CalDAV ----------

fn resource_name(uid: &str) -> String {
    let s: String = uid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{s}.ics")
}

fn put(
    nc: &Client,
    cfg: &Config,
    url: &str,
    body: String,
    cond: (&str, &str),
) -> Result<()> {
    let r = nc
        .put(url)
        .basic_auth(&cfg.user, Some(&cfg.pass))
        .header("Content-Type", "text/calendar; charset=utf-8")
        .header(cond.0, cond.1)
        .body(body)
        .send()?;
    if !r.status().is_success() {
        bail!("PUT {url} -> {}", r.status());
    }
    Ok(())
}

fn process(nc: &Client, cfg: &Config, ev: &[String]) -> Result<Outcome> {
    let (uid, props) = todo_props(ev)?;
    let url = format!(
        "{}/{}",
        cfg.calendar_url.trim_end_matches('/'),
        resource_name(&uid)
    );

    let resp = nc.get(&url).basic_auth(&cfg.user, Some(&cfg.pass)).send()?;
    match resp.status() {
        StatusCode::NOT_FOUND => {
            put(nc, cfg, &url, new_todo(&uid, &props), ("If-None-Match", "*"))?;
            Ok(Outcome::Created)
        }
        s if s.is_success() => {
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let body = resp.text()?;
            match merge(&body, &props) {
                None => Ok(Outcome::Unchanged),
                Some(new_body) => {
                    let (h, v) = match &etag {
                        Some(e) => ("If-Match", e.as_str()),
                        None => ("If-Match", "*"),
                    };
                    put(nc, cfg, &url, new_body, (h, v))?;
                    Ok(Outcome::Updated)
                }
            }
        }
        s => bail!("GET {url} -> {s}"),
    }
}

fn sync_once(moodle: &Client, nc: &Client, cfg: &Config) -> Result<()> {
    let ics = moodle
        .get(&cfg.moodle_url)
        .send()?
        .error_for_status()?
        .text()?;
    let events = parse_events(&ics);

    let (mut created, mut updated, mut unchanged, mut errors) = (0, 0, 0, 0);
    for ev in &events {
        match process(nc, cfg, ev) {
            Ok(Outcome::Created) => created += 1,
            Ok(Outcome::Updated) => updated += 1,
            Ok(Outcome::Unchanged) => unchanged += 1,
            Err(e) => {
                errors += 1;
                eprintln!("error: {e:#}");
            }
        }
    }
    println!(
        "total={} created={created} updated={updated} unchanged={unchanged} errors={errors}",
        events.len()
    );
    Ok(())
}

fn main() -> Result<()> {
    let cfg = Config {
        moodle_url: env_req("MOODLE_ICS_URL")?,
        calendar_url: env_req("CALENDAR_URL")?,
        user: env_req("NC_USER")?,
        pass: env_req("NC_APP_PASSWORD")?,
    };

    let moodle = Client::builder().timeout(Duration::from_secs(30)).build()?;

    let mut b = Client::builder().timeout(Duration::from_secs(30));
    if env_true("NC_INSECURE") {
        b = b.danger_accept_invalid_certs(true);
    }
    if let Ok(path) = env::var("NC_CA_FILE") {
        let pem = fs::read(&path).with_context(|| format!("failed to read {path}"))?;
        b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
    }
    let nc = b.build()?;

    let interval = env::var("SYNC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());

    let Some(secs) = interval else {
        return sync_once(&moodle, &nc, &cfg);
    };
    loop {
        if let Err(e) = sync_once(&moodle, &nc, &cfg) {
            eprintln!("sync error: {e:#}");
        }
        thread::sleep(Duration::from_secs(secs));
    }
}
