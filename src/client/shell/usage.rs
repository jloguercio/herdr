//! Claude Code and Codex plan limits (the 5-hour and weekly windows) for the bottom usage bar.
//!
//! Reads the sessions those tools already keep on this machine; it never logs in or refreshes
//! tokens. Requests go through `curl` with headers on stdin, so tokens never show up in `ps`.
//! Results are cached in the runtime dir so several clients share one fetch.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use serde_json::{json, Value};

use super::render::{display_width, put_segment};
use super::Palette;

const FRESH: Duration = Duration::from_secs(5 * 60);
/// The usage endpoints rate-limit aggressively; back off after a failure.
const FRESH_AFTER_ERROR: Duration = Duration::from_secs(10 * 60);
const POLL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub(super) struct UsageWindow {
    pub(super) label: &'static str,
    pub(super) pct: f64,
    /// Unix seconds.
    pub(super) resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ProviderUsage {
    Windows(Vec<UsageWindow>),
    Error(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct UsageSnapshot {
    pub(super) claude: Option<ProviderUsage>,
    pub(super) codex: Option<ProviderUsage>,
}

/// Background poller shared by the render loop through a generation counter.
pub(super) struct UsageMonitor {
    latest: Arc<Mutex<Option<UsageSnapshot>>>,
    generation: Arc<AtomicU64>,
}

impl UsageMonitor {
    pub(super) fn start() -> Self {
        let latest = Arc::new(Mutex::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let (thread_latest, thread_generation) = (latest.clone(), generation.clone());
        let _ = std::thread::Builder::new()
            .name("herdr-usage".into())
            .spawn(move || poll_forever(&thread_latest, &thread_generation));
        Self { latest, generation }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(super) fn snapshot(&self) -> Option<UsageSnapshot> {
        self.latest.lock().ok()?.clone()
    }
}

fn poll_forever(latest: &Mutex<Option<UsageSnapshot>>, generation: &AtomicU64) {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let cache = cache_path();
    let mut last_good = UsageSnapshot::default();
    loop {
        let now = unix_now();
        let snapshot = match read_cache(&cache, now) {
            Some(cached) => cached,
            None => {
                let fetched = UsageSnapshot {
                    claude: keep_good(claude(&home), &mut last_good.claude),
                    codex: keep_good(codex(&home), &mut last_good.codex),
                };
                write_cache(&cache, now, &fetched);
                fetched
            }
        };
        if let Ok(mut slot) = latest.lock() {
            if slot.as_ref() != Some(&snapshot) {
                *slot = Some(snapshot);
                generation.fetch_add(1, Ordering::Release);
            }
        }
        std::thread::sleep(POLL);
    }
}

/// A failed request keeps showing the last good numbers instead of the error.
fn keep_good(
    fresh: Option<ProviderUsage>,
    last: &mut Option<ProviderUsage>,
) -> Option<ProviderUsage> {
    match fresh {
        Some(ProviderUsage::Error(_)) if last.is_some() => last.clone(),
        other => {
            last.clone_from(&other);
            other
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn cache_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("herdr-usage.json")
}

fn read_cache(path: &Path, now: i64) -> Option<UsageSnapshot> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let fetched_at = value.get("fetched_at")?.as_i64()?;
    let snapshot = UsageSnapshot {
        claude: provider_from_json(value.get("claude")?),
        codex: provider_from_json(value.get("codex")?),
    };
    let has_error = [&snapshot.claude, &snapshot.codex]
        .into_iter()
        .any(|provider| matches!(provider, Some(ProviderUsage::Error(_))));
    let ttl = if has_error { FRESH_AFTER_ERROR } else { FRESH };
    (now - fetched_at < ttl.as_secs() as i64).then_some(snapshot)
}

fn write_cache(path: &Path, now: i64, snapshot: &UsageSnapshot) {
    let value = json!({
        "fetched_at": now,
        "claude": provider_to_json(snapshot.claude.as_ref()),
        "codex": provider_to_json(snapshot.codex.as_ref()),
    });
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, value.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn provider_to_json(provider: Option<&ProviderUsage>) -> Value {
    match provider {
        None => Value::Null,
        Some(ProviderUsage::Error(message)) => json!({ "error": message }),
        Some(ProviderUsage::Windows(windows)) => json!({
            "windows": windows
                .iter()
                .map(|w| json!({ "label": w.label, "pct": w.pct, "resets_at": w.resets_at }))
                .collect::<Vec<_>>()
        }),
    }
}

fn provider_from_json(value: &Value) -> Option<ProviderUsage> {
    if let Some(message) = value.get("error").and_then(Value::as_str) {
        return Some(ProviderUsage::Error(message.to_owned()));
    }
    let windows = value.get("windows")?.as_array()?;
    Some(ProviderUsage::Windows(
        windows
            .iter()
            .filter_map(|w| {
                Some(UsageWindow {
                    label: window_label(w.get("label")?.as_str()?),
                    pct: w.get("pct")?.as_f64()?,
                    resets_at: w.get("resets_at").and_then(Value::as_i64),
                })
            })
            .collect(),
    ))
}

fn window_label(label: &str) -> &'static str {
    [FIVE_HOURS, DAY, WEEK]
        .into_iter()
        .find(|known| *known == label)
        .unwrap_or(FIVE_HOURS)
}

const FIVE_HOURS: &str = "5h";
const DAY: &str = "day";
const WEEK: &str = "week";

/// Codex says how long each window is; on some plans the only window is the weekly one.
fn label_for_seconds(seconds: i64) -> &'static str {
    match seconds {
        ..=21_600 => FIVE_HOURS,
        ..=172_800 => DAY,
        _ => WEEK,
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// GET through curl; the URL and headers go in a config on stdin.
fn get(url: &str, headers: &[(&str, &str)]) -> Result<Value, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut config = format!("url = \"{url}\"\nheader = \"User-Agent: herdr\"\n");
    for (name, value) in headers {
        config.push_str(&format!("header = \"{name}: {value}\"\n"));
    }
    let mut child = Command::new("curl")
        .args(["-sS", "-m", "10", "-K", "-", "-w", "\n%{http_code}"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "curl missing".to_owned())?;
    child
        .stdin
        .take()
        .ok_or("curl failed")?
        .write_all(config.as_bytes())
        .map_err(|_| "curl failed".to_owned())?;
    let output = child
        .wait_with_output()
        .map_err(|_| "curl failed".to_owned())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, status) = text.rsplit_once('\n').unwrap_or(("", "000"));
    match status.trim() {
        "000" => Err("offline".into()),
        "401" | "403" => Err("session expired".into()),
        "429" => Err("rate limited".into()),
        "200" => serde_json::from_str(body).map_err(|_| "bad response".into()),
        other => Err(format!("error {other}")),
    }
}

fn windows(
    entries: [(&'static str, &Value); 2],
    pct_key: &str,
    reset: impl Fn(&Value) -> Option<i64>,
) -> ProviderUsage {
    ProviderUsage::Windows(
        entries
            .into_iter()
            .filter_map(|(label, w)| {
                Some(UsageWindow {
                    label: w
                        .get("limit_window_seconds")
                        .and_then(Value::as_i64)
                        .map_or(label, label_for_seconds),
                    pct: w.get(pct_key)?.as_f64()?,
                    resets_at: reset(w),
                })
            })
            .collect(),
    )
}

fn rfc3339_seconds(value: &Value) -> Option<i64> {
    let text = value.as_str()?;
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(time::OffsetDateTime::unix_timestamp)
}

/// Claude Code: ~/.claude/.credentials.json → the endpoint behind `/usage`.
fn claude(home: &Path) -> Option<ProviderUsage> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let credentials = read_json(&dir.join(".credentials.json"))?;
    let token = credentials
        .pointer("/claudeAiOauth/accessToken")?
        .as_str()?
        .to_owned();
    let auth = format!("Bearer {token}");
    Some(
        match get(
            "https://api.anthropic.com/api/oauth/usage",
            &[
                ("Authorization", &auth),
                ("anthropic-beta", "oauth-2025-04-20"),
            ],
        ) {
            Ok(usage) => windows(
                [
                    (FIVE_HOURS, &usage["five_hour"]),
                    (WEEK, &usage["seven_day"]),
                ],
                "utilization",
                |w| rfc3339_seconds(&w["resets_at"]),
            ),
            Err(message) => ProviderUsage::Error(message),
        },
    )
}

/// Codex: ~/.codex/auth.json (ChatGPT sign-in) → ChatGPT's usage endpoint. API keys have no such limits.
fn codex(home: &Path) -> Option<ProviderUsage> {
    let dir = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let auth = read_json(&dir.join("auth.json"))?;
    let token = auth.pointer("/tokens/access_token")?.as_str()?.to_owned();
    let bearer = format!("Bearer {token}");
    let account = auth.pointer("/tokens/account_id").and_then(Value::as_str);
    let mut headers = vec![("Authorization", bearer.as_str())];
    if let Some(account) = account {
        headers.push(("chatgpt-account-id", account));
    }
    let reset = |w: &Value| w.get("reset_at").and_then(Value::as_i64);
    Some(
        match get("https://chatgpt.com/backend-api/wham/usage", &headers) {
            Ok(usage) => {
                let limits = &usage["rate_limit"];
                windows(
                    [
                        (FIVE_HOURS, &limits["primary_window"]),
                        (WEEK, &limits["secondary_window"]),
                    ],
                    "used_percent",
                    reset,
                )
            }
            Err(message) => codex_from_sessions(&dir).unwrap_or(ProviderUsage::Error(message)),
        },
    )
}

/// The last limits notice Codex wrote into its newest saved session.
fn codex_from_sessions(dir: &Path) -> Option<ProviderUsage> {
    fn newest(dir: &Path, best: &mut Option<(SystemTime, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                newest(&path, best);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                if best.as_ref().is_none_or(|(time, _)| modified > *time) {
                    *best = Some((modified, path));
                }
            }
        }
    }
    let mut best = None;
    newest(&dir.join("sessions"), &mut best);
    let text = std::fs::read_to_string(best?.1).ok()?;
    text.lines().rev().find_map(|line| {
        let event: Value = serde_json::from_str(line).ok()?;
        let limits = event.pointer("/payload/rate_limits")?;
        limits.as_object().filter(|map| !map.is_empty())?;
        Some(windows(
            [
                (FIVE_HOURS, &limits["primary"]),
                (WEEK, &limits["secondary"]),
            ],
            "used_percent",
            |w| w.get("resets_at").and_then(Value::as_i64),
        ))
    })
}

/// "2h10m", "3d4h", "12m".
fn until(resets_at: i64, now: i64) -> String {
    let minutes = (resets_at - now).max(0) / 60;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else {
        format!("{mins}m")
    }
}

/// One row across the whole app: `<icon> Claude  5h 42% ↻1h20m  week 18% ↻3d4h   <icon> Codex …`.
pub(super) fn render_usage_bar(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: Option<&UsageSnapshot>,
    palette: &Palette,
    icons: bool,
) {
    if area.height == 0 {
        return;
    }
    let base = Style::default().fg(palette.text).bg(palette.panel_bg);
    buffer.set_style(area, base);
    let (y, right) = (area.y, area.right());
    let mut x = area.x.saturating_add(1);
    let dim = base.fg(palette.overlay0);
    let Some(snapshot) = snapshot else {
        put_segment(buffer, x, y, right, "loading usage…", dim);
        return;
    };
    let now = unix_now();
    let providers = [
        (super::nerd_icons::CLAUDE, "Claude", &snapshot.claude),
        (super::nerd_icons::OPENAI, "Codex", &snapshot.codex),
    ];
    let mut first = true;
    for (icon, name, usage) in providers {
        let Some(usage) = usage else {
            continue;
        };
        if !first {
            x = put_segment(buffer, x, y, right, "   ", base);
        }
        first = false;
        let label = if icons {
            format!("{icon} {name} ")
        } else {
            format!("{name} ")
        };
        x = put_segment(
            buffer,
            x,
            y,
            right,
            &label,
            base.fg(palette.accent).add_modifier(Modifier::BOLD),
        );
        match usage {
            ProviderUsage::Error(message) => {
                x = put_segment(buffer, x, y, right, message, base.fg(palette.red));
            }
            ProviderUsage::Windows(windows) => {
                for window in windows {
                    let pct = window.pct.round().clamp(0.0, 100.0);
                    let colour = if pct >= 85.0 {
                        palette.red
                    } else if pct >= 60.0 {
                        palette.yellow
                    } else {
                        palette.green
                    };
                    x = put_segment(buffer, x, y, right, &format!(" {} ", window.label), dim);
                    x = put_segment(buffer, x, y, right, &format!("{pct:.0}%"), base.fg(colour));
                    if let Some(resets_at) = window.resets_at {
                        x = put_segment(
                            buffer,
                            x,
                            y,
                            right,
                            &format!(" ↻{}", until(resets_at, now)),
                            dim,
                        );
                    }
                }
            }
        }
    }
    if first {
        let text = "no Claude Code or Codex session";
        if display_width(text) < area.width {
            put_segment(buffer, x, y, right, text, dim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_uses_the_largest_units() {
        assert_eq!(until(0 + 3 * 86_400 + 4 * 3600 + 59, 0), "3d4h");
        assert_eq!(until(2 * 3600 + 10 * 60, 0), "2h10m");
        assert_eq!(until(12 * 60 + 30, 0), "12m");
        assert_eq!(until(-5, 0), "0m");
    }

    #[test]
    fn cache_round_trips_and_expires() {
        let dir = std::env::temp_dir().join(format!("herdr-usage-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        let snapshot = UsageSnapshot {
            claude: Some(ProviderUsage::Windows(vec![UsageWindow {
                label: FIVE_HOURS,
                pct: 42.0,
                resets_at: Some(99),
            }])),
            codex: Some(ProviderUsage::Error("rate limited".into())),
        };
        write_cache(&path, 1_000, &snapshot);
        assert_eq!(read_cache(&path, 1_060), Some(snapshot));
        assert_eq!(read_cache(&path, 1_000 + 11 * 60), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_window_length_names_the_window() {
        let usage = json!({ "used_percent": 4, "limit_window_seconds": 604_800, "reset_at": 7 });
        let ProviderUsage::Windows(found) = windows(
            [(FIVE_HOURS, &usage), (WEEK, &Value::Null)],
            "used_percent",
            |w| w["reset_at"].as_i64(),
        ) else {
            panic!("expected windows");
        };
        assert_eq!(
            found,
            vec![UsageWindow {
                label: WEEK,
                pct: 4.0,
                resets_at: Some(7)
            }]
        );
    }

    #[test]
    fn a_failure_keeps_the_last_good_numbers() {
        let good = Some(ProviderUsage::Windows(Vec::new()));
        let mut last = None;
        assert_eq!(keep_good(good.clone(), &mut last), good);
        assert_eq!(
            keep_good(Some(ProviderUsage::Error("offline".into())), &mut last),
            good
        );
    }
}
