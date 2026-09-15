//! Discord Bot API連携。Python版 discord.py に対応。
//!
//! webhook.rs(単純なURL直POST)とは独立した、より重厚な送信経路。
//! Botトークンでチャンネル内にスレッドを作成し、そのスレッドへメッセージを送る。

use std::time::Duration;

use crate::errors::DiscordAPIError;
use crate::fetch::Fetcher;
use crate::models::Post;
use crate::webhook::split_by_runes;

pub const API_BASE: &str = "https://discord.com/api/v10";

pub struct Manager {
    token: String,
    channel_id: String,
    api_base: String,
    fetcher: Fetcher,
}

impl Manager {
    pub fn new(token: impl Into<String>, channel_id: impl Into<String>) -> Self {
        Self::with_api_base(token, channel_id, API_BASE)
    }

    pub fn with_api_base(
        token: impl Into<String>,
        channel_id: impl Into<String>,
        api_base: impl Into<String>,
    ) -> Self {
        Self {
            token: token.into(),
            channel_id: channel_id.into(),
            api_base: api_base.into(),
            fetcher: Fetcher::new("x5ch_rs/0.1 (discord-bot)"),
        }
    }

    /// トークン/チャンネルIDが設定されているか(プレースホルダのままでないか)を確認する。
    pub fn enabled(&self) -> bool {
        if self.token.is_empty() || self.token.contains("YOUR_BOT_TOKEN") {
            return false;
        }
        !self.channel_id.is_empty()
    }

    /// 指定チャンネル内にスレッドを作成し、Discord側のスレッドIDを返す。
    pub async fn create_thread(&self, title: &str) -> Result<String, DiscordAPIError> {
        if !self.enabled() {
            return Err(DiscordAPIError(
                "Discord機能が無効です(トークン/チャンネルID未設定)".to_string(),
            ));
        }

        let safe_title = truncate_runes(title, 95);
        let body = serde_json::json!({
            "name": safe_title,
            "type": 11,
            "auto_archive_duration": 1440,
        })
        .to_string();
        let url = format!("{}/channels/{}/threads", self.api_base, self.channel_id);

        let (status, resp_body) = self.do_post(&url, body).await?;
        if status != 201 {
            return Err(DiscordAPIError(format!("{status} {resp_body}")));
        }

        serde_json::from_str::<serde_json::Value>(&resp_body)
            .ok()
            .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_string))
            .ok_or_else(|| {
                DiscordAPIError("レスポンス解析エラー: id フィールドがありません".to_string())
            })
    }

    /// スレッドへ1件のレスを送信する(2000字超は自動分割)。
    pub async fn send_message(&self, discord_thread_id: &str, post: &Post) -> Result<(), DiscordAPIError> {
        if !self.enabled() || discord_thread_id.is_empty() {
            return Ok(());
        }

        let header = format!("**{}** : {} : {}", post.num, post.name, post.date);
        let full_content = format!("{header}\n{}", post.message);

        if full_content.chars().count() <= 2000 {
            return self.post_content(discord_thread_id, &full_content).await;
        }

        // webhook.rsで作ったsplit_by_runesをそのまま再利用できた
        // (「n文字ずつに分ける」という操作はwebhook経由でもBot API経由でも同じ)。
        let parts = split_by_runes(&full_content, 1900);
        let total = parts.len();
        for (i, part) in parts.iter().enumerate() {
            let content = if i < total - 1 {
                format!("{part}\n(続く...)")
            } else {
                part.clone()
            };
            self.post_content(discord_thread_id, &content).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(())
    }

    async fn post_content(&self, thread_id: &str, content: &str) -> Result<(), DiscordAPIError> {
        let body = serde_json::json!({ "content": content }).to_string();
        let url = format!("{}/channels/{}/messages", self.api_base, thread_id);

        loop {
            let (status, resp_body) = self.do_post(&url, body.clone()).await?;

            if status == 429 {
                tokio::time::sleep(Duration::from_secs_f64(parse_retry_after(&resp_body))).await;
                continue;
            }
            if status >= 400 {
                return Err(DiscordAPIError(format!("{status} {resp_body}")));
            }
            return Ok(());
        }
    }

    async fn do_post(&self, url: &str, body: String) -> Result<(u16, String), DiscordAPIError> {
        self.fetcher
            .post_json_with_headers(
                url,
                body,
                vec![("Authorization".to_string(), format!("Bot {}", self.token))],
            )
            .await
            .map_err(|e| DiscordAPIError(e.to_string()))
    }
}

fn parse_retry_after(resp_body: &str) -> f64 {
    let retry_after = serde_json::from_str::<serde_json::Value>(resp_body)
        .ok()
        .and_then(|v| v.get("retry_after").and_then(|r| r.as_f64()));
    match retry_after {
        Some(v) if v > 0.0 => v,
        _ => 1.0,
    }
}

/// 文字数(コードポイント数)基準で切り詰める。95文字を超えるスレタイを
/// Discordのスレッド名上限(100文字)に収めるために使う。
pub fn truncate_runes(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    let truncated: String = s.chars().take(n).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_is_false_for_placeholder_token() {
        let m = Manager::new("YOUR_BOT_TOKEN_HERE", "12345");
        assert!(!m.enabled());
    }

    #[test]
    fn enabled_is_false_without_channel_id() {
        let m = Manager::new("real-token-abc", "");
        assert!(!m.enabled());
    }

    #[test]
    fn enabled_is_true_with_real_token_and_channel() {
        let m = Manager::new("real-token-abc", "12345");
        assert!(m.enabled());
    }

    #[test]
    fn truncate_runes_appends_ellipsis_when_over_limit() {
        let long_title = "あ".repeat(100);
        let result = truncate_runes(&long_title, 95);
        assert_eq!(result.chars().count(), 95 + 3); // 95文字 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_runes_leaves_short_title_untouched() {
        assert_eq!(truncate_runes("短いタイトル", 95), "短いタイトル");
    }

    #[test]
    fn parse_retry_after_falls_back_to_one_second() {
        assert_eq!(parse_retry_after("not json"), 1.0);
        assert_eq!(parse_retry_after(r#"{"retry_after": 0}"#), 1.0);
        assert_eq!(parse_retry_after(r#"{"retry_after": 2.5}"#), 2.5);
    }
}
