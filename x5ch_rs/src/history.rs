//! 閲覧履歴の永続化。Python版 history.py の Manager クラスに対応。
//!
//! Python版はファイルI/Oを「同期」で行いつつ、asyncio.Lockでタスク間の競合だけ防いでいた
//! (I/O自体は非同期化されていない)。ここでも同じ発想で、tokio::sync::Mutexで
//! HashMapを守りつつ、中身の読み書きは std::fs の同期APIをそのまま使う。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::models::ThreadInfo;
use crate::threads::HistoryStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    res: u32,
    title: String,
    board_url: String,
    dat_file: String,
    timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    discord_thread_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RecentThread {
    pub thread_info: ThreadInfo,
    pub timestamp: i64,
}

/// 閲覧履歴をJSONファイルに永続化するマネージャ。
pub struct Manager {
    file_path: PathBuf,
    data: Mutex<HashMap<String, Entry>>,
}

impl Manager {
    /// Python版と同じく、コンストラクタの時点で同期的にファイルをロードする。
    /// 読み込み・パース失敗時は空の履歴として扱う(Python版の except節と同じ寛容さ)。
    pub fn new(file_path: impl Into<PathBuf>) -> Self {
        let file_path = file_path.into();
        let data = Self::load(&file_path);
        Self {
            file_path,
            data: Mutex::new(data),
        }
    }

    fn load(path: &Path) -> HashMap<String, Entry> {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return HashMap::new();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    fn save(&self, data: &HashMap<String, Entry>) -> bool {
        let Ok(body) = serde_json::to_string_pretty(data) else {
            return false;
        };
        std::fs::write(&self.file_path, body).is_ok()
    }

    /// Python版はre.subを3連発していたが、いずれも「先頭/末尾の固定文字列を消す」だけなので、
    /// 正規表現なしでstrip_prefix/strip_suffixで書ける(実行コストも下がる)。
    fn normalize_url(raw_url: &str) -> String {
        let mut u = raw_url;
        if let Some(rest) = u.strip_prefix("https://") {
            u = rest;
        } else if let Some(rest) = u.strip_prefix("http://") {
            u = rest;
        }
        if let Some(rest) = u.strip_prefix("www.") {
            u = rest;
        }
        let u = u.strip_suffix('/').unwrap_or(u);
        u.replace("2ch.net", "5ch.io").replace("5ch.net", "5ch.io")
    }

    fn generate_key(board_url: &str, dat_file: &str) -> String {
        format!("{}::{}", Self::normalize_url(board_url), dat_file)
    }

    pub async fn get_discord_thread_id(&self, board_url: &str, dat_file: &str) -> Option<String> {
        let data = self.data.lock().await;
        data.get(&Self::generate_key(board_url, dat_file))
            .and_then(|e| e.discord_thread_id.clone())
    }

    pub async fn has_history_in_board(&self, board_url: &str) -> bool {
        let target = Self::normalize_url(board_url);
        let data = self.data.lock().await;
        data.values().any(|e| Self::normalize_url(&e.board_url) == target)
    }

    pub async fn has_history_in_category(&self, boards: &[crate::models::Board]) -> bool {
        for b in boards {
            if self.has_history_in_board(&b.url).await {
                return true;
            }
        }
        false
    }

    pub async fn delete_thread(&self, board_url: &str, dat_file: &str) -> bool {
        let mut data = self.data.lock().await;
        let key = Self::generate_key(board_url, dat_file);
        if data.remove(&key).is_none() {
            return false;
        }
        self.save(&data)
    }

    pub async fn update_history(
        &self,
        t: &ThreadInfo,
        res_num: u32,
        discord_thread_id: Option<String>,
    ) {
        let mut data = self.data.lock().await;
        let key = Self::generate_key(&t.board_url, &t.dat_file);
        let current = data.get(&key);

        let new_res = current.map_or(res_num, |c| res_num.max(c.res));
        let new_discord_id = discord_thread_id.or_else(|| current.and_then(|c| c.discord_thread_id.clone()));

        data.insert(
            key,
            Entry {
                res: new_res,
                title: t.title.clone(),
                board_url: t.board_url.clone(),
                dat_file: t.dat_file.clone(),
                timestamp: now_unix(),
                discord_thread_id: new_discord_id,
            },
        );
        self.save(&data);
    }

    /// 履歴を新しい順(タイムスタンプ降順)に返す。
    pub async fn get_recent_threads(&self) -> Vec<RecentThread> {
        let data = self.data.lock().await;
        let mut threads: Vec<RecentThread> = data
            .values()
            .map(|e| RecentThread {
                thread_info: ThreadInfo {
                    dat_file: e.dat_file.clone(),
                    title: e.title.clone(),
                    count: 0,
                    ikioi: 0.0,
                    board_url: e.board_url.clone(),
                    last_read: e.res,
                    url: String::new(),
                },
                timestamp: e.timestamp,
            })
            .collect();
        threads.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        threads
    }

    /// export-batchコマンド用に、履歴の全エントリをboard_url/dat_file/resだけの
    /// 軽量なビューで返す。
    pub async fn all_entries(&self) -> Vec<HistoryEntryView> {
        let data = self.data.lock().await;
        data.values()
            .map(|e| HistoryEntryView {
                board_url: e.board_url.clone(),
                dat_file: e.dat_file.clone(),
                res: e.res,
            })
            .collect()
    }
}

/// Manager::all_entries()が返す軽量ビュー。
#[derive(Debug, Clone)]
pub struct HistoryEntryView {
    pub board_url: String,
    pub dat_file: String,
    pub res: u32,
}

/// HistoryStoreトレイトの実装。threads.rs/nextthread.rsから見て、
/// Managerは「履歴を読み書きできる何か」として扱える。
impl HistoryStore for Manager {
    async fn get_last_read(&self, board_url: &str, dat_file: &str) -> u32 {
        let data = self.data.lock().await;
        data.get(&Self::generate_key(board_url, dat_file))
            .map(|e| e.res)
            .unwrap_or(0)
    }

    async fn exists(&self, board_url: &str, dat_file: &str) -> bool {
        let data = self.data.lock().await;
        data.contains_key(&Self::generate_key(board_url, dat_file))
    }

    async fn add_new_thread(&self, title: &str, board_url: &str, dat_file: &str) {
        let mut data = self.data.lock().await;
        let key = Self::generate_key(board_url, dat_file);
        if data.contains_key(&key) {
            return;
        }
        data.insert(
            key,
            Entry {
                res: 0,
                title: title.to_string(),
                board_url: board_url.to_string(),
                dat_file: dat_file.to_string(),
                timestamp: now_unix(),
                discord_thread_id: None,
            },
        );
        self.save(&data);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// 永続化を一切行わないダミーのHistoryStore実装。search/read/exportのような
/// 使い捨てCLIコマンドで使う。
pub struct NullHistory;

impl HistoryStore for NullHistory {
    async fn get_last_read(&self, _board_url: &str, _dat_file: &str) -> u32 {
        0
    }
    async fn exists(&self, _board_url: &str, _dat_file: &str) -> bool {
        false
    }
    async fn add_new_thread(&self, _title: &str, _board_url: &str, _dat_file: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("x5ch_rs_test_{name}_{}.json", std::process::id()))
    }

    #[tokio::test]
    async fn add_and_read_back_persists_across_new_manager() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        {
            let manager = Manager::new(&path);
            manager.add_new_thread("テストスレ", "https://egg.5ch.net/livejupiter/", "123.dat").await;
        }

        // 別インスタンスで読み直しても、ファイル経由で内容が引き継がれることを確認。
        let manager2 = Manager::new(&path);
        assert!(manager2.exists("https://egg.5ch.io/livejupiter/", "123.dat").await);
        assert_eq!(manager2.get_last_read("https://egg.5ch.io/livejupiter/", "123.dat").await, 0);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn update_history_keeps_max_res() {
        let path = temp_path("maxres");
        let _ = std::fs::remove_file(&path);
        let manager = Manager::new(&path);

        let t = ThreadInfo::new("999.dat");
        manager.update_history(&t, 10, None).await;
        manager.update_history(&t, 5, None).await; // 5 < 10 なので巻き戻らないはず

        assert_eq!(manager.get_last_read(&t.board_url, &t.dat_file).await, 10);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn normalize_url_strips_scheme_www_slash_and_upgrades_domain() {
        assert_eq!(
            Manager::normalize_url("https://www.egg.5ch.net/livejupiter/"),
            "egg.5ch.io/livejupiter"
        );
    }
}
