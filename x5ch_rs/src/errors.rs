//! 例外階層。Python版 errors.py に対応。
//!
//! Pythonは `class FetchError(X5chError)` のようにクラス継承で階層を作り、
//! 呼び出し側は `except X5chError` で全部まとめて捕まえられる。
//! Rustには継承がないので、代わりに「全パターンを1つのenumに集約する」設計にする。
//! 各下位エラー型(FetchErrorなど)は #[from] を付けておくと、`?` 演算子が
//! 自動的に X5chError に変換してくれる — ちょうどPythonの例外伝播に近い書き心地になる。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("リダイレクト回数が上限({0})に達しました")]
    TooManyRedirects(usize),

    #[error("通信タイムアウト: {0}")]
    Timeout(String),

    #[error("通信エラー: {0}")]
    Network(String),

    #[error("HTTP Error: {0}")]
    HttpStatus(u16),
}

/// 板メニュー取得の失敗(menu.cr MenuError)。
#[derive(Debug, Error)]
#[error("板メニュー取得エラー: {0}")]
pub struct MenuError(pub String);

/// スレッド一覧取得の失敗(threads.cr ThreadsError)。
#[derive(Debug, Error)]
#[error("スレ一覧取得エラー: {0}")]
pub struct ThreadsError(pub String);

/// 全板検索の失敗(search.cr SearchError)。
#[derive(Debug, Error)]
#[error("全板検索エラー: {0}")]
pub struct SearchError(pub String);

pub const THREAD_GONE_MESSAGE: &str = "スレッドはdat落ちしています";

/// Browser層での失敗。url には実際に失敗したURLを可能な限り持たせる。
/// Python版の `url: str | None = None` は Rust では `Option<String>` に対応する。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct BrowserError {
    pub message: String,
    pub url: Option<String>,
}

impl BrowserError {
    pub fn new(message: impl Into<String>, url: Option<String>) -> Self {
        Self {
            message: message.into(),
            url,
        }
    }

    /// Python版 ThreadGoneError() に対応するショートカット。
    pub fn thread_gone() -> Self {
        Self::new(THREAD_GONE_MESSAGE, None)
    }
}

/// Discord Bot API連携の失敗(discord.py DiscordAPIError)。
/// Python版でもX5chErrorの継承ツリーには入れず独立した例外だったので、
/// Rust版も同様にX5chErrorへは合流させていない。
#[derive(Debug, Error)]
#[error("{0}")]
pub struct DiscordAPIError(pub String);

/// このプロジェクトの全エラーをまとめる最上位型。Python版 X5chError に対応。
///
/// #[from] のおかげで、各層の関数内で `some_fetch_call()?` と書くだけで
/// FetchError が自動的に X5chError::Fetch(...) に変換されて上に伝播する。
#[derive(Debug, Error)]
pub enum X5chError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Menu(#[from] MenuError),
    #[error(transparent)]
    Threads(#[from] ThreadsError),
    #[error(transparent)]
    Search(#[from] SearchError),
    #[error(transparent)]
    Browser(#[from] BrowserError),
}
