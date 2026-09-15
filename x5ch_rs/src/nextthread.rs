//! 次スレ検出。Python版 nextthread.py に対応。

use once_cell::sync::Lazy;
use regex::Regex;

use crate::fetch::Fetcher;
use crate::menu::decode_to_utf8;
use crate::models::{Post, ThreadInfo};
use crate::threads::HistoryStore;

static CURRENT_BOARD_URL_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^https?://([^/]+)/([^/]+)/").unwrap());
static TITLE_TAG_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<title>(.*?)</title>").unwrap());
static TITLE_SUFFIX_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)\s*[-|]\s*5ch\.(net|io).*").unwrap());

/// 現スレの900番以降のレスから、次スレらしきdat番号を抜き出す(純粋関数)。
/// Python版は重複チェックをhistory.exists()呼び出しの中でしか行っていなかったが、
/// ここでは先にHashSetで重複を除いておくことで、同じスレへの無駄なfetchを減らしている。
pub fn find_next_thread_candidates(
    current_board_url: &str,
    current_dat_file: &str,
    posts: &[Post],
) -> Vec<String> {
    let Some(caps) = CURRENT_BOARD_URL_PATTERN.captures(current_board_url) else {
        return Vec::new();
    };
    let server = &caps[1];
    let board = &caps[2];

    let pattern = Regex::new(&format!(
        r"https?://{}/test/read\.cgi/{}/(\d+)/?",
        regex::escape(server),
        regex::escape(board)
    ))
    .unwrap();

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    for post in posts.iter().filter(|p| p.num >= 900) {
        for m in pattern.captures_iter(&post.message) {
            let dat_key = m[1].to_string();
            if format!("{dat_key}.dat") == current_dat_file {
                continue;
            }
            if seen.insert(dat_key.clone()) {
                result.push(dat_key);
            }
        }
    }
    result
}

/// <title>タグの中身から、末尾の "- 5ch.net" 等のサフィックスを除いたタイトルを取り出す(純粋関数)。
pub fn extract_title(html_content: &str) -> String {
    let Some(caps) = TITLE_TAG_PATTERN.captures(html_content) else {
        return String::new();
    };
    let title = caps[1].trim();
    TITLE_SUFFIX_PATTERN.replace(title, "").trim().to_string()
}

/// board_url + dat_key から read.cgi のタイトルを取りに行く。取得失敗時は空文字を返す
/// (Python版が `except FetchError: return ""` としていたのと同じ「失敗は握りつぶす」仕様)。
pub async fn fetch_thread_title(fetcher: &Fetcher, board_url: &str, dat_key: &str) -> String {
    let Ok(uri) = url::Url::parse(board_url) else {
        return String::new();
    };
    let segments: Vec<&str> = uri
        .path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let Some(board_name) = segments.last() else {
        return String::new();
    };

    let scheme = uri.scheme();
    let host = uri.host_str().unwrap_or("");
    let port = uri.port().map(|p| format!(":{p}")).unwrap_or_default();
    let read_url = format!("{scheme}://{host}{port}/test/read.cgi/{board_name}/{dat_key}/");

    let Ok((body, _)) = fetcher.fetch(&read_url).await else {
        return String::new();
    };

    extract_title(&decode_to_utf8(&body))
}

/// 現スレのレスから次スレ候補を検出し、未登録なら履歴に追加する。
pub async fn detect_and_add_next_thread<H: HistoryStore>(
    fetcher: &Fetcher,
    history: &H,
    posts: &[Post],
    current: &ThreadInfo,
) {
    for dat_key in find_next_thread_candidates(&current.board_url, &current.dat_file, posts) {
        let dat_file = format!("{dat_key}.dat");

        if history.exists(&current.board_url, &dat_file).await {
            continue;
        }

        let title = fetch_thread_title(fetcher, &current.board_url, &dat_key).await;
        if !title.is_empty() {
            history.add_new_thread(&title, &current.board_url, &dat_file).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(num: u32, message: &str) -> Post {
        Post {
            num,
            name: "名無し".to_string(),
            date: String::new(),
            message: message.to_string(),
        }
    }

    #[test]
    fn extract_title_leaves_non_hyphen_suffix_untouched() {
        // サフィックスパターンは "- 5ch.net" 形式(ハイフン/パイプ区切り)のみ対象。
        // "@5ch.net" のような区切りは対象外で、そのまま残る仕様(Python版と同じ)。
        let html = "<html><head><title>移植スレ part2 - なんでも実況J@5ch.net</title></head></html>";
        assert_eq!(extract_title(html), "移植スレ part2 - なんでも実況J@5ch.net");
    }

    #[test]
    fn extract_title_strips_5ch_suffix_hyphen_form() {
        let html = "<title>移植スレ part2 - 5ch.net</title>";
        assert_eq!(extract_title(html), "移植スレ part2");
    }

    #[test]
    fn find_next_thread_candidates_detects_from_late_posts_only() {
        let posts = vec![
            post(899, "https://egg.5ch.net/test/read.cgi/livejupiter/1700000999/ 見えないはず"),
            post(900, "次スレ https://egg.5ch.net/test/read.cgi/livejupiter/1700001000/"),
            post(901, "https://egg.5ch.net/test/read.cgi/livejupiter/1700001000/ また同じ"),
        ];

        let candidates =
            find_next_thread_candidates("https://egg.5ch.net/livejupiter/", "1699999999.dat", &posts);

        assert_eq!(candidates, vec!["1700001000".to_string()]);
    }
}
