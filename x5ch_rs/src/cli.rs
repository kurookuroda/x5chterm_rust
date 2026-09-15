//! 非対話CLIコマンド。Python版 cli/main.py に対応。
//!
//! history/queueを一切更新しない使い捨てコマンド(search/read/export/export-batch/
//! webhook-send)。対話TUI(ロック取得・履歴更新・Discord転送)は別モジュールで扱う。
//!
//! ここで一番気持ちいいのは classify_error_type/extract_error_url。
//! search/read/export はそれぞれ SearchError/BrowserError/ThreadsError という
//! 別々の型でエラーを返してくるが、第2章で作った `X5chError` の #[from] のおかげで
//! `X5chError::from(e)` 一発でどれも同じ型に集約でき、エラー処理を1箇所にまとめられる。
//! Python版が `except Exception as ex` で何でも受け止めていたのと同じ効果を、
//! 型を保ったまま実現している。

use std::time::Duration;

use serde_json::json;

use crate::browser::Browser;
use crate::config;
use crate::errors::{X5chError, THREAD_GONE_MESSAGE};
use crate::fetch::Fetcher;
use crate::history::{Manager, NullHistory};
use crate::models::{Post, ThreadInfo};
use crate::webhook;

fn classify_error_type(err: &X5chError) -> &'static str {
    match err {
        X5chError::Fetch(_) => "network",
        X5chError::Browser(be) if be.message == THREAD_GONE_MESSAGE => "thread_gone",
        _ => "other",
    }
}

fn extract_error_url(err: &X5chError) -> Option<String> {
    match err {
        X5chError::Browser(be) => be.url.clone(),
        _ => None,
    }
}

fn print_envelope(env: &serde_json::Value, to_stderr: bool) {
    let text = serde_json::to_string_pretty(env).unwrap();
    if to_stderr {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
}

pub async fn run_search_command(args: &[String]) -> i32 {
    let Some(keyword) = args.first() else {
        print_envelope(
            &json!({"ok": false, "error": "使い方: x5ch search <keyword>", "error_type": "other"}),
            true,
        );
        return 1;
    };

    let cfg = config::load_config();
    let browser = Browser::new(
        cfg.user_agent,
        NullHistory,
        Duration::from_secs_f64(cfg.cache_expiration),
    );

    match browser.search_global(keyword).await {
        Ok(results) => {
            let results_json: Vec<_> = results
                .iter()
                .map(|r| {
                    json!({
                        "title": r.title, "count": r.count,
                        "board_url": r.board_url, "dat_file": r.dat_file, "url": r.url,
                    })
                })
                .collect();
            print_envelope(&json!({"ok": true, "results": results_json}), false);
            0
        }
        Err(e) => {
            let err = X5chError::from(e);
            print_envelope(
                &json!({
                    "ok": false, "error": err.to_string(),
                    "error_type": classify_error_type(&err),
                    "error_url": extract_error_url(&err),
                }),
                true,
            );
            1
        }
    }
}

pub async fn run_read_command(args: &[String]) -> i32 {
    if args.len() < 2 {
        print_envelope(
            &json!({"ok": false, "error": "使い方: x5ch read <board_url> <dat_file>", "error_type": "other"}),
            true,
        );
        return 1;
    }
    let board_url = &args[0];
    let dat_file = &args[1];

    let cfg = config::load_config();
    let browser = Browser::new(
        cfg.user_agent,
        NullHistory,
        Duration::from_secs_f64(cfg.cache_expiration),
    );

    let mut t = ThreadInfo::new(dat_file.clone());
    t.board_url = board_url.clone();

    match browser.get_thread_data(&mut t).await {
        Ok(posts) => {
            let posts_json: Vec<_> = posts
                .iter()
                .map(|p| json!({"num": p.num, "name": p.name, "date": p.date, "message": p.message}))
                .collect();
            print_envelope(
                &json!({
                    "ok": true,
                    "thread": {"title": t.title, "board_url": board_url, "dat_file": dat_file, "count": posts.len()},
                    "posts": posts_json,
                }),
                false,
            );
            0
        }
        Err(e) => {
            let err = X5chError::from(e);
            print_envelope(
                &json!({
                    "ok": false, "error": err.to_string(),
                    "error_type": classify_error_type(&err),
                    "error_url": extract_error_url(&err),
                }),
                true,
            );
            1
        }
    }
}

/// "--since-num N" / "--since-num=N" を取り出す。Python版と同じ手動パース方式
/// (clap等のCLIパーサクレートを増やさず、Python版のwhileループをそのまま翻訳した)。
fn parse_since_num_flag(args: &[String]) -> (u32, Vec<String>) {
    let mut since_num = 0u32;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--since-num" {
            i += 1;
            if let Some(v) = args.get(i) {
                since_num = v.parse().unwrap_or(0);
            }
        } else if let Some(v) = arg.strip_prefix("--since-num=") {
            since_num = v.parse().unwrap_or(0);
        } else {
            rest.push(arg.clone());
        }
        i += 1;
    }
    (since_num, rest)
}

pub async fn run_export_command(args: &[String]) -> i32 {
    let (since_num, rest) = parse_since_num_flag(args);

    if rest.len() < 2 {
        eprintln!("使い方: x5ch export <board_url> <dat_file> [--since-num N]");
        eprintln!("例:     x5ch export https://mao.5ch.io/linux/ 1765829109.dat");
        return 1;
    }
    let board_url = &rest[0];
    let dat_file = &rest[1];

    let cfg = config::load_config();
    let browser = Browser::new(
        cfg.user_agent,
        NullHistory,
        Duration::from_secs_f64(cfg.cache_expiration),
    );

    match browser.export_thread_data(board_url, dat_file, since_num).await {
        Ok(result) => {
            println!("{}", result.to_pretty_json().unwrap());
            0
        }
        Err(be) => {
            if be.message == THREAD_GONE_MESSAGE {
                eprintln!("スレッドはdat落ちしています");
            } else {
                let url_part = be
                    .url
                    .as_deref()
                    .map(|u| format!(" (URL: {u})"))
                    .unwrap_or_default();
                eprintln!("エラー: {be}{url_part}");
            }
            1
        }
    }
}

struct BatchTarget {
    board_url: String,
    dat_file: String,
    since_num: u32,
}

async fn load_targets_from_history(history_file: &str, incremental: bool) -> Vec<BatchTarget> {
    let hist = Manager::new(history_file);
    hist.all_entries()
        .await
        .into_iter()
        .map(|e| BatchTarget {
            board_url: e.board_url,
            dat_file: e.dat_file,
            since_num: if incremental { e.res } else { 0 },
        })
        .collect()
}

fn load_targets_from_queue(queue_file: &str) -> Vec<BatchTarget> {
    let path = std::path::Path::new(queue_file);
    if !path.exists() {
        return Vec::new();
    }

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("キューファイル読み込みエラー: {e}");
            std::process::exit(1);
        }
    };
    let raw: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("キューファイル読み込みエラー: {e}");
            std::process::exit(1);
        }
    };
    let Some(items) = raw.as_array() else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            let board_url = item.get("board_url").and_then(|v| v.as_str())?.to_string();
            let dat_file = item.get("dat_file").and_then(|v| v.as_str())?.to_string();
            let since_num = item.get("since_num").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            Some(BatchTarget { board_url, dat_file, since_num })
        })
        .collect()
}

pub async fn run_export_batch_command(args: &[String]) -> i32 {
    let mut source = "history".to_string();
    let mut incremental = false;
    let mut input_path: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--source" {
            i += 1;
            if let Some(v) = args.get(i) {
                source = v.clone();
            }
        } else if let Some(v) = arg.strip_prefix("--source=") {
            source = v.to_string();
        } else if arg == "--incremental" {
            incremental = true;
        } else if arg == "--input" {
            i += 1;
            if let Some(v) = args.get(i) {
                input_path = Some(v.clone());
            }
        } else if let Some(v) = arg.strip_prefix("--input=") {
            input_path = Some(v.to_string());
        }
        i += 1;
    }

    let cfg = config::load_config();

    let targets = match source.as_str() {
        "history" => {
            load_targets_from_history(input_path.as_deref().unwrap_or(&cfg.history_file), incremental)
                .await
        }
        "queue" => load_targets_from_queue(input_path.as_deref().unwrap_or(&cfg.queue_file)),
        other => {
            eprintln!("不明な--source: {other} (historyまたはqueueを指定)");
            return 1;
        }
    };

    if targets.is_empty() {
        eprintln!("対象のスレッドがありません");
        return 1;
    }

    let browser = Browser::new(
        cfg.user_agent,
        NullHistory,
        Duration::from_secs_f64(cfg.cache_expiration),
    );

    let mut threads = Vec::new();
    let mut errors = Vec::new();
    let total = targets.len();

    for (idx, t) in targets.iter().enumerate() {
        eprintln!("[{}/{total}] 取得中: {} {}", idx + 1, t.board_url, t.dat_file);
        match browser
            .export_thread_data(&t.board_url, &t.dat_file, t.since_num)
            .await
        {
            Ok(result) => threads.push(serde_json::to_value(&result).unwrap()),
            Err(be) => {
                let err = X5chError::from(be);
                errors.push(json!({
                    "board_url": t.board_url, "dat_file": t.dat_file, "since_num": t.since_num,
                    "error": err.to_string(), "error_type": classify_error_type(&err),
                    "error_url": extract_error_url(&err),
                }));
            }
        }
    }

    print_envelope(&json!({"ok": true, "threads": threads, "errors": errors}), false);
    0
}

pub async fn run_webhook_send_command(args: &[String]) -> i32 {
    let mut urls_override = Vec::new();
    let mut positional = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--webhook-url" {
            i += 1;
            if let Some(v) = args.get(i) {
                urls_override.push(v.clone());
            }
        } else if let Some(v) = arg.strip_prefix("--webhook-url=") {
            urls_override.push(v.to_string());
        } else {
            positional.push(arg.clone());
        }
        i += 1;
    }

    let Some(json_path) = positional.first() else {
        eprintln!("使い方: x5ch webhook-send <json_file|-> [--webhook-url URL ...]");
        eprintln!("例:     x5ch export ... | x5ch webhook-send -");
        return 1;
    };

    let raw = if json_path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        buf
    } else {
        match std::fs::read_to_string(json_path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("ファイル読み込みエラー: {e}");
                return 1;
            }
        }
    };

    let data: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("JSON解析エラー: {e}");
            return 1;
        }
    };

    let cfg = config::load_config();
    let urls = if !urls_override.is_empty() {
        urls_override
    } else {
        webhook::load_webhook_urls(&cfg.webhook_urls_file)
    };

    if urls.is_empty() {
        eprintln!("webhook URLが指定/設定されていません");
        eprintln!("--webhook-url で指定するか、{} を用意してください", cfg.webhook_urls_file);
        return 1;
    }

    let thread_entries: Vec<serde_json::Value> =
        if let Some(threads) = data.get("threads").and_then(|v| v.as_array()) {
            threads.clone()
        } else if data.get("posts").is_some() {
            vec![data.clone()]
        } else {
            eprintln!("未対応のJSON形式です(export/export-batchが出力したJSONを指定してください)");
            return 1;
        };

    let fetcher = Fetcher::new(cfg.user_agent.clone());
    let mut total_sent = 0usize;
    let mut all_failures: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for entry in &thread_entries {
        let posts_raw = entry
            .get("posts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if posts_raw.is_empty() {
            continue;
        }

        let thread_title = entry
            .get("thread")
            .and_then(|t| t.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let posts: Vec<Post> = posts_raw
            .iter()
            .map(|p| Post {
                num: p.get("num").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                name: p
                    .get("author_name_display")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("名無し")
                    .to_string(),
                date: p
                    .get("posted_at")
                    .and_then(|v| v.as_str())
                    .or_else(|| p.get("posted_at_raw").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .to_string(),
                message: p
                    .get("body_display")
                    .and_then(|v| v.as_str())
                    .or_else(|| p.get("body_raw").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .to_string(),
            })
            .collect();

        let failures = webhook::broadcast_posts(&fetcher, &urls, &posts, webhook::MESSAGE_INTERVAL).await;
        total_sent += posts.len();
        for (url, failed_nums) in failures {
            if !failed_nums.is_empty() {
                let entry_list = all_failures.entry(url).or_default();
                for n in failed_nums {
                    entry_list.push(format!("{thread_title}#{n}"));
                }
            }
        }
    }

    print_envelope(
        &json!({
            "ok": all_failures.is_empty(),
            "sent_threads": thread_entries.len(),
            "sent_posts": total_sent,
            "failures": all_failures,
        }),
        false,
    );

    i32::from(!all_failures.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_num_flag_supports_both_forms() {
        let args = vec!["--since-num".to_string(), "42".to_string(), "board".to_string(), "dat".to_string()];
        let (n, rest) = parse_since_num_flag(&args);
        assert_eq!(n, 42);
        assert_eq!(rest, vec!["board".to_string(), "dat".to_string()]);

        let args2 = vec!["board".to_string(), "dat".to_string(), "--since-num=7".to_string()];
        let (n2, rest2) = parse_since_num_flag(&args2);
        assert_eq!(n2, 7);
        assert_eq!(rest2, vec!["board".to_string(), "dat".to_string()]);
    }
}
