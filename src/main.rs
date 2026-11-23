//! log-forwarder: Loki에서 `forward_logs="true"` 스트림(컨테이너 라벨 monitoring.forward-logs=true)의
//! 새 로그 줄을 주기적으로 가져와 Discord 웹훅으로 카드(embed)를 보낸다.
//!
//! 채널: 스트림 라벨 `channel`(컨테이너 라벨 monitoring.channel)이 있으면 `DISCORD_WEBHOOK_URL_<CHANNEL>`,
//! 없거나 그 변수가 비어 있으면 `DISCORD_WEBHOOK_URL`로 보낸다.
//! 시작 시점 이후의 로그만 보낸다 (재시작 중에 난 로그는 건너뛴다).

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const QUERY: &str = r#"{forward_logs="true"}"#;
/// Discord: 메시지당 embed 최대 10개
const MAX_EMBEDS: usize = 10;
const MAX_LINES_PER_POLL: usize = 500;
/// Loki 수집 지연(수 초)을 감안해 이만큼 전까지만 읽는다. 늦게 들어온 줄을 놓치지 않으려는 것.
const INGEST_LAG_NS: i128 = 5_000_000_000;

#[derive(Debug, Clone)]
struct Entry {
    ts_ns: i128,
    line: String,
    labels: BTreeMap<String, String>,
}

fn log(level: &str, msg: &str, kv: &[(&str, String)]) {
    let mut s = format!("level={level} msg={msg:?}");
    for (k, v) in kv {
        s.push_str(&format!(" {k}={v:?}"));
    }
    println!("{s}");
}

fn now_ns() -> i128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i128).unwrap_or(0)
}

fn env_or(key: &str, def: &str) -> String {
    env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| def.to_string())
}

// ------------------------------------------------------------------ logfmt

/// key=value / key="quoted value" 쌍을 순서대로 뽑는다. logfmt가 아니면 None.
fn parse_logfmt(line: &str) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut s = line.trim();
    while !s.is_empty() {
        let eq = s.find('=')?;
        if eq == 0 || s[..eq].contains(' ') {
            return None;
        }
        let key = s[..eq].to_string();
        s = &s[eq + 1..];
        let val;
        if let Some(rest) = s.strip_prefix('"') {
            let mut v = String::new();
            let mut chars = rest.char_indices();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => match chars.next() {
                        Some((_, 'n')) => v.push('\n'),
                        Some((_, 't')) => v.push('\t'),
                        Some((_, e)) => v.push(e),
                        None => return None,
                    },
                    '"' => {
                        end = Some(i);
                        break;
                    }
                    _ => v.push(c),
                }
            }
            let end = end?;
            val = v;
            s = &rest[end + 1..];
        } else {
            let end = s.find(' ').unwrap_or(s.len());
            val = s[..end].to_string();
            s = &s[end..];
        }
        out.push((key, val));
        s = s.trim_start();
    }
    Some(out)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n - 1).collect();
    t.push('…');
    t
}

// ------------------------------------------------------------------ 시간

/// Unix 나노초 → RFC3339 UTC (Discord embed timestamp). chrono 없이 날짜 계산.
fn rfc3339(ts_ns: i128) -> String {
    let secs = ts_ns.div_euclid(1_000_000_000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Howard Hinnant의 civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}

// ------------------------------------------------------------------ Discord 카드

fn to_embed(e: &Entry) -> Value {
    let level = e.labels.get("level").map(String::as_str).unwrap_or("");
    let (color, icon) = match level {
        "info" => (3_447_003, "🔵"),
        "warn" => (16_098_851, "⚠️"),
        "error" | "fatal" => (15_548_997, "🚨"),
        _ => (9_807_270, "📄"),
    };
    let container = e.labels.get("container").cloned().unwrap_or_default();
    let service = e.labels.get("service").filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| container.clone());

    let (title, description, fields) = match parse_logfmt(&e.line) {
        None => (format!("{icon} {service}"), Some(format!("```\n{}\n```", e.line)), vec![]),
        Some(kv) => {
            let mut msg = String::new();
            let mut fields = Vec::new();
            for (k, v) in kv {
                match k.as_str() {
                    "time" | "ts" | "level" | "lvl" => {}
                    "msg" | "message" => msg = v,
                    _ if !v.is_empty() && fields.len() < 25 => fields.push(json!({
                        "name": truncate(&k, 256),
                        "value": truncate(&format!("`{v}`"), 1024),
                        "inline": true,
                    })),
                    _ => {}
                }
            }
            if msg.is_empty() {
                msg = level.to_string();
            }
            (format!("{icon} {service} · {msg}"), None, fields)
        }
    };

    let mut em = json!({
        "title": truncate(&title, 256),
        "color": color,
        "footer": { "text": container },
        "timestamp": rfc3339(e.ts_ns),
    });
    if let Some(d) = description {
        em["description"] = json!(truncate(&d, 4000));
    }
    if !fields.is_empty() {
        em["fields"] = json!(fields);
    }
    em
}

fn webhook_for(channel: Option<&str>) -> Option<String> {
    if let Some(ch) = channel.filter(|c| !c.is_empty()) {
        let key = format!("DISCORD_WEBHOOK_URL_{}", ch.to_uppercase().replace(['-', '.'], "_"));
        if let Some(v) = env::var(key).ok().filter(|v| !v.is_empty()) {
            return Some(v);
        }
    }
    env::var("DISCORD_WEBHOOK_URL").ok().filter(|v| !v.is_empty())
}

// ------------------------------------------------------------------ Loki / Discord 호출

struct Forwarder {
    loki: String,
    agent: ureq::Agent,
    /// 이미 보낸 마지막 로그 시각 (ns)
    cursor: i128,
}

impl Forwarder {
    fn fetch(&self, end: i128) -> Result<Vec<Entry>, String> {
        let resp = self
            .agent
            .get(&format!("{}/loki/api/v1/query_range", self.loki))
            .query("query", QUERY)
            .query("start", &(self.cursor + 1).to_string())
            .query("end", &end.to_string())
            .query("direction", "forward")
            .query("limit", &MAX_LINES_PER_POLL.to_string())
            .call()
            .map_err(|e| e.to_string())?;
        let body: Value = resp.into_json().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in body["data"]["result"].as_array().into_iter().flatten() {
            let labels: BTreeMap<String, String> = r["stream"]
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
                .unwrap_or_default();
            for v in r["values"].as_array().into_iter().flatten() {
                let (Some(ts), Some(line)) = (v[0].as_str().and_then(|s| s.parse::<i128>().ok()), v[1].as_str()) else {
                    continue;
                };
                out.push(Entry { ts_ns: ts, line: line.to_string(), labels: labels.clone() });
            }
        }
        out.sort_by_key(|e| e.ts_ns);
        Ok(out)
    }

    /// embed를 10개씩 나눠 보낸다. 429면 retry_after만큼 기다렸다가 다시 보낸다 (최대 3번).
    fn send(&self, hook: &str, embeds: &[Value]) -> Result<(), String> {
        for chunk in embeds.chunks(MAX_EMBEDS) {
            let body = json!({ "username": "Homeserver logs", "embeds": chunk });
            let mut attempt = 0;
            loop {
                match self.agent.post(hook).send_json(&body) {
                    Ok(_) => break,
                    Err(ureq::Error::Status(429, resp)) if attempt < 3 => {
                        let wait = resp
                            .into_json::<Value>()
                            .ok()
                            .and_then(|v| v["retry_after"].as_f64())
                            .unwrap_or(1.0);
                        log("WARN", "discord rate limited", &[("wait_s", format!("{wait:.2}"))]);
                        sleep(Duration::from_secs_f64(wait + 0.1));
                        attempt += 1;
                    }
                    Err(ureq::Error::Status(code, resp)) => {
                        let text = resp.into_string().unwrap_or_default();
                        return Err(format!("discord {code}: {}", truncate(&text, 300)));
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
        Ok(())
    }

    fn poll(&mut self) {
        let end = now_ns() - INGEST_LAG_NS;
        if end <= self.cursor {
            return;
        }
        let entries = match self.fetch(end) {
            Ok(v) => v,
            Err(e) => {
                log("ERROR", "loki query failed", &[("err", e)]);
                return;
            }
        };

        // 웹훅별로 모으되 처음 나온 순서를 지킨다
        let mut by_hook: Vec<(String, Vec<Value>)> = Vec::new();
        for e in &entries {
            let Some(hook) = webhook_for(e.labels.get("channel").map(String::as_str)) else {
                continue;
            };
            match by_hook.iter_mut().find(|(h, _)| *h == hook) {
                Some((_, v)) => v.push(to_embed(e)),
                None => by_hook.push((hook, vec![to_embed(e)])),
            }
        }
        for (hook, embeds) in &by_hook {
            // 실패한 줄은 다시 보내지 않는다 (같은 줄이 반복 전송되는 것보다 낫다).
            if let Err(e) = self.send(hook, embeds) {
                log("ERROR", "discord send failed", &[("err", e), ("lines", embeds.len().to_string())]);
            }
        }

        self.cursor = match entries.last() {
            // 한도까지 받았으면 받은 마지막 줄 다음부터 이어 읽는다.
            Some(last) if entries.len() == MAX_LINES_PER_POLL => last.ts_ns,
            _ => end,
        };
        if !entries.is_empty() {
            log("INFO", "forwarded", &[("lines", entries.len().to_string())]);
        }
    }
}

fn parse_duration(s: &str) -> Option<Duration> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        _ => None,
    }
}

fn main() {
    let interval_s = env_or("POLL_INTERVAL", "15s");
    let Some(interval) = parse_duration(&interval_s) else {
        log("ERROR", "bad POLL_INTERVAL", &[("value", interval_s)]);
        std::process::exit(1);
    };
    let mut f = Forwarder {
        loki: env_or("LOKI_URL", "http://loki:3100").trim_end_matches('/').to_string(),
        agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build(),
        cursor: now_ns() - INGEST_LAG_NS,
    };
    log(
        "INFO",
        "started",
        &[
            ("loki", f.loki.clone()),
            ("interval", interval_s),
            ("default_webhook", webhook_for(None).is_some().to_string()),
        ],
    );
    loop {
        sleep(interval);
        f.poll();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logfmt() {
        let kv = parse_logfmt(
            r#"time=2026-11-20T02:51:41.329Z level=INFO msg="record up to date" name=geun.me action=unchanged ip=59.13.60.72"#,
        )
        .unwrap();
        let keys: Vec<_> = kv.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["time", "level", "msg", "name", "action", "ip"]);
        assert_eq!(kv[2].1, "record up to date");
        assert!(parse_logfmt("plain text line").is_none());
        let kv = parse_logfmt(r#"msg="say \"hi\"" k=v"#).unwrap();
        assert_eq!(kv[0].1, r#"say "hi""#);
    }

    #[test]
    fn embed() {
        let labels = BTreeMap::from([
            ("level".into(), "info".into()),
            ("service".into(), "dns-updater".into()),
            ("container".into(), "dns-updater-dns-updater-1".into()),
        ]);
        let e = to_embed(&Entry {
            ts_ns: 0,
            line: r#"time=x level=INFO msg="record updated" name=geun.me action=updated ip=1.2.3.4"#.into(),
            labels,
        });
        assert_eq!(e["title"], "🔵 dns-updater · record updated");
        assert_eq!(e["fields"].as_array().unwrap().len(), 3);
        assert_eq!(e["fields"][2]["value"], "`1.2.3.4`");
        assert_eq!(e["timestamp"], "1970-01-01T00:00:00Z");
    }

    #[test]
    fn time_format() {
        assert_eq!(rfc3339(1_763_607_101_000_000_000), "2025-11-20T02:51:41Z");
        assert_eq!(rfc3339(951_782_400_000_000_000), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn routing() {
        env::set_var("DISCORD_WEBHOOK_URL", "default");
        env::set_var("DISCORD_WEBHOOK_URL_DNS_UPDATER", "dns");
        assert_eq!(webhook_for(Some("dns-updater")).as_deref(), Some("dns"));
        assert_eq!(webhook_for(Some("other")).as_deref(), Some("default"));
        assert_eq!(webhook_for(None).as_deref(), Some("default"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("15s"), Some(Duration::from_secs(15)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("x"), None);
    }
}
