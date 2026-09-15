//! 板メニュー取得。Python版 menu.py に対応。
//!
//! ここでの設計方針: 「ネットワーク取得(I/O)」と「取得済みテキストの解析(純粋な計算)」を
//! はっきり分ける。get_menu_from_json/html が I/O 担当、parse_menu_json/parse_menu_html が
//! 純粋関数。この分離のおかげで、5chに実際にアクセスできないこのサンドボックスでも
//! parse側だけは #[cfg(test)] でユニットテストできる(cargo testで実行可能)。

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;

use crate::errors::MenuError;
use crate::fetch::Fetcher;
use crate::models::{Board, Category};

pub const MENU_URL_JSON: &str = "https://menu.5ch.io/bbsmenu.json";
pub const MENU_URL_HTML: &str = "https://menu.5ch.io/bbsmenu.html";

static HTML_MENU_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)(?:<B>([^<]+)</B>)|(?:<A HREF=["']?([^ >"']+)["']?[^>]*>([^<]+)</A>)"#)
        .unwrap()
});

/// CP932(Shift_JIS)バイト列をUTF-8文字列に変換する。
///
/// Python版は「cp932でstrict decode試行→失敗ならutf-8+replaceにフォールバック」だが、
/// encoding_rsはUnicode標準(WHATWG)準拠の非strictデコーダで、無効バイトをその場で
/// 置換文字に変えてしまう設計。ここでは had_errors フラグを見て、エラーがあった場合のみ
/// utf-8フォールバックする形でPython版の意図を再現する(完全に同一の挙動ではない点に注意)。
pub fn decode_to_utf8(data: &[u8]) -> String {
    let (cow, _, had_errors) = encoding_rs::SHIFT_JIS.decode(data);
    if !had_errors {
        return cow.into_owned();
    }
    String::from_utf8_lossy(data).into_owned()
}

#[derive(Debug, Deserialize, Default)]
struct MenuJson {
    menu_list: Option<Vec<MenuCategoryJson>>,
}

#[derive(Debug, Deserialize, Default)]
struct MenuCategoryJson {
    category_name: Option<String>,
    category_content: Option<Vec<MenuBoardJson>>,
}

#[derive(Debug, Deserialize, Default)]
struct MenuBoardJson {
    board_name: Option<String>,
    url: Option<String>,
}

/// bbsmenu.jsonのテキストをCategory一覧に変換する(純粋関数)。
pub fn parse_menu_json(text: &str) -> Result<Vec<Category>, MenuError> {
    let data: MenuJson =
        serde_json::from_str(text).map_err(|e| MenuError(format!("JSON解析エラー: {e}")))?;

    let mut categories = Vec::new();
    for cat in data.menu_list.unwrap_or_default() {
        let mut boards = Vec::new();
        for b in cat.category_content.unwrap_or_default() {
            let Some(url) = b.url else { continue };
            boards.push(Board {
                title: b.board_name.unwrap_or_default(),
                url: normalize_menu_url(&url),
            });
        }
        if !boards.is_empty() {
            categories.push(Category {
                title: cat.category_name.unwrap_or_default(),
                boards,
            });
        }
    }
    Ok(categories)
}

/// bbsmenu.htmlのテキストをCategory一覧に変換する(純粋関数)。
pub fn parse_menu_html(html_content: &str) -> Vec<Category> {
    let mut categories = Vec::new();
    let mut current_category = String::new();
    let mut current_boards: Vec<Board> = Vec::new();
    let mut has_category = false;

    for caps in HTML_MENU_PATTERN.captures_iter(html_content) {
        if let Some(cat_name) = caps.get(1) {
            if has_category && !current_boards.is_empty() {
                categories.push(Category {
                    title: current_category.clone(),
                    boards: std::mem::take(&mut current_boards),
                });
            }
            current_category = cat_name.as_str().trim().to_string();
            has_category = true;
            continue;
        }

        if let Some(href) = caps.get(2) {
            let href = href.as_str();
            let allowed = ["5ch.io", "5ch.net", "2ch.net", "bbspink.com"];
            if !allowed.iter().any(|d| href.contains(d)) {
                continue;
            }
            if has_category {
                let title = caps.get(3).map(|m| m.as_str()).unwrap_or("").trim().to_string();
                current_boards.push(Board {
                    title,
                    url: normalize_menu_url(href),
                });
            }
        }
    }

    if has_category && !current_boards.is_empty() {
        categories.push(Category {
            title: current_category,
            boards: current_boards,
        });
    }

    categories
}

pub fn normalize_menu_url(url: &str) -> String {
    let mut url = url.replace("2ch.net", "5ch.io").replace("5ch.net", "5ch.io");
    if let Some(rest) = url.strip_prefix("http:") {
        url = format!("https:{rest}");
    }
    if !url.ends_with('/') {
        url.push('/');
    }
    url
}

async fn get_menu_from_json(fetcher: &Fetcher, url: &str) -> Result<Vec<Category>, MenuError> {
    let (body, _) = fetcher
        .fetch(url)
        .await
        .map_err(|e| MenuError(e.to_string()))?;
    let text = String::from_utf8(body.clone()).unwrap_or_else(|_| decode_to_utf8(&body));
    parse_menu_json(&text)
}

async fn get_menu_from_html(fetcher: &Fetcher, url: &str) -> Result<Vec<Category>, MenuError> {
    let (body, _) = fetcher
        .fetch(url)
        .await
        .map_err(|e| MenuError(e.to_string()))?;
    Ok(parse_menu_html(&decode_to_utf8(&body)))
}

/// 板メニューをJSON優先・HTML fallbackで取得する。
///
/// Python版は `except Exception: pass` で「何が起きても握りつぶして次へ」だったが、
/// Rustでは `Result` を握りつぶすのに例外機構は要らない。`if let Ok(...)` で
/// 「成功した場合だけ中身を見る、失敗は素通り」と書けば同じ効果になる。
pub async fn get_menu(fetcher: &Fetcher) -> Result<Vec<Category>, MenuError> {
    if let Ok(cats) = get_menu_from_json(fetcher, MENU_URL_JSON).await {
        if !cats.is_empty() {
            return Ok(cats);
        }
    }

    if let Ok(cats) = get_menu_from_html(fetcher, MENU_URL_HTML).await {
        if !cats.is_empty() {
            return Ok(cats);
        }
    }

    Err(MenuError(
        "メニューの取得に失敗しました(JSON/HTML両方とも失敗)".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_menu_url_upgrades_and_appends_slash() {
        assert_eq!(
            normalize_menu_url("http://egg.5ch.net/livejupiter"),
            "https://egg.5ch.io/livejupiter/"
        );
    }

    #[test]
    fn parse_menu_json_extracts_categories_and_skips_empty_ones() {
        let sample = r#"{
            "menu_list": [
                {
                    "category_name": "ニュース",
                    "category_content": [
                        {"board_name": "なんJ", "url": "http://egg.5ch.net/livejupiter/"}
                    ]
                },
                {
                    "category_name": "空カテゴリ",
                    "category_content": []
                }
            ]
        }"#;

        let categories = parse_menu_json(sample).unwrap();
        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].title, "ニュース");
        assert_eq!(categories[0].boards[0].title, "なんJ");
        assert_eq!(categories[0].boards[0].url, "https://egg.5ch.io/livejupiter/");
    }

    #[test]
    fn parse_menu_html_groups_boards_under_category() {
        let sample = r#"
            <B>ニュース</B>
            <A HREF="http://egg.5ch.net/livejupiter/">なんJ</A>
            <A HREF="http://example.com/">対象外ドメイン</A>
        "#;

        let categories = parse_menu_html(sample);
        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].title, "ニュース");
        assert_eq!(categories[0].boards.len(), 1);
        assert_eq!(categories[0].boards[0].title, "なんJ");
    }
}
