//! 全板横断検索。Python版 search.py に対応。

use once_cell::sync::Lazy;
use regex::Regex;

use crate::errors::SearchError;
use crate::fetch::Fetcher;
use crate::models::ThreadInfo;
use crate::parse::HTML_TAG_PATTERN;
use crate::threads::HistoryStore;

pub const SEARCH_BASE_URL: &str = "https://ff5ch.syoboi.jp/?q=";

static SEARCH_RESULT_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)<a\s+[^>]*href="(https?://[^.]+\.5ch\.(?:net|io)/test/read\.cgi/[^/]+/\d+/?)"[^>]*>(.+?)</a>"#,
    )
    .unwrap()
});
static THREAD_URL_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"https?://([^.]+)\.5ch\.(?:net|io)/test/read\.cgi/([^/]+)/(\d+)/?").unwrap());
static TITLE_COUNT_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(.*)\((\d+)\)$").unwrap());

/// ff5chはUTF-8で応答するため、妥当性チェックのみ行う(不正バイトは置換文字に)。
pub fn to_valid_utf8(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// Pythonの urllib.parse.quote_plus 相当。スペースは'+'にエンコードする
/// application/x-www-form-urlencoded 形式を、既に依存に入っている url クレートの
/// form_urlencoded モジュールでそのまま再現できる。
pub fn quote_plus(keyword: &str) -> String {
    url::form_urlencoded::byte_serialize(keyword.as_bytes()).collect()
}

/// 1件の検索結果。I/O(fetch)と切り離した純粋データ。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub dat_file: String,
    pub title: String,
    pub count: u32,
    pub board_url: String,
    pub url: String,
}

/// 検索結果HTMLをSearchHit一覧に変換する(純粋関数)。
pub fn parse_search_results(html_content: &str) -> Vec<SearchHit> {
    let mut results = Vec::new();

    for caps in SEARCH_RESULT_PATTERN.captures_iter(html_content) {
        let full_url = &caps[1];
        let raw_title = &caps[2];

        let Some(url_parts) = THREAD_URL_PATTERN.captures(full_url) else {
            continue;
        };
        let server = &url_parts[1];
        let board_name = &url_parts[2];
        let dat_num = &url_parts[3];

        let mut title = HTML_TAG_PATTERN.replace_all(raw_title, "").trim().to_string();
        let mut count = 0u32;
        if let Some(cm) = TITLE_COUNT_PATTERN.captures(&title.clone()) {
            title = cm[1].trim().to_string();
            count = cm[2].parse().unwrap_or(0);
        }

        results.push(SearchHit {
            dat_file: format!("{dat_num}.dat"),
            title,
            count,
            board_url: format!("https://{server}.5ch.io/{board_name}/"),
            url: format!("https://{server}.5ch.io/test/read.cgi/{board_name}/{dat_num}/"),
        });
    }

    results
}

/// キーワードでff5ch経由の全板横断検索を行う。
pub async fn search_global<H: HistoryStore>(
    fetcher: &Fetcher,
    history: &H,
    keyword: &str,
) -> Result<Vec<ThreadInfo>, SearchError> {
    let search_url = format!("{SEARCH_BASE_URL}{}", quote_plus(keyword));

    let (body, _) = fetcher
        .fetch(&search_url)
        .await
        .map_err(|e| SearchError(format!("検索エラー: {e}")))?;

    let html_content = to_valid_utf8(&body);

    let mut results = Vec::new();
    for hit in parse_search_results(&html_content) {
        let last_read = history.get_last_read(&hit.board_url, &hit.dat_file).await;
        results.push(ThreadInfo {
            dat_file: hit.dat_file,
            title: hit.title,
            count: hit.count,
            ikioi: 0.0,
            board_url: hit.board_url,
            last_read,
            url: hit.url,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_plus_encodes_spaces_as_plus() {
        assert_eq!(quote_plus("Rust 移植"), "Rust+%E7%A7%BB%E6%A4%8D");
    }

    #[test]
    fn parse_search_results_extracts_title_and_count() {
        let sample = r#"<a href="https://egg.5ch.net/test/read.cgi/livejupiter/1700000000/">なんJ移植スレ(123)</a>"#;
        let hits = parse_search_results(sample);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "なんJ移植スレ");
        assert_eq!(hits[0].count, 123);
        assert_eq!(hits[0].dat_file, "1700000000.dat");
        assert_eq!(hits[0].board_url, "https://egg.5ch.io/livejupiter/");
    }
}
