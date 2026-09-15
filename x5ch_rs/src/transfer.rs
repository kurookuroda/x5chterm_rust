//! Discord転送Worker。Python版 transfer.py に対応。
//!
//! Python版は `asyncio.Condition`(ロック+待機+notify_all)でキューの監視を実装していた。
//! Rustの標準的な対応物は「状態を守る Mutex」+「待っている人を起こす Notify」の組み合わせ。
//! ここでの設計判断を先に書いておく:
//!
//! - キュー変更の通知には `tokio::sync::Notify::notify_one()` を使う。実行ループ(run_loop)は
//!   常にただ1つしか存在しないので、`notify_one()`が持つ「待っている人が居ない時は
//!   "起こす権利"を1個だけ貯めておく(permit)」という性質だけで、通知の取りこぼしを防げる。
//!   (Python版のnotify_all()は「待っている全員を起こす」機能だが、待ち手が複数いる
//!   前提でなければ、Rust版はnotify_one()の方がシンプルかつ安全。)
//! - `wait_until_done()`だけは、正確な条件変数ではなく短い間隔のポーリングで簡略化した。
//!   本来はここも同じNotifyの仕組みで正確に書けるが、複数種類の待ち手が同じNotifyを
//!   共有すると取りこぼしが起きやすくなる(notify_one()は「1人だけ」起こす前提のため)。
//!   ポーリングは行儀が悪く見えるが、意図を明示した上での実用上の割り切り。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::browser::Browser;
use crate::discord;
use crate::errors::THREAD_GONE_MESSAGE;
use crate::history::Manager;
use crate::models::ThreadInfo;
use crate::threads::HistoryStore;

const DEFAULT_NETWORK_RETRY_DELAY_SECS: f64 = 30.0;
const DEFAULT_DISCORD_RETRY_DELAY_SECS: f64 = 10.0;
const DEFAULT_MESSAGE_INTERVAL_SECS: f64 = 1.0;

/// Discordへの転送待ちタスク。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub title: String,
    pub board_url: String,
    pub dat_file: String,
}

/// 失敗の種類。Python版がNetworkFetchError/DiscordAPIError/その他で分岐していたのに対応。
enum WorkerError {
    Network(String),
    Discord(String),
    Other(String),
}

struct WorkerState {
    queue: Vec<Task>,
    working: bool,
    suspended: bool,
    shutdown: bool,
    current_task: Option<Task>,
    current_idx: usize,
    total_msgs: usize,
    last_error: String,
}

/// キューを監視し、順番にスレッドをDiscordへミラーするバックグラウンドワーカー。
pub struct Worker {
    browser: Arc<Browser<Arc<Manager>>>,
    discord: Arc<discord::Manager>,
    history: Arc<Manager>,
    queue_file: PathBuf,
    network_retry_delay: Duration,
    discord_retry_delay: Duration,
    message_interval: Duration,
    state: Mutex<WorkerState>,
    notify: Notify,
    run_handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Worker {
    pub fn new(
        browser: Arc<Browser<Arc<Manager>>>,
        discord: Arc<discord::Manager>,
        history: Arc<Manager>,
        queue_file: impl Into<PathBuf>,
    ) -> Self {
        Self::with_delays(
            browser,
            discord,
            history,
            queue_file,
            Duration::from_secs_f64(DEFAULT_NETWORK_RETRY_DELAY_SECS),
            Duration::from_secs_f64(DEFAULT_DISCORD_RETRY_DELAY_SECS),
            Duration::from_secs_f64(DEFAULT_MESSAGE_INTERVAL_SECS),
        )
    }

    pub fn with_delays(
        browser: Arc<Browser<Arc<Manager>>>,
        discord: Arc<discord::Manager>,
        history: Arc<Manager>,
        queue_file: impl Into<PathBuf>,
        network_retry_delay: Duration,
        discord_retry_delay: Duration,
        message_interval: Duration,
    ) -> Self {
        let queue_file = queue_file.into();
        let queue = load_queue(&queue_file);

        Self {
            browser,
            discord,
            history,
            queue_file,
            network_retry_delay,
            discord_retry_delay,
            message_interval,
            state: Mutex::new(WorkerState {
                queue,
                working: false,
                suspended: false,
                shutdown: false,
                current_task: None,
                current_idx: 0,
                total_msgs: 0,
                last_error: String::new(),
            }),
            notify: Notify::new(),
            run_handle: std::sync::Mutex::new(None),
        }
    }

    /// バックグラウンドループを開始する。`Arc<Worker>`越しに呼ぶ必要がある
    /// — spawnされたタスクが'static寿命で自分自身(Worker)への参照を持ち続けるため、
    /// 所有権を共有できるArcが要る(Pythonの暗黙のself参照キャプチャと違い、Rustでは
    /// 「誰がいつまで生かすか」を型で明示する)。
    pub fn start(self: &Arc<Self>) {
        let mut slot = self.run_handle.lock().unwrap();
        if slot.is_none() {
            let worker = Arc::clone(self);
            *slot = Some(tokio::spawn(async move { worker.run_loop().await }));
        }
    }

    pub async fn enqueue(&self, task: Task) {
        let mut s = self.state.lock().await;
        s.last_error.clear();
        s.queue.push(task);
        self.save_queue_locked(&s.queue);
        drop(s);
        self.notify.notify_one();
    }

    pub async fn delete_at(&self, index: usize) -> Option<Task> {
        let mut s = self.state.lock().await;
        if index >= s.queue.len() {
            return None;
        }
        let removed = s.queue.remove(index);
        self.save_queue_locked(&s.queue);
        Some(removed)
    }

    pub async fn queue_list(&self) -> Vec<Task> {
        self.state.lock().await.queue.clone()
    }

    pub async fn busy(&self) -> bool {
        let s = self.state.lock().await;
        !s.queue.is_empty() || s.working
    }

    pub async fn remaining_threads(&self) -> usize {
        self.state.lock().await.queue.len()
    }

    pub async fn last_error(&self) -> String {
        self.state.lock().await.last_error.clone()
    }

    pub async fn suspend(&self) {
        self.state.lock().await.suspended = true;
    }

    pub async fn resume(&self) {
        let mut s = self.state.lock().await;
        s.suspended = false;
        drop(s);
        self.notify.notify_one();
    }

    /// 現在処理中のタスクをキュー先頭に戻したうえでシャットダウンする。
    pub async fn kill(&self) {
        let mut s = self.state.lock().await;
        s.shutdown = true;
        if s.working {
            if let Some(t) = s.current_task.clone() {
                s.queue.insert(0, t);
            }
        }
        self.save_queue_locked(&s.queue);
        drop(s);
        self.notify.notify_one();
    }

    pub async fn wait_until_stopped(&self) {
        let handle = self.run_handle.lock().unwrap().take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    /// キューが空になり、処理中のタスクもなくなるまで待つ。
    /// (このファイル冒頭のコメントの通り、正確な条件変数ではなくポーリングで簡略化)
    pub async fn wait_until_done(&self) {
        loop {
            let s = self.state.lock().await;
            let done = s.queue.is_empty() && !s.working;
            drop(s);
            if done {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// TUIのステータス行用の短い文字列を返す。
    pub async fn status_string(&self) -> String {
        let s = self.state.lock().await;
        if !s.last_error.is_empty() {
            return format!(" \x1b[41m[{}]\x1b[0m", s.last_error);
        }

        let is_busy = !s.queue.is_empty() || s.working;
        if !is_busy {
            return String::new();
        }

        let title_info = match &s.current_task {
            Some(t) => format!("{}...", take_chars(&t.title, 8)),
            None => "準備中".to_string(),
        };

        let progress = if s.working && s.total_msgs > 0 {
            format!("({}/{})", s.current_idx, s.total_msgs)
        } else {
            String::new()
        };

        let queue_info = if !s.queue.is_empty() {
            format!(" [待機スレ:{}]", s.queue.len())
        } else {
            String::new()
        };

        format!(" \x1b[33m[転送中:{title_info}{progress}{queue_info}]\x1b[0m")
    }

    async fn run_loop(self: Arc<Self>) {
        loop {
            let Some(task) = self.wait_for_work().await else {
                break;
            };

            {
                let mut s = self.state.lock().await;
                s.working = true;
                s.current_idx = 0;
                s.total_msgs = 0;
            }

            match self.process_mirror(&task).await {
                Ok(()) => self.state.lock().await.last_error.clear(),
                Err(e) => self.handle_task_error(&task, e).await,
            }

            {
                let mut s = self.state.lock().await;
                s.current_task = None;
                s.working = false;
            }
            self.notify.notify_one();
        }
    }

    /// キューに仕事が現れる(or shutdownする)まで待つ。冒頭コメントの通り、
    /// notify_one()のpermit機構のおかげで「チェックした直後にenqueueされて
    /// 通知を取りこぼす」という古典的な競合を安全に回避できている。
    async fn wait_for_work(&self) -> Option<Task> {
        loop {
            {
                let mut s = self.state.lock().await;
                if s.shutdown && s.queue.is_empty() {
                    return None;
                }
                if !s.queue.is_empty() && !s.suspended {
                    let task = s.queue.remove(0);
                    s.current_task = Some(task.clone());
                    self.save_queue_locked(&s.queue);
                    return Some(task);
                }
            }
            self.notify.notified().await;
        }
    }

    async fn process_mirror(&self, task: &Task) -> Result<(), WorkerError> {
        let mut t = ThreadInfo::new(task.dat_file.clone());
        t.title = task.title.clone();
        t.board_url = task.board_url.clone();

        let posts = match self.browser.get_thread_data(&mut t).await {
            Ok(posts) => posts,
            Err(be) if be.message == THREAD_GONE_MESSAGE => return Ok(()), // dat落ちは諦める
            Err(be) => return Err(classify_browser_error(&be)),
        };

        if posts.is_empty() {
            return Ok(());
        }

        let mut discord_thread_id = self
            .history
            .get_discord_thread_id(&task.board_url, &task.dat_file)
            .await;

        if discord_thread_id.is_none() {
            let id = self
                .discord
                .create_thread(&task.title)
                .await
                .map_err(|e| WorkerError::Discord(e.to_string()))?;
            self.history.update_history(&t, 0, Some(id.clone())).await;
            discord_thread_id = Some(id);
        }
        let discord_thread_id = discord_thread_id.expect("直前に設定済み");

        let last_read = self
            .history
            .get_last_read(&task.board_url, &task.dat_file)
            .await;
        let new_posts: Vec<_> = posts.into_iter().filter(|p| p.num > last_read).collect();
        if new_posts.is_empty() {
            return Ok(());
        }

        {
            let mut s = self.state.lock().await;
            s.total_msgs = new_posts.len();
        }

        for (idx, post) in new_posts.iter().enumerate() {
            {
                let mut s = self.state.lock().await;
                if s.shutdown {
                    return Ok(());
                }
                s.current_idx = idx + 1;
            }

            self.discord
                .send_message(&discord_thread_id, post)
                .await
                .map_err(|e| WorkerError::Discord(e.to_string()))?;
            self.history
                .update_history(&t, post.num, Some(discord_thread_id.clone()))
                .await;
            tokio::time::sleep(self.message_interval).await;
        }

        Ok(())
    }

    async fn handle_task_error(&self, task: &Task, err: WorkerError) {
        match err {
            WorkerError::Network(_) => {
                self.set_last_error("ネットワークエラー、後で再試行します").await;
                tokio::time::sleep(self.network_retry_delay).await;
                self.requeue(task.clone()).await;
            }
            WorkerError::Discord(msg) => {
                self.set_last_error(&discord::truncate_runes(&msg, 20)).await;
                tokio::time::sleep(self.discord_retry_delay).await;
                self.requeue(task.clone()).await;
            }
            WorkerError::Other(msg) => {
                self.set_last_error(&discord::truncate_runes(&msg, 20)).await;
            }
        }
    }

    async fn set_last_error(&self, msg: &str) {
        self.state.lock().await.last_error = msg.to_string();
    }

    async fn requeue(&self, task: Task) {
        let mut s = self.state.lock().await;
        s.queue.push(task);
        self.save_queue_locked(&s.queue);
        drop(s);
        self.notify.notify_one();
    }

    fn save_queue_locked(&self, queue: &[Task]) {
        if let Ok(body) = serde_json::to_string_pretty(queue) {
            let _ = std::fs::write(&self.queue_file, body);
        }
    }
}

fn load_queue(path: &Path) -> Vec<Task> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// browser::get_thread_data()が返すBrowserErrorから、ネットワーク由来の失敗かどうかを
/// 判定する。Python版はNetworkFetchErrorという専用の例外型で判定していたが、
/// Rust版のBrowserErrorはメッセージに理由を埋め込む設計(第8章)にしたため、
/// ここではメッセージのプレフィックスで判定する簡略化をしている
/// (型情報を完全に保つには、BrowserErrorにネットワーク由来かどうかのフラグを
/// 持たせる設計に変更する必要がある)。
fn classify_browser_error(be: &crate::errors::BrowserError) -> WorkerError {
    if be.message.starts_with("スレッド取得に失敗しました") {
        WorkerError::Network(be.message.clone())
    } else {
        WorkerError::Other(be.message.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("x5ch_rs_transfer_test_{name}_{}.json", std::process::id()))
    }

    fn make_worker(queue_file: &Path) -> Worker {
        let history_path = temp_path("history_for_worker");
        let _ = std::fs::remove_file(&history_path);
        let history = Arc::new(Manager::new(&history_path));
        let browser = Arc::new(Browser::new(
            "x5ch_rs/0.1 (test)",
            history.clone(),
            Duration::from_secs(60),
        ));
        let discord = Arc::new(discord::Manager::new("", "")); // 空トークン=disabled
        Worker::new(browser, discord, history, queue_file)
    }

    fn sample_task(n: u32) -> Task {
        Task {
            title: format!("テストスレ{n}"),
            board_url: "https://egg.5ch.io/livejupiter/".to_string(),
            dat_file: format!("{n}.dat"),
        }
    }

    #[tokio::test]
    async fn enqueue_and_delete_updates_queue() {
        let qpath = temp_path("queue1");
        let _ = std::fs::remove_file(&qpath);
        let worker = make_worker(&qpath);

        worker.enqueue(sample_task(1)).await;
        worker.enqueue(sample_task(2)).await;
        assert_eq!(worker.remaining_threads().await, 2);
        assert!(worker.busy().await);

        let removed = worker.delete_at(0).await;
        assert_eq!(removed.unwrap().dat_file, "1.dat");
        assert_eq!(worker.remaining_threads().await, 1);

        let _ = std::fs::remove_file(&qpath);
    }

    #[tokio::test]
    async fn enqueue_persists_to_queue_file() {
        let qpath = temp_path("queue2");
        let _ = std::fs::remove_file(&qpath);
        let worker = make_worker(&qpath);

        worker.enqueue(sample_task(42)).await;

        let saved = load_queue(&qpath);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].dat_file, "42.dat");

        let _ = std::fs::remove_file(&qpath);
    }

    #[tokio::test]
    async fn status_string_is_empty_when_idle() {
        let qpath = temp_path("queue3");
        let _ = std::fs::remove_file(&qpath);
        let worker = make_worker(&qpath);
        assert_eq!(worker.status_string().await, "");
        let _ = std::fs::remove_file(&qpath);
    }

    #[tokio::test]
    async fn status_string_shows_error_when_set() {
        let qpath = temp_path("queue4");
        let _ = std::fs::remove_file(&qpath);
        let worker = make_worker(&qpath);
        worker.set_last_error("テストエラー").await;
        assert_eq!(worker.status_string().await, " \u{1b}[41m[テストエラー]\u{1b}[0m");
        let _ = std::fs::remove_file(&qpath);
    }

    #[test]
    fn classify_browser_error_detects_network_prefix() {
        let network_err = crate::errors::BrowserError::new(
            "スレッド取得に失敗しました: HTTP Error: 500",
            None,
        );
        assert!(matches!(classify_browser_error(&network_err), WorkerError::Network(_)));

        let other_err = crate::errors::BrowserError::new("board_urlの解析に失敗: foo", None);
        assert!(matches!(classify_browser_error(&other_err), WorkerError::Other(_)));
    }
}
