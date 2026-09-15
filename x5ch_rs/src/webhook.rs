//! Discord Webhook送信。Python版 webhook.py に対応。
//!
//! discord.py(Bot API方式)とは独立した、より簡易な送信経路。Botトークンも
//! スレッド作成も不要で、指定したチャンネルのWebhook URLへ直接POSTするだけ。

use std::collections::HashMap;
use std::time::Duration;

use crate::fetch::Fetcher;
use crate::models::Post;

pub const MESSAGE_INTERVAL: Duration = Duration::from_secs(1);

/// webhook URL一覧を読み込む。JSON配列・改行区切りテキストのどちらにも対応する
/// (`#`始まりの行はコメントとして無視)。ファイルが無ければ空リストを返す。
pub fn load_webhook_urls(path: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }

    if let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(text) {
        return items
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
    }

    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// discord.py(Bot API版)と同じフォーマット。
pub fn format_post(post: &Post) -> String {
    format!("**{}** : {} : {}\n{}", post.num, post.name, post.date, post.message)
}

/// 文字数基準でn文字ずつのチャンクに分割する。
/// Pythonの `s[i:i+n]` はUTF-16的なコードポイント単位のスライスだが、
/// Rustの `&str` はバイト境界でしかスライスできない(UTF-8の途中でスライスすると
/// panicする)。なのでいったん `chars()` で文字単位のVecに変換してからchunksする。
pub fn split_by_runes(s: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(n.max(1)).map(|c| c.iter().collect()).collect()
}

async fn post_once(fetcher: &Fetcher, url: &str, content: &str) -> Result<(), String> {
    let body = serde_json::json!({ "content": content }).to_string();

    loop {
        let (status, text) = fetcher
            .post_json(url, body.clone())
            .await
            .map_err(|e| e.to_string())?;

        if status == 429 {
            let retry_after = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("retry_after").and_then(|r| r.as_f64()))
                .filter(|v| *v > 0.0)
                .unwrap_or(1.0);
            tokio::time::sleep(Duration::from_secs_f64(retry_after)).await;
            continue;
        }

        if status >= 400 {
            return Err(format!("Webhook送信エラー: {status} {text}"));
        }

        return Ok(());
    }
}

/// 1件のメッセージを1つのURLへ送信する(2000字超は自動分割)。
pub async fn send_to_webhook(fetcher: &Fetcher, url: &str, content: &str) -> Result<(), String> {
    if content.chars().count() <= 2000 {
        return post_once(fetcher, url, content).await;
    }

    let parts = split_by_runes(content, 1900);
    let total = parts.len();
    for (i, part) in parts.iter().enumerate() {
        let chunk = if i < total - 1 {
            format!("{part}\n(続く...)")
        } else {
            part.clone()
        };
        post_once(fetcher, url, &chunk).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(())
}

/// 1件の投稿を全URLへブロードキャストする。送信に失敗したURLの一覧を返す。
pub async fn broadcast_post(fetcher: &Fetcher, urls: &[String], post: &Post) -> Vec<String> {
    let content = format_post(post);
    let mut failed = Vec::new();
    for url in urls {
        if send_to_webhook(fetcher, url, &content).await.is_err() {
            failed.push(url.clone());
        }
    }
    failed
}

/// 複数の投稿を、1件ずつ全URLへ順にブロードキャストする。
/// 戻り値は {webhook_url: [送信失敗したpost.numのリスト]} の形。
pub async fn broadcast_posts(
    fetcher: &Fetcher,
    urls: &[String],
    posts: &[Post],
    interval: Duration,
) -> HashMap<String, Vec<u32>> {
    let mut failures: HashMap<String, Vec<u32>> =
        urls.iter().map(|u| (u.clone(), Vec::new())).collect();

    for post in posts {
        let failed_urls = broadcast_post(fetcher, urls, post).await;
        for u in failed_urls {
            failures.entry(u).or_default().push(post.num);
        }
        tokio::time::sleep(interval).await;
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_by_runes_splits_by_char_count() {
        let parts = split_by_runes("あいうえおかきくけこ", 3);
        assert_eq!(parts, vec!["あいう", "えおか", "きくけ", "こ"]);
    }

    #[test]
    fn format_post_matches_discord_layout() {
        let post = Post {
            num: 5,
            name: "名無しさん".to_string(),
            date: "2026/09/14 10:00:00".to_string(),
            message: "本文だよ".to_string(),
        };
        assert_eq!(
            format_post(&post),
            "**5** : 名無しさん : 2026/09/14 10:00:00\n本文だよ"
        );
    }

    #[test]
    fn load_webhook_urls_parses_json_array() {
        let path = std::env::temp_dir().join(format!("x5ch_rs_wh_json_{}.json", std::process::id()));
        std::fs::write(&path, r#"["https://a.example/", "https://b.example/", ""]"#).unwrap();
        let urls = load_webhook_urls(path.to_str().unwrap());
        assert_eq!(urls, vec!["https://a.example/", "https://b.example/"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_webhook_urls_parses_line_based_text_and_skips_comments() {
        let path = std::env::temp_dir().join(format!("x5ch_rs_wh_txt_{}.txt", std::process::id()));
        std::fs::write(&path, "# comment\nhttps://a.example/\n\nhttps://b.example/\n").unwrap();
        let urls = load_webhook_urls(path.to_str().unwrap());
        assert_eq!(urls, vec!["https://a.example/", "https://b.example/"]);
        let _ = std::fs::remove_file(&path);
    }
}
