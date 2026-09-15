//! Browser調整役。Python版 browser.py の Browser クラスに対応。
//!
//! これまでの章で作った各モジュール(menu/threads/search/nextthread/parse/export)を
//! 呼び出しつつ、メニュー・スレ一覧のキャッシュを持つ「まとめ役」。
//! Rust版では `Browser<H: HistoryStore>` のようにHistoryStoreの実装をジェネリクスで
//! 受け取ることで、本番はhistory::Manager、使い捨てコマンドはhistory::NullHistoryと
//! 差し替え可能にしている(第6章で作ったトレイトがここで実際に効いてくる)。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::errors::{BrowserError, MenuError, SearchError, ThreadsError};
use crate::export::{self, ExportResult, ExportSource, ExportThread};
use crate::fetch::Fetcher;
use crate::menu;
use crate::models::{Board, Category, Post, ThreadInfo};
use crate::nextthread;
use crate::parse;
use crate::search;
use crate::threads::{self, HistoryStore};

struct CachedThreads {
    data: Vec<ThreadInfo>,
    time: Instant,
}

pub struct Browser<H: HistoryStore> {
    history: H,
    cache_expire: Duration,
    fetcher: Fetcher,
    menu_cache: Mutex<Option<Vec<Category>>>,
    thread_cache: Mutex<HashMap<String, CachedThreads>>,
}

impl<H: HistoryStore> Browser<H> {
    pub fn new(user_agent: impl Into<String>, history: H, cache_expire: Duration) -> Self {
        Self {
            history,
            cache_expire,
            fetcher: Fetcher::new(user_agent),
            menu_cache: Mutex::new(None),
            thread_cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_menu(&self) -> Result<Vec<Category>, MenuError> {
        let mut cache = self.menu_cache.lock().await;
        if let Some(cats) = cache.as_ref() {
            return Ok(cats.clone());
        }
        let cats = menu::get_menu(&self.fetcher).await?;
        *cache = Some(cats.clone());
        Ok(cats)
    }

    pub async fn invalidate_menu_cache(&self) {
        *self.menu_cache.lock().await = None;
    }

    /// force_reload=falseの場合、cache_expire以内ならキャッシュを返す。
    /// 取得に失敗した場合、期限切れであっても古いキャッシュがあればそれにフォールバックする。
    pub async fn get_threads(
        &self,
        board: &mut Board,
        force_reload: bool,
    ) -> Result<Vec<ThreadInfo>, ThreadsError> {
        if !force_reload {
            let cache = self.thread_cache.lock().await;
            if let Some(cached) = cache.get(&board.url) {
                if cached.time.elapsed() < self.cache_expire {
                    return Ok(cached.data.clone());
                }
            }
        }

        let stale = {
            let cache = self.thread_cache.lock().await;
            cache.get(&board.url).map(|c| c.data.clone())
        };

        match threads::get_threads(&self.fetcher, &self.history, board).await {
            Ok(threads) => {
                self.thread_cache.lock().await.insert(
                    board.url.clone(),
                    CachedThreads {
                        data: threads.clone(),
                        time: Instant::now(),
                    },
                );
                Ok(threads)
            }
            Err(e) => stale.ok_or(e),
        }
    }

    pub async fn search_global(&self, keyword: &str) -> Result<Vec<ThreadInfo>, SearchError> {
        search::search_global(&self.fetcher, &self.history, keyword).await
    }

    /// 指定スレッドをアーカイブ用の完全なJSON構造で取得する。
    /// since_num > 0 の場合、その番号以下のレスは posts から除外する
    /// (thread.post_count は除外前の総レス数のまま)。
    pub async fn export_thread_data(
        &self,
        board_url: &str,
        dat_file: &str,
        since_num: u32,
    ) -> Result<ExportResult, BrowserError> {
        let read_url = build_read_url(board_url, dat_file)?;

        let (body, final_url) = self.fetcher.fetch(&read_url).await.map_err(|e| {
            BrowserError::new(
                format!("スレッド取得に失敗しました: {e}"),
                Some(read_url.clone()),
            )
        })?;

        let html_content = menu::decode_to_utf8(&body);
        if html_content.contains("dat落ち") {
            return Err(BrowserError::thread_gone());
        }

        let uri = url::Url::parse(board_url).map_err(|_| {
            BrowserError::new(
                format!("board_urlの解析に失敗: {board_url}"),
                Some(board_url.to_string()),
            )
        })?;

        let dat_num = dat_file.strip_suffix(".dat").unwrap_or(dat_file);
        let thread_external_id =
            format!("5ch:{}{}{}", uri.host_str().unwrap_or(""), uri.path(), dat_num);

        let all_posts = export::parse_posts_for_export(&html_content, &thread_external_id);

        // 900番タイトル検出は第5章で作ったnextthread::extract_titleをそのまま再利用できる。
        // Python版はここでnextthread_mod.TITLE_TAG_PATTERN等を直接呼んでいたが、
        // Rust版は「タイトル抽出」という操作自体を1つの公開関数にまとめてあるおかげで
        // ロジックの重複がない。
        let title = nextthread::extract_title(&html_content);

        let posts = if since_num > 0 {
            all_posts
                .iter()
                .filter(|p| p.num > since_num)
                .cloned()
                .collect()
        } else {
            all_posts.clone()
        };

        let board_name = export::extract_board_name(&html_content);
        let created_at = export::dat_timestamp_to_rfc3339(dat_file);

        Ok(ExportResult {
            source: ExportSource {
                provider: "5ch".to_string(),
                board_url: board_url.to_string(),
                dat_file: dat_file.to_string(),
                thread_url: final_url,
                scraped_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            },
            thread: ExportThread {
                external_id: thread_external_id,
                title,
                board_name: (!board_name.is_empty()).then_some(board_name),
                created_at: (!created_at.is_empty()).then_some(created_at),
                post_count: all_posts.len(),
            },
            posts,
        })
    }

    /// 指定スレッドの全レスを取得する。取得後、900番以降のレスから次スレを検出して履歴に追加する。
    pub async fn get_thread_data(&self, t: &mut ThreadInfo) -> Result<Vec<Post>, BrowserError> {
        let read_url = build_read_url(&t.board_url, &t.dat_file)?;

        let (body, final_url) = self.fetcher.fetch(&read_url).await.map_err(|e| {
            BrowserError::new(
                format!("スレッド取得に失敗しました: {e}"),
                Some(read_url.clone()),
            )
        })?;

        let html_content = menu::decode_to_utf8(&body);
        if html_content.contains("dat落ち") {
            return Err(BrowserError::thread_gone());
        }

        let posts = parse::parse_posts(&html_content);
        t.url = final_url;

        nextthread::detect_and_add_next_thread(&self.fetcher, &self.history, &posts, t).await;

        Ok(posts)
    }
}

/// board_url + dat_file から read.cgi の完全URLを組み立てる。
pub fn build_read_url(board_url: &str, dat_file: &str) -> Result<String, BrowserError> {
    let uri = url::Url::parse(board_url).map_err(|_| {
        BrowserError::new(
            format!("board_urlの解析に失敗: {board_url}"),
            Some(board_url.to_string()),
        )
    })?;

    let segments: Vec<&str> = uri
        .path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let Some(board_name) = segments.last() else {
        return Err(BrowserError::new(
            format!("board_urlから板名を特定できません: {board_url}"),
            Some(board_url.to_string()),
        ));
    };

    let dat_num = dat_file.strip_suffix(".dat").unwrap_or(dat_file);
    if dat_num.is_empty() || !dat_num.chars().all(|c| c.is_ascii_digit()) {
        return Err(BrowserError::new(
            format!("不正なdat_file: {dat_file}"),
            Some(board_url.to_string()),
        ));
    }

    let scheme = uri.scheme();
    let host = uri.host_str().unwrap_or("");
    let port = uri.port().map(|p| format!(":{p}")).unwrap_or_default();

    Ok(format!(
        "{scheme}://{host}{port}/test/read.cgi/{board_name}/{dat_num}/"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_read_url_assembles_read_cgi_path() {
        let url = build_read_url("https://egg.5ch.io/livejupiter/", "1700000000.dat").unwrap();
        assert_eq!(
            url,
            "https://egg.5ch.io/test/read.cgi/livejupiter/1700000000/"
        );
    }

    #[test]
    fn build_read_url_rejects_non_numeric_dat_file() {
        let err = build_read_url("https://egg.5ch.io/livejupiter/", "abc.dat").unwrap_err();
        assert!(err.message.contains("不正なdat_file"));
    }

    #[test]
    fn build_read_url_rejects_board_url_without_board_name() {
        let err = build_read_url("https://egg.5ch.io/", "1700000000.dat").unwrap_err();
        assert!(err.message.contains("板名を特定できません"));
    }
}
