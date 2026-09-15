//! アーカイブ用エクスポート。Python版 export.py に対応。

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;

use crate::parse::{inner_html, post_nodes, restore_h_prefix, HTML_TAG_PATTERN};

static MAIL_LINK_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"<a\s+[^>]*href="/cdn-cgi/l/email-protection#([0-9a-fA-F]+)"[^>]*>(.*?)</a>"#)
        .unwrap()
});
static REPLY_LINK_TAG_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"<a[^>]*class="reply_link"[^>]*>"#).unwrap());
static HREF_NUM_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r#"href="[^"]*?/(\d+)""#).unwrap());
static POSTED_AT_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(\d{4})/(\d{2})/(\d{2})\([月火水木金土日]\)\s+(\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?")
        .unwrap()
});
static LD_JSON_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?s)<script\s+type="application/ld\+json">(.*?)</script>"#).unwrap());

fn jst() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).unwrap()
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportPost {
    pub external_id: String,
    pub num: u32,
    pub author_name_display: String,
    pub user_id: String,
    pub posted_at_raw: String,
    pub body_raw: String,
    pub body_display: String,
    pub body_html_original: String,
    pub mail_encoded: Option<String>,
    pub mail_decoded: Option<String>,
    pub posted_at: Option<String>,
    pub reply_to: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportThread {
    pub external_id: String,
    pub title: String,
    pub post_count: usize,
    pub board_name: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportSource {
    pub provider: String,
    pub board_url: String,
    pub dat_file: String,
    pub thread_url: String,
    pub scraped_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportResult {
    pub source: ExportSource,
    pub thread: ExportThread,
    pub posts: Vec<ExportPost>,
}

impl ExportResult {
    pub fn to_pretty_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// このJSON構造(source/thread/posts)にできるだけ対応させたMarkdown表現を作る。
    pub fn to_markdown(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("# {}\n", self.thread.title));
        lines.push(format!(
            "- 板: {}",
            self.thread.board_name.as_deref().unwrap_or("(不明)")
        ));
        lines.push(format!("- board_url: {}", self.source.board_url));
        lines.push(format!("- thread_url: {}", self.source.thread_url));
        lines.push(format!("- dat_file: {}", self.source.dat_file));
        lines.push(format!("- external_id: {}", self.thread.external_id));
        lines.push(format!(
            "- 作成日時: {}",
            self.thread.created_at.as_deref().unwrap_or("(不明)")
        ));
        lines.push(format!("- 取得日時: {}", self.source.scraped_at));
        lines.push(format!("- レス数(全体): {}", self.thread.post_count));
        lines.push(format!("- レス数(このファイル): {}", self.posts.len()));
        lines.push("\n---\n".to_string());

        for p in &self.posts {
            let mut header = format!("## {} {}", p.num, p.author_name_display);
            if !p.user_id.is_empty() {
                header.push_str(&format!(" ID:{}", p.user_id));
            }
            header.push(' ');
            header.push_str(p.posted_at.as_deref().unwrap_or(&p.posted_at_raw));
            lines.push(format!("{header}\n"));

            if let Some(reply_to) = &p.reply_to {
                if !reply_to.is_empty() {
                    let joined = reply_to
                        .iter()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(format!("> Reply to: {joined}\n"));
                }
            }
            lines.push(format!("{}\n", p.body_display));
            lines.push("---\n".to_string());
        }

        lines.join("\n")
    }
}

/// CloudflareのメールXOR難読化(cdn-cgi/l/email-protection)をデコードする。
pub fn decode_cf_email(hex_str: &str) -> Option<String> {
    if hex_str.len() < 2 || hex_str.len() % 2 != 0 {
        return None;
    }
    let key = u8::from_str_radix(&hex_str[0..2], 16).ok()?;

    let mut raw = Vec::new();
    let bytes = hex_str.as_bytes();
    let mut i = 2;
    while i < bytes.len() {
        let byte_str = std::str::from_utf8(&bytes[i..i + 2]).ok()?;
        let byte = u8::from_str_radix(byte_str, 16).ok()?;
        raw.push(byte ^ key);
        i += 2;
    }

    Some(String::from_utf8_lossy(&raw).into_owned())
}

/// 本文HTML中の class="reply_link" アンカーから返信先レス番号を抽出する。
pub fn extract_reply_to(content_html: &str) -> Vec<u32> {
    let mut result = Vec::new();
    for tag_match in REPLY_LINK_TAG_PATTERN.find_iter(content_html) {
        if let Some(caps) = HREF_NUM_PATTERN.captures(tag_match.as_str()) {
            if let Ok(n) = caps[1].parse::<u32>() {
                result.push(n);
            }
        }
    }
    result
}

/// "2025/12/16(火) 05:05:09.95" 形式をISO8601(JST、ナノ秒精度相当)に変換する。
pub fn parse_posted_at(raw: &str) -> Option<String> {
    let caps = POSTED_AT_PATTERN.captures(raw)?;

    let year: i32 = caps[1].parse().ok()?;
    let month: u32 = caps[2].parse().ok()?;
    let day: u32 = caps[3].parse().ok()?;
    let hour: u32 = caps[4].parse().ok()?;
    let minute: u32 = caps[5].parse().ok()?;
    let second: u32 = caps[6].parse().ok()?;
    let frac = caps.get(7).map(|m| m.as_str());

    let mut micro_str = frac.unwrap_or("").to_string();
    micro_str.push_str("000000");
    let microsecond: u32 = micro_str[..6].parse().ok()?;

    let dt = jst()
        .with_ymd_and_hms(year, month, day, hour, minute, second)
        .single()?
        .checked_add_signed(chrono::Duration::microseconds(microsecond as i64))?;

    Some(format_rfc3339_nano(dt, frac))
}

fn format_rfc3339_nano(dt: DateTime<FixedOffset>, frac_digits: Option<&str>) -> String {
    let base = dt.format("%Y-%m-%dT%H:%M:%S").to_string();

    let mut frac_part = String::new();
    if let Some(frac) = frac_digits {
        let trimmed = frac.trim_end_matches('0');
        if !trimmed.is_empty() {
            frac_part = format!(".{trimmed}");
        }
    }

    let offset_seconds = dt.offset().local_minus_utc();
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let total_minutes = offset_seconds.abs() / 60;
    let (oh, om) = (total_minutes / 60, total_minutes % 60);

    format!("{base}{frac_part}{sign}{oh:02}:{om:02}")
}

/// 本文HTML断片を、タグ除去+エンティティデコードまでしたプレーンテキストにする
/// (h復元は行わない生のテキスト。h復元込みが必要な場合は別途 restore_h_prefix を通す)。
pub fn extract_plain_text(content_html: &str) -> String {
    let text = content_html.replace("<br>", "\n");
    let text = HTML_TAG_PATTERN.replace_all(&text, " ");
    let text = html_escape::decode_html_entities(&text).to_string();
    text.trim().to_string()
}

/// スレッドHTMLをアーカイブ用の完全な構造でパースする。
pub fn parse_posts_for_export(html_content: &str, thread_external_id: &str) -> Vec<ExportPost> {
    let document = scraper::Html::parse_document(html_content);

    let postid_sel = scraper::Selector::parse("span.postid").unwrap();
    let username_sel = scraper::Selector::parse("span.postusername").unwrap();
    let date_sel = scraper::Selector::parse("span.date").unwrap();
    let uid_sel = scraper::Selector::parse("span.uid").unwrap();
    let content_sel = scraper::Selector::parse("div.post-content").unwrap();

    let mut posts = Vec::new();

    for node in post_nodes(&document) {
        let Some(id_node) = node.select(&postid_sel).next() else {
            continue;
        };
        let Ok(num) = id_node.text().collect::<String>().trim().parse::<u32>() else {
            continue;
        };

        let mut author_display = "名無し".to_string();
        let mut mail_encoded = None;
        let mut mail_decoded = None;

        if let Some(name_node) = node.select(&username_sel).next() {
            let raw_name = inner_html(Some(name_node));
            if let Some(mm) = MAIL_LINK_PATTERN.captures(&raw_name) {
                let encoded = mm[1].to_string();
                author_display = HTML_TAG_PATTERN.replace_all(&mm[2], "").trim().to_string();
                mail_decoded = decode_cf_email(&encoded);
                mail_encoded = Some(encoded);
            } else {
                let cleaned = HTML_TAG_PATTERN.replace_all(&raw_name, "").trim().to_string();
                if !cleaned.is_empty() {
                    author_display = cleaned;
                }
            }
        }

        let posted_at_raw = node
            .select(&date_sel)
            .next()
            .map(|n| n.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        let user_id = node
            .select(&uid_sel)
            .next()
            .map(|n| {
                n.text()
                    .collect::<String>()
                    .trim()
                    .strip_prefix("ID:")
                    .unwrap_or(&n.text().collect::<String>())
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();

        let body_html = inner_html(node.select(&content_sel).next());
        let reply_to = extract_reply_to(&body_html);
        let body_raw = extract_plain_text(&body_html);
        let body_display = restore_h_prefix(&body_raw);

        posts.push(ExportPost {
            external_id: format!("{thread_external_id}#{num}"),
            num,
            author_name_display: author_display,
            user_id,
            posted_at_raw: posted_at_raw.clone(),
            body_raw,
            body_display,
            body_html_original: body_html.trim().to_string(),
            mail_encoded,
            mail_decoded,
            posted_at: parse_posted_at(&posted_at_raw),
            reply_to: if reply_to.is_empty() { None } else { Some(reply_to) },
        });
    }

    posts
}

/// ページ全体のHTMLからJSON-LDパンくずリストの板名(position=2)を抽出する。
pub fn extract_board_name(full_html: &str) -> String {
    let Some(caps) = LD_JSON_PATTERN.captures(full_html) else {
        return String::new();
    };
    let Ok(raw_items) = serde_json::from_str::<serde_json::Value>(&caps[1]) else {
        return String::new();
    };
    let Some(items) = raw_items.as_array() else {
        return String::new();
    };

    for item in items {
        if item.get("@type").and_then(|v| v.as_str()) != Some("BreadcrumbList") {
            continue;
        }
        let Some(elements) = item.get("itemListElement").and_then(|v| v.as_array()) else {
            continue;
        };
        for el in elements {
            if el.get("position").and_then(|v| v.as_i64()) == Some(2) {
                if let Some(name) = el.get("name").and_then(|v| v.as_str()) {
                    return name.to_string();
                }
            }
        }
    }

    String::new()
}

/// datファイル名(スレ立て時刻のUNIXタイムスタンプ)をISO8601(UTC)に変換する。
pub fn dat_timestamp_to_rfc3339(dat_file: &str) -> String {
    let ts_str = dat_file.strip_suffix(".dat").unwrap_or(dat_file);
    let Ok(ts) = ts_str.parse::<i64>() else {
        return String::new();
    };
    let Some(dt) = Utc.timestamp_opt(ts, 0).single() else {
        return String::new();
    };
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_cf_email_xor_decodes_hex() {
        // "a@b.com" を key=0x10 でXORしたものを復元できるか
        let key: u8 = 0x10;
        let plain = b"a@b.com";
        let mut hex = format!("{key:02x}");
        for b in plain {
            hex.push_str(&format!("{:02x}", b ^ key));
        }
        assert_eq!(decode_cf_email(&hex).as_deref(), Some("a@b.com"));
    }

    #[test]
    fn parse_posted_at_converts_to_jst_rfc3339() {
        let result = parse_posted_at("2025/12/16(火) 05:05:09.95").unwrap();
        assert_eq!(result, "2025-12-16T05:05:09.95+09:00");
    }

    #[test]
    fn dat_timestamp_to_rfc3339_converts_unix_time() {
        // 1700000000 = 2023-11-14T22:13:20Z
        assert_eq!(dat_timestamp_to_rfc3339("1700000000.dat"), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn extract_board_name_reads_breadcrumb_position_2() {
        let html = r#"<script type="application/ld+json">
        [{"@type":"BreadcrumbList","itemListElement":[
            {"position":1,"name":"5ch.net"},
            {"position":2,"name":"なんでも実況J"}
        ]}]
        </script>"#;
        assert_eq!(extract_board_name(html), "なんでも実況J");
    }
}
