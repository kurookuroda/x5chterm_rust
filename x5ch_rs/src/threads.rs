//! 板内スレッド一覧取得。Python版 threads.py に対応。
//!
//! Python版の `class HistoryStore(Protocol)` は「このメソッド群を持っていればOK」という
//! 構造的部分型(ダックタイピングの型付き版)。Rustではtraitがこれにあたる。
//! ここで使っている「traitの中に直接 async fn を書く」機能は、実は2023年末リリースの
//! Rust 1.75で安定化されたばかりの新しめの機能 — このサンドボックスのRustもちょうど1.75なので、
//! 追加クレート(async-trait)なしでそのまま使える。

use once_cell::sync::Lazy;
use regex::Regex;

use crate::errors::ThreadsError;
use crate::fetch::Fetcher;
use crate::menu::decode_to_utf8;
use crate::models::{Board, ThreadInfo};

/// Python版の HistoryStore(Protocol) に対応。
pub trait HistoryStore {
    async fn get_last_read(&self, board_url: &str, dat_file: &str) -> u32;
    #[allow(dead_code)]
    async fn exists(&self, board_url: &str, dat_file: &str) -> bool;
    #[allow(dead_code)]
    async fn add_new_thread(&self, title: &str, board_url: &str, dat_file: &str);
}

/// Arc<T>越しでもHistoryStoreを呼べるようにするblanket impl。
/// これのおかげで、tui.rsのように「Browserの中と外の両方から同じ履歴ストアを
/// 参照したい」場面で、複製不可能なManagerをArcで包んで共有できる
/// (Arc<T>は所有権を共有するポインタ。Pythonで同じオブジェクトを複数箇所から
/// 参照するのとほぼ同じ感覚だが、こちらはスレッド安全性込みで型に表れる)。
impl<T: HistoryStore + Send + Sync> HistoryStore for std::sync::Arc<T> {
    async fn get_last_read(&self, board_url: &str, dat_file: &str) -> u32 {
        (**self).get_last_read(board_url, dat_file).await
    }
    async fn exists(&self, board_url: &str, dat_file: &str) -> bool {
        (**self).exists(board_url, dat_file).await
    }
    async fn add_new_thread(&self, title: &str, board_url: &str, dat_file: &str) {
        (**self).add_new_thread(title, board_url, dat_file).await
    }
}

static SUBJECT_LINE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\d+\.dat)<>(.*?)\((\d+)\)\s*$").unwrap());

/// subject.txtの1行から (dat_file, title, count, ikioi) を抜き出す(純粋関数)。
/// menu.rsと同じ理由で、I/Oと切り離してここだけ単体テストできるようにしている。
pub fn parse_subject_lines(data: &str, now: i64) -> Vec<(String, String, u32, f64)> {
    let mut result = Vec::new();

    for line in data.split('\n') {
        let Some(caps) = SUBJECT_LINE_PATTERN.captures(line) else {
            continue;
        };
        let dat_file = caps[1].to_string();
        let title = caps[2].trim().to_string();
        let Ok(count) = caps[3].parse::<u32>() else {
            continue;
        };
        let ikioi = calc_ikioi(&dat_file, count, now);
        result.push((dat_file, title, count, ikioi));
    }

    result
}

pub fn calc_ikioi(dat_file: &str, count: u32, now: i64) -> f64 {
    let ts_str = dat_file.strip_suffix(".dat").unwrap_or(dat_file);
    let Ok(ts) = ts_str.parse::<i64>() else {
        return 0.0;
    };

    let mut elapsed_seconds = now - ts;
    if elapsed_seconds < 1 {
        elapsed_seconds = 1;
    }
    let elapsed_days = elapsed_seconds as f64 / 86400.0;

    count as f64 / elapsed_days
}

/// 板のスレッド一覧(subject.txt)を取得し、勢い順(降順)に並べて返す。
///
/// subject_urlがリダイレクトされた場合、board.url をその場で書き換える。
/// Python版は同じBoardオブジェクトを書き換えることで呼び出し元に伝播させていたが、
/// Rustでは「誰が書き換えてよいか」を型で表明する必要があるので、引数を `&mut Board` にして
/// 「この関数はboardを変更する権利を持つ」ことを呼び出し側にもコンパイラにも明示する。
pub async fn get_threads<H: HistoryStore>(
    fetcher: &Fetcher,
    history: &H,
    board: &mut Board,
) -> Result<Vec<ThreadInfo>, ThreadsError> {
    let subject_url = format!("{}subject.txt", board.url);

    let (body, final_url) = fetcher
        .fetch(&subject_url)
        .await
        .map_err(|e| ThreadsError(format!("スレッド一覧の取得に失敗しました: {e}")))?;

    if final_url != subject_url {
        board.url = final_url
            .strip_suffix("subject.txt")
            .unwrap_or(&final_url)
            .to_string();
    }

    let data = decode_to_utf8(&body);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let mut threads = Vec::new();
    for (dat_file, title, count, ikioi) in parse_subject_lines(&data, now) {
        let last_read = history.get_last_read(&board.url, &dat_file).await;
        threads.push(ThreadInfo {
            dat_file,
            title,
            count,
            ikioi,
            board_url: board.url.clone(),
            last_read,
            url: String::new(),
        });
    }

    threads.sort_by(|a, b| b.ikioi.partial_cmp(&a.ikioi).unwrap_or(std::cmp::Ordering::Equal));
    Ok(threads)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calc_ikioi_computes_posts_per_day() {
        // ちょうど1日前(86400秒前)に立ったスレで、レス数240なら勢いは240/日。
        let now = 1_000_000_000;
        let dat_file = format!("{}.dat", now - 86_400);
        assert!((calc_ikioi(&dat_file, 240, now) - 240.0).abs() < 0.001);
    }

    #[test]
    fn parse_subject_lines_extracts_dat_title_count() {
        let sample = "1700000000.dat<>テストスレ(123)\nおかしい行\n1700000100.dat<>別のスレ(5)\n";
        let result = parse_subject_lines(sample, 1_700_100_000);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "1700000000.dat");
        assert_eq!(result[0].1, "テストスレ");
        assert_eq!(result[0].2, 123);
    }
}
