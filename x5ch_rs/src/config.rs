//! アプリ設定。Python版 config.py に対応(環境変数 + ホームディレクトリのデフォルトパス方式)。

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub discord_bot_token: String,
    pub discord_channel_id: String,
    pub history_file: String,
    pub queue_file: String,
    pub lock_file: String,
    pub pid_file: String,
    pub webhook_urls_file: String,
    pub cache_expiration: f64, // 秒
    pub user_agent: String,
}

/// Python版 _env_or() 相当。環境変数が未設定 or 空文字ならfallbackを使う。
fn env_or(key: &str, fallback: String) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => fallback,
    }
}

pub fn load_config() -> AppConfig {
    // Path.home()相当。dirsクレートはOSごとの「ホームディレクトリ」の流儀の違いを
    // 吸収してくれる(Pythonのpathlib.Path.homeも内部で同種のことをしている)。
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let home_path = |name: &str| home.join(name).to_string_lossy().into_owned();

    AppConfig {
        discord_bot_token: std::env::var("X5CH_DISCORD_BOT_TOKEN").unwrap_or_default(),
        discord_channel_id: std::env::var("X5CH_DISCORD_CHANNEL_ID").unwrap_or_default(),
        history_file: env_or("X5CH_HISTORY_FILE", home_path(".x5ch_history.json")),
        queue_file: env_or("X5CH_QUEUE_FILE", home_path(".x5ch_queue.json")),
        lock_file: env_or("X5CH_LOCK_FILE", home_path(".x5ch.lock")),
        pid_file: env_or("X5CH_PID_FILE", home_path(".x5ch.pid")),
        webhook_urls_file: env_or("X5CH_WEBHOOK_URLS_FILE", home_path(".x5ch_webhooks.json")),
        cache_expiration: 300.0,
        user_agent: "w3m/0.5.3".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_or_falls_back_when_unset() {
        std::env::remove_var("X5CH_TEST_VALUE_UNUSED");
        assert_eq!(
            env_or("X5CH_TEST_VALUE_UNUSED", "fallback".to_string()),
            "fallback"
        );
    }
}
