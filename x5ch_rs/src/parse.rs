//! スレッドHTML→Post変換。Python版 parse.py に対応。
//!
//! Python版は selectolax(CSSセレクタでDOMを辿れるHTMLパーサ)を使っていた。
//! Rustでは同じ立ち位置の scraper クレート(内部はブラウザ級のパーサ html5ever)を使う。
//!
//! 正規表現まわりで1つ、Pythonとの大きな違いに当たった:
//! RustのregexクレートはPythonのreと違って「後読み(lookbehind, (?<!...))」を
//! サポートしていない。これは意図的な設計で、regexクレートは「入力長に対して
//! 必ず線形時間で終わる」ことを保証するため、バックトラックを伴う高度な構文を
//! 最初から採用していない。今回はアンチリンク復元(H_RESTORE_PATTERN)がこれに
//! 該当したので、マッチ位置の1文字前を自分でチェックする形に書き換えた。

use once_cell::sync::Lazy;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

use crate::models::Post;

/// 他モジュール(export.rs)からも参照される汎用のタグ除去パターン。
pub static HTML_TAG_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());

/// 5chの「アンチリンク」表記(先頭hを1文字落とした ttp(s)://)を検出するパターン。
/// 後読みが使えないので、ここでは「hを含まない」判定は呼び出し側で別途行う。
static H_RESTORE_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"ttps?://").unwrap());

/// レス1件分を表すdiv要素(class属性に'clear post'を含む)を列挙する。
pub fn post_nodes(document: &Html) -> Vec<ElementRef<'_>> {
    let selector = Selector::parse("div").unwrap();
    document
        .select(&selector)
        .filter(|node| {
            node.value()
                .attr("class")
                .map(|cls| cls.contains("clear post"))
                .unwrap_or(false)
        })
        .collect()
}

/// scraperのElementRefから、子要素を含む生のinner HTMLを取り出す。
/// Python版と同じく「開始タグの'>'から終了タグの'<'まで」を文字列操作で切り出す方式。
pub fn inner_html(node: Option<ElementRef>) -> String {
    let Some(node) = node else {
        return String::new();
    };
    let outer = node.html();
    let start = outer.find('>');
    let end = outer.rfind('<');
    match (start, end) {
        (Some(s), Some(e)) if e > s => outer[s + 1..e].to_string(),
        _ => String::new(),
    }
}

/// Python版の (?<!h)(ttps?://) 相当。マッチ直前の1文字を手動で見て、'h'でなければ補う。
pub fn restore_h_prefix(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;

    for m in H_RESTORE_PATTERN.find_iter(text) {
        result.push_str(&text[last_end..m.start()]);
        let preceded_by_h = text[..m.start()].chars().next_back() == Some('h');
        if !preceded_by_h {
            result.push('h');
        }
        result.push_str(m.as_str());
        last_end = m.end();
    }
    result.push_str(&text[last_end..]);
    result
}

/// 本文HTML断片を、Python版と同じ手順でプレーンテキストに整形する。
fn clean_content(content_html: &str) -> String {
    let text = content_html.replace("<br>", "\n");
    let text = HTML_TAG_PATTERN.replace_all(&text, " ");
    let text = html_escape::decode_html_entities(&text).to_string();
    let text = text.trim();
    restore_h_prefix(text)
}

/// scraperの.text()イテレータで取れる断片を連結し、前後の空白を落とす。
/// Python版の node.text(strip=True) に対応するヘルパー。
fn node_text(node: ElementRef) -> String {
    node.text().collect::<String>().trim().to_string()
}

/// スレッドHTMLから軽量なPost一覧(TUI/search/read等の用途向け)を抽出する。
pub fn parse_posts(html_content: &str) -> Vec<Post> {
    let document = Html::parse_document(html_content);

    let postid_sel = Selector::parse("span.postid").unwrap();
    let username_sel = Selector::parse("span.postusername").unwrap();
    let date_sel = Selector::parse("span.date").unwrap();
    let uid_sel = Selector::parse("span.uid").unwrap();
    let content_sel = Selector::parse("div.post-content").unwrap();

    let mut posts = Vec::new();

    for node in post_nodes(&document) {
        let Some(id_node) = node.select(&postid_sel).next() else {
            continue;
        };
        let Ok(num) = node_text(id_node).parse::<u32>() else {
            continue;
        };

        let mut name = "名無し".to_string();
        if let Some(name_node) = node.select(&username_sel).next() {
            let cleaned = HTML_TAG_PATTERN
                .replace_all(&inner_html(Some(name_node)), "")
                .trim()
                .to_string();
            if !cleaned.is_empty() {
                name = cleaned;
            }
        }

        let date_part = node
            .select(&date_sel)
            .next()
            .map(node_text)
            .unwrap_or_default();
        let uid_part = node
            .select(&uid_sel)
            .next()
            .map(node_text)
            .unwrap_or_default();
        let date = format!("{date_part} {uid_part}");

        let content_node = node.select(&content_sel).next();
        let message = clean_content(&inner_html(content_node));

        posts.push(Post {
            num,
            name,
            date,
            message,
        });
    }

    posts
}
