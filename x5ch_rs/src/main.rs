mod browser;
mod cli;
mod config;
mod discord;
mod errors;
mod export;
mod fetch;
mod history;
mod lock;
mod menu;
mod models;
mod nextthread;
mod parse;
mod search;
mod threads;
mod transfer;
mod tui;
mod webhook;

use errors::X5chError;
use fetch::Fetcher;
use models::{Board, Category, ThreadInfo};
use threads::HistoryStore;

async fn try_fetch(fetcher: &Fetcher, label: &str, url: &str) {
    match fetcher.fetch(url).await {
        Ok((body, final_url)) => {
            println!("[{label}] 取得成功: {final_url} ({} bytes)", body.len());
        }
        Err(e) => {
            eprintln!("[{label}] 取得失敗: {e}");
        }
    }
}

/// エントリポイント。Python版main()の
/// `handlers = {"search": ..., "read": ..., ...}` によるディスパッチに対応。
/// 引数なしの場合、Python版は対話TUIを起動するが、TUIはまだ移植していないので
/// 代わりにこれまでの全章のデモをまとめて実行する(`demo`サブコマンドでも呼べる)。
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        run_demo().await;
        return;
    }

    let (command, rest) = (args[0].as_str(), &args[1..]);
    let exit_code = match command {
        "search" => cli::run_search_command(rest).await,
        "read" => cli::run_read_command(rest).await,
        "export" => cli::run_export_command(rest).await,
        "export-batch" => cli::run_export_batch_command(rest).await,
        "webhook-send" => cli::run_webhook_send_command(rest).await,
        "tui" => {
            let cfg = config::load_config();
            let history = std::sync::Arc::new(history::Manager::new(&cfg.history_file));
            let browser = std::sync::Arc::new(browser::Browser::new(
                cfg.user_agent,
                history.clone(),
                std::time::Duration::from_secs_f64(cfg.cache_expiration),
            ));
            let discord_mgr = std::sync::Arc::new(discord::Manager::new(
                cfg.discord_bot_token.clone(),
                cfg.discord_channel_id.clone(),
            ));
            let worker = std::sync::Arc::new(transfer::Worker::new(
                browser.clone(),
                discord_mgr.clone(),
                history.clone(),
                &cfg.queue_file,
            ));

            match tui::run(browser, history, discord_mgr, worker).await {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("TUIエラー: {e}");
                    1
                }
            }
        }
        "demo" => {
            run_demo().await;
            0
        }
        other => {
            eprintln!("不明なコマンド: {other}");
            1
        }
    };

    std::process::exit(exit_code);
}

/// これまでの全章の動作確認をまとめて実行する(第1〜9章相当)。
async fn run_demo() {
    let fetcher = Fetcher::new("x5ch_rs/0.1 (learning-rust)");

    // 1. HTTPSでの動作確認(rustls経由)
    try_fetch(
        &fetcher,
        "https",
        "https://raw.githubusercontent.com/kurookuroda/5chterm_py/main/x5ch_py/README.md",
    )
    .await;

    // 2. 従来通りHTTPでも動くことの確認
    try_fetch(
        &fetcher,
        "http",
        "http://archive.ubuntu.com/ubuntu/dists/noble/Release",
    )
    .await;

    // 3. わざと存在しないパスで404を起こし、エラー分岐を確認
    try_fetch(
        &fetcher,
        "404",
        "https://raw.githubusercontent.com/kurookuroda/5chterm_py/main/does-not-exist.md",
    )
    .await;

    demo_models();
    demo_parse();
    demo_menu();
    demo_threads().await;
    demo_search();
    demo_nextthread();
    demo_history().await;
    demo_export();
    demo_browser().await;
    demo_config();
    demo_lock();
    demo_discord().await;
    demo_transfer().await;

    if let Err(e) = demo_error_propagation(&fetcher).await {
        eprintln!("[error伝播デモ] 予想通りX5chErrorとして受け取れた: {e}");
    }
}

/// models.rsの動作確認。Category(カテゴリ)の下にBoard(板)を積んで表示するだけ。
fn demo_models() {
    let mut category = Category::new("ニュース速報");
    category.boards.push(Board {
        title: "なんJ".to_string(),
        url: "https://example.5ch.net/livejupiter/".to_string(),
    });

    println!("\n[models] {category:?}");

    let mut thread = ThreadInfo::new("1234567890.dat");
    thread.count = 42;
    thread.last_read = 10;
    println!(
        "[models] スレ「{}」has_new={}",
        thread.dat_file,
        thread.has_new()
    );
}

/// parse.rsの動作確認。実際の5chにはアクセスできないので、想定される
/// DOM構造を模した合成HTMLでテストする(実運用時のHTMLを取得して確認してください)。
fn demo_parse() {
    let sample_html = r##"
    <div class="clear post">
      <span class="postid">1</span>
      <span class="postusername"><b>名無しさん</b></span>
      <span class="date">2026/09/14(月) 10:00:00</span>
      <span class="uid">ID:abc123</span>
      <div class="post-content">初めてRustでHTMLパースしてみた!<br>ttp://example.com/ を貼ってみる</div>
    </div>
    <div class="clear post">
      <span class="postid">2</span>
      <span class="postusername">名無しさん</span>
      <span class="date">2026/09/14(月) 10:05:00</span>
      <span class="uid">ID:xyz789</span>
      <div class="post-content">&gt;&gt;1 &nbsp;乙</div>
    </div>
    "##;

    let posts = parse::parse_posts(sample_html);
    println!("\n[parse] {}件のレスを抽出", posts.len());
    for post in &posts {
        println!("  {post:?}");
    }
}

/// menu.rsの動作確認。実際のmenu.5ch.ioには届かないので、bbsmenu.json相当の
/// 合成JSON/HTMLを直接パースする(parse_menu_json/parse_menu_htmlは純粋関数なので可能)。
fn demo_menu() {
    let sample_json = r#"{
        "menu_list": [
            {
                "category_name": "ニュース速報",
                "category_content": [
                    {"board_name": "なんJ", "url": "http://egg.5ch.net/livejupiter/"}
                ]
            }
        ]
    }"#;
    let categories = menu::parse_menu_json(sample_json).expect("JSON解析に失敗");
    println!("\n[menu] JSON経由: {categories:?}");

    let sample_html = r#"
        <B>ニュース速報</B>
        <A HREF="http://egg.5ch.net/livejupiter/">なんJ</A>
    "#;
    let categories = menu::parse_menu_html(sample_html);
    println!("[menu] HTML経由: {categories:?}");
}

/// threads.rsの動作確認。subject.txt相当の合成テキストをparse_subject_linesに通し、
/// ThreadInfo::has_new()まで一通り動かす。
async fn demo_threads() {
    let now = 1_700_100_000;
    let sample_subject = "1700000000.dat<>雑談スレ(123)\n1700000100.dat<>質問スレ(5)\n";

    let history_store = history::NullHistory;
    let mut threads = Vec::new();
    for (dat_file, title, count, ikioi) in threads::parse_subject_lines(sample_subject, now) {
        let last_read = history_store
            .get_last_read("https://egg.5ch.io/livejupiter/", &dat_file)
            .await;
        threads.push(ThreadInfo {
            dat_file,
            title,
            count,
            ikioi,
            board_url: "https://egg.5ch.io/livejupiter/".to_string(),
            last_read,
            url: String::new(),
        });
    }
    threads.sort_by(|a, b| b.ikioi.partial_cmp(&a.ikioi).unwrap());

    println!("\n[threads] 勢い順:");
    for t in &threads {
        println!(
            "  {} (勢い={:.1}/日, has_new={})",
            t.title,
            t.ikioi,
            t.has_new()
        );
    }
}

/// search.rsの動作確認。ff5chの実HTMLには届かないので、想定される結果行を模した
/// 合成HTMLをparse_search_resultsに直接通す。
fn demo_search() {
    let sample = r#"<a href="https://egg.5ch.net/test/read.cgi/livejupiter/1700000000/">なんJ移植スレ(123)</a>"#;
    let hits = search::parse_search_results(sample);
    println!("\n[search] {hits:?}");
    println!("[search] quote_plus(\"Rust 移植\") = {}", search::quote_plus("Rust 移植"));
}

/// nextthread.rsの動作確認。900番以降のレスから次スレを検出する部分だけ、
/// 合成データで動かす(実際のfetchはfetch_thread_titleの中でのみ発生する)。
fn demo_nextthread() {
    let posts = vec![
        models::Post {
            num: 900,
            name: "名無し".to_string(),
            date: String::new(),
            message: "次スレ https://egg.5ch.net/test/read.cgi/livejupiter/1700001000/".to_string(),
        },
    ];
    let candidates = nextthread::find_next_thread_candidates(
        "https://egg.5ch.net/livejupiter/",
        "1699999999.dat",
        &posts,
    );
    println!("[nextthread] 次スレ候補: {candidates:?}");

    let html = "<title>移植スレ part2 - 5ch.net</title>";
    println!("[nextthread] タイトル抽出: {:?}", nextthread::extract_title(html));
}


/// history.rsの動作確認。実際にJSONファイルへ書き込み、別インスタンスで読み直して
/// 永続化が効いていることを確かめる。
async fn demo_history() {
    let path = std::env::temp_dir().join("x5ch_rs_demo_history.json");
    let _ = std::fs::remove_file(&path);

    {
        let manager = history::Manager::new(&path);
        manager
            .add_new_thread("移植進捗スレ", "https://egg.5ch.net/livejupiter/", "1700000000.dat")
            .await;
    }

    // 別インスタンスとして読み直す = プロセスを跨いでも履歴が残ることの確認。
    let manager2 = history::Manager::new(&path);
    let exists = manager2
        .exists("https://egg.5ch.io/livejupiter/", "1700000000.dat")
        .await;
    println!("\n[history] 永続化されたか: {exists} (path={})", path.display());

    let _ = std::fs::remove_file(&path);
}


/// export.rsの動作確認。第3章と同じ合成HTMLをparse_posts_for_exportに通し、
/// ExportResultを組み立ててJSON/Markdown両方の出力を確認する。
fn demo_export() {
    let sample_html = r##"
    <div class="clear post">
      <span class="postid">1</span>
      <span class="postusername"><b>名無しさん</b></span>
      <span class="date">2026/09/14(月) 10:00:00.12</span>
      <span class="uid">ID:abc123</span>
      <div class="post-content">初めてのエクスポートテスト<br>ttp://example.com/ を貼ってみる</div>
    </div>
    "##;

    let posts = export::parse_posts_for_export(sample_html, "5ch:egg.5ch.io/livejupiter/1700000000");
    let result = export::ExportResult {
        source: export::ExportSource {
            provider: "5ch".to_string(),
            board_url: "https://egg.5ch.io/livejupiter/".to_string(),
            dat_file: "1700000000.dat".to_string(),
            thread_url: "https://egg.5ch.io/test/read.cgi/livejupiter/1700000000/".to_string(),
            scraped_at: "2026-09-14T12:00:00Z".to_string(),
        },
        thread: export::ExportThread {
            external_id: "5ch:egg.5ch.io/livejupiter/1700000000".to_string(),
            title: "テストスレ".to_string(),
            post_count: posts.len(),
            board_name: Some("なんJ".to_string()),
            created_at: Some(export::dat_timestamp_to_rfc3339("1700000000.dat")),
        },
        posts,
    };

    println!("\n[export] JSON:\n{}", result.to_pretty_json().unwrap());
    println!("\n[export] Markdown:\n{}", result.to_markdown());
}


/// browser.rsの動作確認。実際の5ch.ioには接続できないので、ここでは
/// 「各モジュールの呼び出しがちゃんと配線されているか」「失敗時にBrowserErrorへ
/// きちんと変換されるか」を確認する。build_read_urlだけは純粋関数なので実際に成功例も見せる。
async fn demo_browser() {
    println!(
        "\n[browser] build_read_url = {:?}",
        browser::build_read_url("https://egg.5ch.io/livejupiter/", "1700000000.dat")
    );

    let history_store = history::NullHistory;
    let b = browser::Browser::new(
        "x5ch_rs/0.1 (learning-rust)",
        history_store,
        std::time::Duration::from_secs(60),
    );

    // menu.5ch.ioには届かないので、JSON/HTML両方失敗してMenuErrorになるはず。
    match b.get_menu().await {
        Ok(cats) => println!("[browser] メニュー取得成功(想定外): {} categories", cats.len()),
        Err(e) => println!("[browser] メニュー取得は想定通り失敗: {e}"),
    }
}


/// config.rsの動作確認。環境変数を1つだけ上書きして、優先順位を確認する。
fn demo_config() {
    std::env::set_var("X5CH_HISTORY_FILE", "/tmp/custom_history.json");
    let cfg = config::load_config();
    println!("\n[config] history_file = {}", cfg.history_file);
    println!("[config] lock_file(デフォルト) = {}", cfg.lock_file);
    println!("[config] user_agent = {}", cfg.user_agent);
    std::env::remove_var("X5CH_HISTORY_FILE");
}

/// lock.rsの動作確認。自プロセスのPIDファイルを書いて読み直し、
/// 生存確認(process_alive)まで一通り動かす。実際のflock取得はデモ内では行わない
/// (デモプロセス自身のロック取得は他の実行と競合しうるため)。
fn demo_lock() {
    let pid_path = std::env::temp_dir().join("x5ch_rs_demo.pid");
    let pid_path_str = pid_path.to_string_lossy().into_owned();

    lock::write_pid(&pid_path_str).expect("PIDファイル書き込みに失敗");
    let pid = lock::read_pid(&pid_path_str);
    println!("\n[lock] 書き込んだPID: {pid:?}");
    println!("[lock] 自プロセスは生存している: {}", lock::process_alive(pid.unwrap()));

    lock::remove_pid(&pid_path_str);
    println!("[lock] PIDファイル削除後の再読み込み: {:?}", lock::read_pid(&pid_path_str));
}


/// discord.rsの動作確認。トークン未設定なのでenabled()がfalseになり、
/// create_thread/send_messageが安全に早期returnすることを確認する。
async fn demo_discord() {
    let m = discord::Manager::new("", "");
    println!("\n[discord] enabled (未設定) = {}", m.enabled());

    let post = models::Post {
        num: 1,
        name: "名無しさん".to_string(),
        date: "2026/09/14 10:00:00".to_string(),
        message: "テスト投稿".to_string(),
    };
    // enabled()==falseなので、実際には何も送信せずOk(())が返るはず。
    match m.send_message("dummy-thread-id", &post).await {
        Ok(()) => println!("[discord] 無効時のsend_messageは何もせず正常終了"),
        Err(e) => println!("[discord] 予期しないエラー: {e}"),
    }

    println!(
        "[discord] truncate_runes(\"あ\"×100, 95) の長さ = {} 文字",
        discord::truncate_runes(&"あ".repeat(100), 95).chars().count()
    );
}


/// transfer.rsの動作確認。実際にWorkerをバックグラウンド起動し、
/// enqueue→status_string→killまで一通り動かす。Discordは無効化(トークン空)のままなので
/// 実際の転送は行われないが、キュー管理とバックグラウンドループの起動/終了は本物。
async fn demo_transfer() {
    let history_path = std::env::temp_dir().join("x5ch_rs_demo_transfer_history.json");
    let queue_path = std::env::temp_dir().join("x5ch_rs_demo_transfer_queue.json");
    let _ = std::fs::remove_file(&history_path);
    let _ = std::fs::remove_file(&queue_path);

    let history = std::sync::Arc::new(history::Manager::new(&history_path));
    let browser = std::sync::Arc::new(browser::Browser::new(
        "x5ch_rs/0.1 (transfer-demo)",
        history.clone(),
        std::time::Duration::from_secs(60),
    ));
    let discord_mgr = std::sync::Arc::new(discord::Manager::new("", "")); // 空トークン=無効

    let worker = std::sync::Arc::new(transfer::Worker::with_delays(
        browser,
        discord_mgr,
        history,
        &queue_path,
        std::time::Duration::from_millis(300),
        std::time::Duration::from_millis(300),
        std::time::Duration::from_millis(100),
    ));

    // 注意: ここではあえてworker.start()を呼ばない。start()するとバックグラウンド
    // ループが実スレッドのfetchを試み、5ch.ioに繋がらないこのサンドボックスでは
    // 「取得失敗→再キュー→再試行...」を延々繰り返してしまう(本番のRetry設計としては
    // 正しい動きだが、デモが終わらなくなる)。なのでここではキュー管理メソッド
    // (enqueue/status_string/remaining_threads/delete_at)だけを動かす。
    worker
        .enqueue(transfer::Task {
            title: "移植進捗スレ".to_string(),
            board_url: "https://egg.5ch.io/livejupiter/".to_string(),
            dat_file: "1700000000.dat".to_string(),
        })
        .await;

    println!("\n[transfer] status = {:?}", worker.status_string().await);
    println!("[transfer] remaining_threads = {}", worker.remaining_threads().await);
    println!("[transfer] busy = {}", worker.busy().await);

    let removed = worker.delete_at(0).await;
    println!("[transfer] delete_at(0) = {:?}", removed.map(|t| t.title));
    println!("[transfer] remaining_threads(削除後) = {}", worker.remaining_threads().await);

    let _ = std::fs::remove_file(&history_path);
    let _ = std::fs::remove_file(&queue_path);
}


/// errors.rsの#[from]変換の確認。
/// fetcher.fetch()が返すFetchErrorを、`?`だけでX5chErrorに自動変換して上に伝播させる。
async fn demo_error_propagation(fetcher: &Fetcher) -> Result<(), X5chError> {
    let (_, _) = fetcher
        .fetch("https://raw.githubusercontent.com/kurookuroda/5chterm_py/main/does-not-exist.md")
        .await?; // ここでFetchErrorが自動的にX5chError::Fetchに変換される
    Ok(())
}
