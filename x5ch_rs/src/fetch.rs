//! HTTP取得層。Python版 fetch.py の Fetcher クラスに対応。
//!
//! Pythonの `class Fetcher: def __init__(self, user_agent): ...` と同じ発想で、
//! 「設定(user_agent)を持つオブジェクト」として構造体+implブロックにまとめる。

use crate::errors::FetchError;

const REQUEST_TIMEOUT_SECS: u64 = 30;
const MAX_REDIRECTS: u32 = 5;

pub struct Fetcher {
    user_agent: String,
}

impl Fetcher {
    pub fn new(user_agent: impl Into<String>) -> Self {
        Self {
            user_agent: user_agent.into(),
        }
    }

    /// URLを取得し、本文(生バイト列)と最終URL(リダイレクト後)を返す。
    pub async fn fetch(&self, url: &str) -> Result<(Vec<u8>, String), FetchError> {
        let user_agent = self.user_agent.clone();
        let url = url.to_string();

        tokio::task::spawn_blocking(move || {
            let tls_connector =
                native_tls::TlsConnector::new().expect("TLSコネクタの初期化に失敗");
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .redirects(MAX_REDIRECTS)
                .user_agent(&user_agent)
                .tls_connector(std::sync::Arc::new(tls_connector))
                .build();

            let resp = agent.get(&url).call().map_err(|e| match e {
                ureq::Error::Transport(t)
                    if t.to_string().contains("too many redirects") =>
                {
                    FetchError::TooManyRedirects(MAX_REDIRECTS as usize)
                }
                ureq::Error::Status(code, _) => FetchError::HttpStatus(code),
                ureq::Error::Transport(t) => {
                    if t.kind() == ureq::ErrorKind::Io {
                        FetchError::Network(t.to_string())
                    } else {
                        FetchError::Timeout(t.to_string())
                    }
                }
            })?;

            let final_url = resp.get_url().to_string();

            let mut body = Vec::new();
            resp.into_reader()
                .read_to_end(&mut body)
                .map_err(|e| FetchError::Network(e.to_string()))?;

            Ok((body, final_url))
        })
        .await
        .expect("blockingタスクがpanicした")
    }

    /// JSON本文をPOSTする。webhook送信(webhook.rs)用。
    /// HTTPエラーステータス(429/4xx/5xx)も例外にはせず、(status, body)としてそのまま返す
    /// — Python版のhttpxが例外を投げず常にResponseオブジェクトを返すのと同じ流儀。
    pub async fn post_json(&self, url: &str, body: String) -> Result<(u16, String), FetchError> {
        self.post_json_with_headers(url, body, Vec::new()).await
    }

    /// post_jsonに追加ヘッダー(Discord BotのAuthorization等)を指定できる版。
    pub async fn post_json_with_headers(
        &self,
        url: &str,
        body: String,
        extra_headers: Vec<(String, String)>,
    ) -> Result<(u16, String), FetchError> {
        let user_agent = self.user_agent.clone();
        let url = url.to_string();

        tokio::task::spawn_blocking(move || {
            let tls_connector =
                native_tls::TlsConnector::new().expect("TLSコネクタの初期化に失敗");
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .user_agent(&user_agent)
                .tls_connector(std::sync::Arc::new(tls_connector))
                .build();

            let mut req = agent.post(&url).set("Content-Type", "application/json");
            for (k, v) in &extra_headers {
                req = req.set(k, v);
            }

            match req.send_string(&body) {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.into_string().unwrap_or_default();
                    Ok((status, text))
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let text = resp.into_string().unwrap_or_default();
                    Ok((code, text))
                }
                Err(ureq::Error::Transport(t)) => Err(FetchError::Network(t.to_string())),
            }
        })
        .await
        .expect("blockingタスクがpanicした")
    }
}
