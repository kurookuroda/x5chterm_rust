//! 対話TUI。Python版 tui/app.py・tui/screens.py に対応。
//!
//! メインメニュー→板一覧→スレ一覧→スレ読みの一本道に加えて、検索モーダル・
//! キュー管理画面・履歴管理画面・Webhook個別送信・番号指定コマンドを実装している。
//! Export(`e`/`E`)は範囲外。
//!
//! ここでも他の章と同じ設計方針を貫いている: 「画面遷移の状態管理(純粋なロジック)」と
//! 「実際の描画・キー入力(I/O)」をはっきり分ける。前者はターミナルなしでcargo testできる。

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;

use crate::browser::Browser;
use crate::config;
use crate::discord;
use crate::fetch::Fetcher;
use crate::history::{Manager, RecentThread};
use crate::models::{Board, Category, Post, ThreadInfo};
use crate::threads::HistoryStore;
use crate::transfer::{self, Worker};
use crate::webhook;

const DEFAULT_STATUS: &str = "j/k:移動 Enter:選択 b/Esc:戻る r:再読込 s:検索 t:キュー管理 \
H:履歴管理 m:送信予約 w/W:Webhook送信 0-9+Enter/コマンド:番号指定 q:終了";

type Term = Terminal<CrosstermBackend<io::Stdout>>;

/// MainMenuScreenの1行(Python版の _RecentEntry / _CategoryEntry に対応)。
#[derive(Debug, Clone)]
pub enum MenuEntry {
    Recent(ThreadInfo),
    Category(Category),
}

/// 画面スタックの1枚。Textualのpush_screen/pop_screenと同じ「スタック」モデルを、
/// Rust版ではVec<Screen>として素直に表現する。
#[derive(Debug, Clone)]
pub enum Screen {
    MainMenu { entries: Vec<MenuEntry>, selected: usize },
    BoardList { category: Category, selected: usize },
    ThreadList { title: String, threads: Vec<ThreadInfo>, selected: usize },
    ThreadPager { thread: ThreadInfo, posts: Vec<Post>, scroll: usize },
    /// 検索キーワード入力モーダル(Python版 SearchModal に対応)。
    Search { input: String },
    /// 転送待機列の一覧・削除(Python版 QueueManageScreen に対応)。
    QueueManage { tasks: Vec<transfer::Task>, selected: usize },
    /// 閲覧履歴の一覧・削除(Python版 HistoryManageScreen に対応)。
    HistoryManage { items: Vec<RecentThread>, selected: usize },
}

impl Screen {
    fn item_count(&self) -> usize {
        match self {
            Screen::MainMenu { entries, .. } => entries.len(),
            Screen::BoardList { category, .. } => category.boards.len(),
            Screen::ThreadList { threads, .. } => threads.len(),
            Screen::QueueManage { tasks, .. } => tasks.len(),
            Screen::HistoryManage { items, .. } => items.len(),
            Screen::ThreadPager { .. } | Screen::Search { .. } => 0,
        }
    }

    fn selected_mut(&mut self) -> Option<&mut usize> {
        match self {
            Screen::MainMenu { selected, .. }
            | Screen::BoardList { selected, .. }
            | Screen::ThreadList { selected, .. }
            | Screen::QueueManage { selected, .. }
            | Screen::HistoryManage { selected, .. } => Some(selected),
            Screen::ThreadPager { .. } | Screen::Search { .. } => None,
        }
    }

    /// この画面が「番号入力→ジャンプ」をサポートするリスト画面かどうか。
    /// Python版 IndexedListViewMixin を適用しているクラス群に対応。
    fn supports_index_buffer(&self) -> bool {
        matches!(
            self,
            Screen::MainMenu { .. }
                | Screen::BoardList { .. }
                | Screen::ThreadList { .. }
                | Screen::QueueManage { .. }
                | Screen::HistoryManage { .. }
        )
    }
}

/// 選択中カーソルを1つ動かす(負数で上、正数で下)。リストの端で折り返す。
pub fn move_selection(screen: &mut Screen, delta: isize) {
    let len = screen.item_count();
    if len == 0 {
        return;
    }
    if let Some(sel) = screen.selected_mut() {
        let next = (*sel as isize + delta).rem_euclid(len as isize);
        *sel = next as usize;
    }
}

/// ThreadPagerでのスクロール(レス単位)。範囲外にははみ出さない。
pub fn scroll_pager(screen: &mut Screen, delta: isize) {
    if let Screen::ThreadPager { posts, scroll, .. } = screen {
        let max = posts.len().saturating_sub(1) as isize;
        *scroll = (*scroll as isize + delta).clamp(0, max) as usize;
    }
}

/// 入力中の数字バッファ文字列を、画面の項目数に照らして有効なインデックスに解決する。
/// Python版 IndexedListViewMixin._resolved_index() に対応する純粋関数。
pub fn resolved_index(buffer: &str, item_count: usize) -> Option<usize> {
    if buffer.is_empty() {
        return None;
    }
    let idx: usize = buffer.parse().ok()?;
    if idx < item_count {
        Some(idx)
    } else {
        None
    }
}

/// 「今選ばれている行を決定(Enter)した」結果、次に何をすべきかを表す。
/// I/Oが要るケース(スレ一覧やレスの取得、削除)は、ここでは実行せずに呼び出し側へ委譲する
/// —この分離のおかげで、activate_at/activate_currentは純粋関数のままユニットテストできる。
#[derive(Debug, Clone)]
pub enum Action {
    None,
    PushBoardList(Category),
    LoadThreadsForBoard(Board),
    LoadPostsForThread(ThreadInfo),
    DeleteQueueAt(usize),
    DeleteHistoryEntry { board_url: String, dat_file: String },
}

/// ハイライト中の行(screen自身が持つselected)を対象に決定する。
pub fn activate_current(screen: &Screen) -> Action {
    let idx = match screen {
        Screen::MainMenu { selected, .. }
        | Screen::BoardList { selected, .. }
        | Screen::ThreadList { selected, .. }
        | Screen::QueueManage { selected, .. }
        | Screen::HistoryManage { selected, .. } => *selected,
        Screen::ThreadPager { .. } | Screen::Search { .. } => return Action::None,
    };
    activate_at(screen, idx)
}

/// 明示的に指定したインデックスの行を対象に決定する。番号入力+Enterのジャンプ用。
pub fn activate_at(screen: &Screen, idx: usize) -> Action {
    match screen {
        Screen::MainMenu { entries, .. } => match entries.get(idx) {
            Some(MenuEntry::Recent(t)) => Action::LoadPostsForThread(t.clone()),
            Some(MenuEntry::Category(c)) => Action::PushBoardList(c.clone()),
            None => Action::None,
        },
        Screen::BoardList { category, .. } => match category.boards.get(idx) {
            Some(b) => Action::LoadThreadsForBoard(b.clone()),
            None => Action::None,
        },
        Screen::ThreadList { threads, .. } => match threads.get(idx) {
            Some(t) => Action::LoadPostsForThread(t.clone()),
            None => Action::None,
        },
        Screen::QueueManage { tasks, .. } => {
            if idx < tasks.len() {
                Action::DeleteQueueAt(idx)
            } else {
                Action::None
            }
        }
        Screen::HistoryManage { items, .. } => match items.get(idx) {
            Some(rt) => Action::DeleteHistoryEntry {
                board_url: rt.thread_info.board_url.clone(),
                dat_file: rt.thread_info.dat_file.clone(),
            },
            None => Action::None,
        },
        Screen::ThreadPager { .. } | Screen::Search { .. } => Action::None,
    }
}

/// 画面スタック。stack.len()==1 のとき(メインメニュー)は'b'で戻れない
/// (Python版もBINDINGSにback操作を持たないのと同じ)。
pub struct AppState {
    pub stack: Vec<Screen>,
    pub status: String,
    /// 番号入力中のバッファ(Python版 IndexedListViewMixin._index_buffer に対応)。
    pub index_buffer: String,
}

impl AppState {
    pub fn top(&self) -> &Screen {
        self.stack.last().expect("stackは常に最低1枚持つ")
    }

    pub fn top_mut(&mut self) -> &mut Screen {
        self.stack.last_mut().expect("stackは常に最低1枚持つ")
    }

    pub fn push(&mut self, screen: Screen) {
        self.stack.push(screen);
    }

    /// 戻れたらtrue、これ以上戻れない(メインメニューのみ)ならfalse。
    pub fn pop(&mut self) -> bool {
        if self.stack.len() > 1 {
            self.stack.pop();
            true
        } else {
            false
        }
    }
}

/// メインメニューの初期ロード。Python版 MainMenuScreen.load() に対応する非同期関数
/// (activate_current等の純粋関数とは違い、ここはhistory/browserへのI/Oを含む)。
async fn load_main_menu(browser: &Browser<Arc<Manager>>, history: &Arc<Manager>) -> Screen {
    let recent = history.get_recent_threads().await;
    let categories = browser.get_menu().await.unwrap_or_default();

    let mut entries = Vec::new();
    for rt in recent.into_iter().take(15) {
        entries.push(MenuEntry::Recent(rt.thread_info));
    }
    for cat in categories {
        entries.push(MenuEntry::Category(cat));
    }

    Screen::MainMenu { entries, selected: 0 }
}

fn menu_list_lines(screen: &Screen) -> (Vec<ListItem<'static>>, usize) {
    match screen {
        Screen::MainMenu { entries, selected } => {
            let items = entries
                .iter()
                .enumerate()
                .map(|(i, e)| match e {
                    MenuEntry::Recent(t) => ListItem::new(format!("{i:>2} ★ {}", t.title)),
                    MenuEntry::Category(c) => ListItem::new(format!("{i:>2}  {}", c.title)),
                })
                .collect();
            (items, *selected)
        }
        Screen::BoardList { category, selected } => {
            let items = category
                .boards
                .iter()
                .enumerate()
                .map(|(i, b)| ListItem::new(format!("{i:>2} {}", b.title)))
                .collect();
            (items, *selected)
        }
        Screen::ThreadList { threads, selected, .. } => {
            let items = threads
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mark = if t.has_new() { "● " } else { "  " };
                    ListItem::new(format!(
                        "{i:>2} {mark}{} ({}) 勢い{:.1}",
                        t.title, t.count, t.ikioi
                    ))
                })
                .collect();
            (items, *selected)
        }
        Screen::QueueManage { tasks, .. } => {
            let items = if tasks.is_empty() {
                vec![ListItem::new("(待機中のタスクはありません)")]
            } else {
                tasks
                    .iter()
                    .enumerate()
                    .map(|(i, t)| ListItem::new(format!("{i:>2} {}", t.title)))
                    .collect()
            };
            (items, 0)
        }
        Screen::HistoryManage { items, .. } => {
            let list_items = if items.is_empty() {
                vec![ListItem::new("(履歴はありません)")]
            } else {
                items
                    .iter()
                    .enumerate()
                    .map(|(i, rt)| {
                        ListItem::new(format!(
                            "{i:>2} {} (Read: {})",
                            rt.thread_info.title, rt.thread_info.last_read
                        ))
                    })
                    .collect()
            };
            (list_items, 0)
        }
        Screen::ThreadPager { .. } | Screen::Search { .. } => (Vec::new(), 0),
    }
}

fn screen_title(screen: &Screen) -> &str {
    match screen {
        Screen::MainMenu { .. } => "5chanterm - メインメニュー",
        Screen::BoardList { category, .. } => category.title.as_str(),
        Screen::ThreadList { title, .. } => title.as_str(),
        Screen::ThreadPager { thread, .. } => thread.title.as_str(),
        Screen::Search { .. } => "検索キーワード (Escでキャンセル)",
        Screen::QueueManage { .. } => "転送待機列の管理 (Enterで削除 / b:戻る)",
        Screen::HistoryManage { .. } => "閲覧履歴の管理 (Enterで削除 / b:戻る)",
    }
}

fn draw(frame: &mut ratatui::Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.size());

    let screen = state.top();
    let title = screen_title(screen);

    match screen {
        Screen::ThreadPager { posts, scroll, .. } => {
            let lines: Vec<Line> = posts
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let style = if i == *scroll {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    Line::from(vec![Span::styled(
                        format!("{} {} {}\n{}", p.num, p.name, p.date, p.message),
                        style,
                    )])
                })
                .collect();
            let para = Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title))
                .wrap(Wrap { trim: false });
            frame.render_widget(para, chunks[0]);
        }
        Screen::Search { input } => {
            let para = Paragraph::new(format!("> {input}_"))
                .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, chunks[0]);
        }
        _ => {
            let (items, selected) = menu_list_lines(screen);
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan));
            let mut list_state = ListState::default();
            list_state.select(Some(selected));
            frame.render_stateful_widget(list, chunks[0], &mut list_state);
        }
    }

    // 番号入力中は「> 12」のように先頭に表示する(Python版の#index-statusラベル相当)。
    let footer_text = if state.index_buffer.is_empty() {
        state.status.clone()
    } else {
        format!("> {}  {}", state.index_buffer, state.status)
    };
    let footer = Paragraph::new(footer_text);
    frame.render_widget(footer, chunks[1]);
}

/// 未読/単発のレスをWebhookへ送信し、状態表示用の短い文字列を返す。
/// ThreadList/ThreadPagerどちらの'w'/'W'ハンドラからも(ハイライト対象でも番号指定でも)共通で使う。
async fn send_via_webhook(fetcher: &Fetcher, urls_file: &str, posts: &[Post]) -> String {
    let urls = webhook::load_webhook_urls(urls_file);
    if urls.is_empty() {
        return "Webhook URLが設定されていません".to_string();
    }
    if posts.is_empty() {
        return "新着なし".to_string();
    }

    let failures = webhook::broadcast_posts(fetcher, &urls, posts, webhook::MESSAGE_INTERVAL).await;
    let total_failed: usize = failures.values().map(std::vec::Vec::len).sum();

    if total_failed > 0 {
        format!("送信完了(一部失敗: {total_failed}件)")
    } else {
        format!("{}件送信しました", posts.len())
    }
}

/// 指定スレッドの未読レスをWebhookへ送信する(ThreadListの'w'。ハイライト対象・番号指定対象共通)。
async fn webhook_send_for(
    t: &ThreadInfo,
    browser: &Browser<Arc<Manager>>,
    history: &Manager,
    fetcher: &Fetcher,
    urls_file: &str,
    terminal: &mut Term,
    state: &mut AppState,
) -> io::Result<()> {
    let mut t = t.clone();
    state.status = "取得中...".to_string();
    terminal.draw(|f| draw(f, state))?;

    match browser.get_thread_data(&mut t).await {
        Ok(posts) => {
            let last_read = history.get_last_read(&t.board_url, &t.dat_file).await;
            let unread: Vec<Post> = posts.into_iter().filter(|p| p.num > last_read).collect();
            state.status = "送信中...".to_string();
            terminal.draw(|f| draw(f, state))?;
            state.status = send_via_webhook(fetcher, urls_file, &unread).await;
        }
        Err(e) => state.status = format!("取得エラー: {e}"),
    }
    Ok(())
}

/// 指定スレッドをDiscord転送キューへ予約する(ThreadListの'm')。
async fn enqueue_for(
    t: &ThreadInfo,
    worker: &Worker,
    discord_mgr: &discord::Manager,
    history: &Manager,
    state: &mut AppState,
) {
    if !discord_mgr.enabled() {
        state.status = "Discordトークン/チャンネルIDが未設定です".to_string();
        return;
    }
    worker
        .enqueue(transfer::Task {
            title: t.title.clone(),
            board_url: t.board_url.clone(),
            dat_file: t.dat_file.clone(),
        })
        .await;
    history.add_new_thread(&t.title, &t.board_url, &t.dat_file).await;
    state.status = format!("キューに追加: {}", t.title);
}

/// 指定スレッドの閲覧履歴を削除する(ThreadListの'H'。第6章のManager::delete_threadを再利用)。
async fn delete_history_for(t: &ThreadInfo, history: &Manager, state: &mut AppState) {
    let ok = history.delete_thread(&t.board_url, &t.dat_file).await;
    state.status = if ok {
        format!("履歴を削除しました: {}", t.title)
    } else {
        "削除対象の履歴がありません".to_string()
    };
}

/// Enterで確定したActionを実行する。ハイライト対象(activate_current)・番号指定
/// (activate_at)のどちらから来たActionも、ここで同じ処理を通る。
async fn handle_action(
    action: Action,
    browser: &Browser<Arc<Manager>>,
    history: &Manager,
    worker: &Worker,
    terminal: &mut Term,
    state: &mut AppState,
) -> io::Result<()> {
    match action {
        Action::None => {}
        Action::PushBoardList(category) => {
            state.push(Screen::BoardList { category, selected: 0 });
        }
        Action::LoadThreadsForBoard(mut board) => {
            state.status = "読み込み中...".to_string();
            terminal.draw(|f| draw(f, state))?;
            match browser.get_threads(&mut board, false).await {
                Ok(threads) => {
                    state.push(Screen::ThreadList {
                        title: board.title.clone(),
                        threads,
                        selected: 0,
                    });
                    state.status = DEFAULT_STATUS.to_string();
                }
                Err(e) => state.status = format!("取得エラー: {e}"),
            }
        }
        Action::LoadPostsForThread(mut thread) => {
            state.status = "読み込み中...".to_string();
            terminal.draw(|f| draw(f, state))?;
            match browser.get_thread_data(&mut thread).await {
                Ok(posts) => {
                    state.push(Screen::ThreadPager { thread, posts, scroll: 0 });
                    state.status = DEFAULT_STATUS.to_string();
                }
                Err(e) => state.status = format!("取得エラー: {e}"),
            }
        }
        Action::DeleteQueueAt(idx) => {
            let deleted = worker.delete_at(idx).await;
            state.status = match &deleted {
                Some(t) => format!("削除しました: {}", t.title),
                None => DEFAULT_STATUS.to_string(),
            };
            let tasks = worker.queue_list().await;
            if let Screen::QueueManage { tasks: slot, selected } = state.top_mut() {
                *slot = tasks;
                if !slot.is_empty() && *selected >= slot.len() {
                    *selected = slot.len() - 1;
                }
            }
        }
        Action::DeleteHistoryEntry { board_url, dat_file } => {
            let ok = history.delete_thread(&board_url, &dat_file).await;
            state.status = if ok {
                "履歴を削除しました".to_string()
            } else {
                "削除に失敗しました".to_string()
            };
            let items = history.get_recent_threads().await;
            if let Screen::HistoryManage { items: slot, selected } = state.top_mut() {
                *slot = items;
                if !slot.is_empty() && *selected >= slot.len() {
                    *selected = slot.len() - 1;
                }
            }
        }
    }
    Ok(())
}

/// TUI本体を起動する。Python版 X5chApp.run() 相当。
/// ターミナル制御(raw mode / alternate screen)を含むため、実際に対話端末上でのみ動作する
/// —このサンドボックスには対話TTYが無いため、ここは「コンパイルは通るが実行はユーザー環境で」
/// という章になる(第1章のHTTPS同様、環境依存の限界を正直に書いておく)。
pub async fn run(
    browser: Arc<Browser<Arc<Manager>>>,
    history: Arc<Manager>,
    discord_mgr: Arc<discord::Manager>,
    worker: Arc<Worker>,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    worker.start();

    // Webhook個別送信('w'/'W')用。DiscordのBot APIとは別経路なので、
    // Worker/Browserとは独立したFetcherを1本持たせる(cli.rsのwebhook-sendコマンドと同じ構成)。
    let cfg = config::load_config();
    let webhook_fetcher = Fetcher::new(cfg.user_agent.clone());
    let webhook_urls_file = cfg.webhook_urls_file.clone();

    let main_menu = load_main_menu(&browser, &history).await;
    let mut state = AppState {
        stack: vec![main_menu],
        status: DEFAULT_STATUS.to_string(),
        index_buffer: String::new(),
    };

    loop {
        terminal.draw(|f| draw(f, &state))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // 検索キーワード入力中は、通常のナビゲーションキーではなく文字入力として扱う
        // (Python版 SearchModal の on_input_submitted / on_key に対応)。
        if let Screen::Search { input } = state.top_mut() {
            match key.code {
                KeyCode::Esc => {
                    state.pop();
                    state.status = DEFAULT_STATUS.to_string();
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => {
                    input.push(c);
                }
                KeyCode::Enter => {
                    let keyword = input.trim().to_string();
                    state.pop();
                    state.status = DEFAULT_STATUS.to_string();
                    if !keyword.is_empty() {
                        state.status = format!("「{keyword}」を検索中...");
                        terminal.draw(|f| draw(f, &state))?;
                        match browser.search_global(&keyword).await {
                            Ok(results) if !results.is_empty() => {
                                state.push(Screen::ThreadList {
                                    title: format!("検索: {keyword}"),
                                    threads: results,
                                    selected: 0,
                                });
                                state.status = DEFAULT_STATUS.to_string();
                            }
                            Ok(_) => state.status = "該当なし".to_string(),
                            Err(e) => state.status = format!("検索エラー: {e}"),
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        // --- 番号入力(IndexedListViewMixin相当) ---
        // 1. 数字キーはバッファに積むだけ(対応画面のみ)。
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_digit() && state.top().supports_index_buffer() {
                state.index_buffer.push(c);
                continue;
            }
        }

        // 2. バッファに何か入っている間は、Backspace/Enter/コマンドキーを
        //    「番号指定」として先に処理する。それ以外のキーはバッファを破棄した上で
        //    通常の処理(下のmatch)に流す — Python版の「未対応キーはバッファ破棄のうえ
        //    通常処理へ」という挙動と同じ。
        if !state.index_buffer.is_empty() {
            match key.code {
                KeyCode::Backspace => {
                    state.index_buffer.pop();
                    continue;
                }
                KeyCode::Enter => {
                    let idx = resolved_index(&state.index_buffer, state.top().item_count());
                    state.index_buffer.clear();
                    match idx {
                        Some(i) => {
                            let action = activate_at(state.top(), i);
                            handle_action(action, &browser, &history, &worker, &mut terminal, &mut state)
                                .await?;
                        }
                        None => state.status = "無効な番号です".to_string(),
                    }
                    continue;
                }
                KeyCode::Char(c @ ('w' | 'm' | 'H')) if matches!(state.top(), Screen::ThreadList { .. }) => {
                    let idx = resolved_index(&state.index_buffer, state.top().item_count());
                    state.index_buffer.clear();
                    let Some(i) = idx else {
                        state.status = "無効な番号です".to_string();
                        continue;
                    };
                    let Screen::ThreadList { threads, .. } = state.top() else {
                        continue;
                    };
                    let Some(t) = threads.get(i).cloned() else {
                        continue;
                    };
                    match c {
                        'w' => {
                            webhook_send_for(
                                &t,
                                &browser,
                                &history,
                                &webhook_fetcher,
                                &webhook_urls_file,
                                &mut terminal,
                                &mut state,
                            )
                            .await?;
                        }
                        'm' => enqueue_for(&t, &worker, &discord_mgr, &history, &mut state).await,
                        'H' => delete_history_for(&t, &history, &mut state).await,
                        _ => unreachable!(),
                    }
                    continue;
                }
                KeyCode::Esc => {
                    // バッファは破棄しつつ、戻る動作自体は下の通常処理に委ねる(Python版と同じ)。
                    state.index_buffer.clear();
                }
                _ => {
                    state.index_buffer.clear();
                }
            }
        }

        match key.code {
            KeyCode::Char('q') if matches!(state.top(), Screen::MainMenu { .. }) => break,
            KeyCode::Char('j') | KeyCode::Down => move_selection(state.top_mut(), 1),
            KeyCode::Char('k') | KeyCode::Up => move_selection(state.top_mut(), -1),
            KeyCode::Char('b') | KeyCode::Esc => {
                state.pop();
            }
            KeyCode::Char('r') if matches!(state.top(), Screen::MainMenu { .. }) => {
                browser.invalidate_menu_cache().await;
                let refreshed = load_main_menu(&browser, &history).await;
                *state.top_mut() = refreshed;
            }
            KeyCode::Char('s') if matches!(state.top(), Screen::MainMenu { .. }) => {
                state.push(Screen::Search { input: String::new() });
            }
            KeyCode::Char('t') if matches!(state.top(), Screen::MainMenu { .. }) => {
                let tasks = worker.queue_list().await;
                state.push(Screen::QueueManage { tasks, selected: 0 });
            }
            KeyCode::Char('H') if matches!(state.top(), Screen::MainMenu { .. }) => {
                let items = history.get_recent_threads().await;
                state.push(Screen::HistoryManage { items, selected: 0 });
            }
            // ThreadList上でハイライト中のスレッドに対する'm'/'w'/'H'
            // (番号指定なしの場合。番号指定版は上のバッファ処理ブロックで済んでいる)。
            KeyCode::Char('m') if matches!(state.top(), Screen::ThreadList { .. }) => {
                if let Screen::ThreadList { threads, selected, .. } = state.top() {
                    if let Some(t) = threads.get(*selected).cloned() {
                        enqueue_for(&t, &worker, &discord_mgr, &history, &mut state).await;
                    }
                }
            }
            KeyCode::Char('w') if matches!(state.top(), Screen::ThreadList { .. }) => {
                if let Screen::ThreadList { threads, selected, .. } = state.top() {
                    if let Some(t) = threads.get(*selected).cloned() {
                        webhook_send_for(
                            &t,
                            &browser,
                            &history,
                            &webhook_fetcher,
                            &webhook_urls_file,
                            &mut terminal,
                            &mut state,
                        )
                        .await?;
                    }
                }
            }
            KeyCode::Char('H') if matches!(state.top(), Screen::ThreadList { .. }) => {
                if let Screen::ThreadList { threads, selected, .. } = state.top() {
                    if let Some(t) = threads.get(*selected).cloned() {
                        delete_history_for(&t, &history, &mut state).await;
                    }
                }
            }
            // ThreadPager上での'w'(未読送信)/'W'(現在位置のみ送信)。
            // Rust版はscrollがそのまま「今見ているレスのインデックス」なので、
            // Python版のような可視位置の逆算(_update_current_res)が不要になっている。
            KeyCode::Char('w') if matches!(state.top(), Screen::ThreadPager { .. }) => {
                if let Screen::ThreadPager { thread, posts, .. } = state.top() {
                    let unread: Vec<Post> = posts
                        .iter()
                        .filter(|p| p.num > thread.last_read)
                        .cloned()
                        .collect();
                    state.status = "送信中...".to_string();
                    terminal.draw(|f| draw(f, &state))?;
                    state.status = send_via_webhook(&webhook_fetcher, &webhook_urls_file, &unread).await;
                }
            }
            KeyCode::Char('W') => {
                if let Screen::ThreadPager { posts, scroll, .. } = state.top() {
                    if let Some(post) = posts.get(*scroll).cloned() {
                        state.status = "送信中...".to_string();
                        terminal.draw(|f| draw(f, &state))?;
                        state.status =
                            send_via_webhook(&webhook_fetcher, &webhook_urls_file, &[post]).await;
                    }
                }
            }
            KeyCode::Enter => {
                let action = activate_current(state.top());
                handle_action(action, &browser, &history, &worker, &mut terminal, &mut state).await?;
            }
            KeyCode::Char('J') => scroll_pager(state.top_mut(), 1),
            KeyCode::Char('K') => scroll_pager(state.top_mut(), -1),
            _ => {}
        }
    }

    worker.kill().await;
    worker.wait_until_stopped().await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_menu() -> Screen {
        Screen::MainMenu {
            entries: vec![
                MenuEntry::Recent(ThreadInfo::new("1.dat")),
                MenuEntry::Category(Category::new("ニュース")),
                MenuEntry::Category(Category::new("趣味")),
            ],
            selected: 0,
        }
    }

    #[test]
    fn move_selection_wraps_around() {
        let mut screen = sample_menu();
        move_selection(&mut screen, -1);
        let Screen::MainMenu { selected, .. } = screen else { unreachable!() };
        assert_eq!(selected, 2); // 0から上に行くと末尾へ折り返す
    }

    #[test]
    fn move_selection_moves_forward() {
        let mut screen = sample_menu();
        move_selection(&mut screen, 1);
        move_selection(&mut screen, 1);
        let Screen::MainMenu { selected, .. } = screen else { unreachable!() };
        assert_eq!(selected, 2);
    }

    #[test]
    fn activate_current_on_category_requests_board_list_push() {
        let mut screen = sample_menu();
        move_selection(&mut screen, 1); // カテゴリ「ニュース」へ
        match activate_current(&screen) {
            Action::PushBoardList(cat) => assert_eq!(cat.title, "ニュース"),
            other => panic!("expected PushBoardList, got {other:?}"),
        }
    }

    #[test]
    fn activate_current_on_recent_requests_load_posts() {
        let screen = sample_menu(); // selected=0 は_RecentEntry
        match activate_current(&screen) {
            Action::LoadPostsForThread(t) => assert_eq!(t.dat_file, "1.dat"),
            other => panic!("expected LoadPostsForThread, got {other:?}"),
        }
    }

    #[test]
    fn activate_at_ignores_screens_own_selected_field() {
        // selected=0のままでも、activate_atに2を渡せば末尾のカテゴリ「趣味」が対象になる。
        let screen = sample_menu();
        match activate_at(&screen, 2) {
            Action::PushBoardList(cat) => assert_eq!(cat.title, "趣味"),
            other => panic!("expected PushBoardList, got {other:?}"),
        }
    }

    #[test]
    fn app_state_pop_refuses_to_close_last_screen() {
        let mut state = AppState { stack: vec![sample_menu()], status: String::new(), index_buffer: String::new() };
        assert!(!state.pop());
        assert_eq!(state.stack.len(), 1);

        state.push(Screen::BoardList { category: Category::new("test"), selected: 0 });
        assert!(state.pop());
        assert_eq!(state.stack.len(), 1);
    }

    #[test]
    fn scroll_pager_clamps_to_post_range() {
        let mut screen = Screen::ThreadPager {
            thread: ThreadInfo::new("1.dat"),
            posts: vec![
                Post { num: 1, name: String::new(), date: String::new(), message: String::new() },
                Post { num: 2, name: String::new(), date: String::new(), message: String::new() },
            ],
            scroll: 0,
        };
        scroll_pager(&mut screen, -5); // 下限より下にははみ出さない
        let Screen::ThreadPager { scroll, .. } = &screen else { unreachable!() };
        assert_eq!(*scroll, 0);

        scroll_pager(&mut screen, 5); // 上限(posts.len()-1)を超えない
        let Screen::ThreadPager { scroll, .. } = &screen else { unreachable!() };
        assert_eq!(*scroll, 1);
    }

    fn sample_task(n: u32) -> transfer::Task {
        transfer::Task {
            title: format!("スレ{n}"),
            board_url: "https://egg.5ch.io/livejupiter/".to_string(),
            dat_file: format!("{n}.dat"),
        }
    }

    #[test]
    fn activate_current_on_queue_manage_requests_delete_at_selected_index() {
        let screen = Screen::QueueManage {
            tasks: vec![sample_task(1), sample_task(2)],
            selected: 1,
        };
        match activate_current(&screen) {
            Action::DeleteQueueAt(idx) => assert_eq!(idx, 1),
            other => panic!("expected DeleteQueueAt, got {other:?}"),
        }
    }

    #[test]
    fn activate_current_on_empty_queue_manage_does_nothing() {
        let screen = Screen::QueueManage { tasks: vec![], selected: 0 };
        assert!(matches!(activate_current(&screen), Action::None));
    }

    #[test]
    fn activate_current_on_history_manage_requests_delete_by_identity() {
        let mut t = ThreadInfo::new("42.dat");
        t.title = "履歴スレ".to_string();
        t.board_url = "https://egg.5ch.io/livejupiter/".to_string();
        let screen = Screen::HistoryManage {
            items: vec![RecentThread { thread_info: t, timestamp: 0 }],
            selected: 0,
        };
        match activate_current(&screen) {
            Action::DeleteHistoryEntry { board_url, dat_file } => {
                assert_eq!(board_url, "https://egg.5ch.io/livejupiter/");
                assert_eq!(dat_file, "42.dat");
            }
            other => panic!("expected DeleteHistoryEntry, got {other:?}"),
        }
    }

    #[test]
    fn move_selection_ignores_search_screen() {
        // Search画面はitem_count()==0なので、move_selectionは何もしない
        // (文字入力はrun()側のSearch専用分岐で扱うため、ここでは動かないのが正しい)。
        let mut screen = Screen::Search { input: "ab".to_string() };
        move_selection(&mut screen, 1);
        let Screen::Search { input } = screen else { unreachable!() };
        assert_eq!(input, "ab");
    }

    #[test]
    fn resolved_index_parses_valid_in_range_number() {
        assert_eq!(resolved_index("2", 5), Some(2));
    }

    #[test]
    fn resolved_index_rejects_out_of_range_or_empty_or_non_numeric() {
        assert_eq!(resolved_index("5", 5), None); // 0..item_count-1が範囲
        assert_eq!(resolved_index("", 5), None);
        assert_eq!(resolved_index("abc", 5), None);
    }

    #[test]
    fn supports_index_buffer_excludes_pager_and_search() {
        assert!(sample_menu().supports_index_buffer());
        assert!(!Screen::Search { input: String::new() }.supports_index_buffer());
        let pager = Screen::ThreadPager { thread: ThreadInfo::new("1.dat"), posts: vec![], scroll: 0 };
        assert!(!pager.supports_index_buffer());
    }

    #[tokio::test]
    async fn send_via_webhook_reports_missing_url_config() {
        let fetcher = Fetcher::new("x5ch_rs/test");
        let missing_path = "/tmp/x5ch_rs_no_such_webhook_urls_file.json";
        let posts = vec![Post {
            num: 1,
            name: "名無し".to_string(),
            date: String::new(),
            message: "test".to_string(),
        }];
        let status = send_via_webhook(&fetcher, missing_path, &posts).await;
        assert_eq!(status, "Webhook URLが設定されていません");
    }

    #[tokio::test]
    async fn send_via_webhook_reports_no_new_posts() {
        let fetcher = Fetcher::new("x5ch_rs/test");
        let path = std::env::temp_dir().join(format!("x5ch_rs_test_wh_urls_{}.json", std::process::id()));
        std::fs::write(&path, r#"["https://example.invalid/webhook"]"#).unwrap();

        let status = send_via_webhook(&fetcher, path.to_str().unwrap(), &[]).await;
        assert_eq!(status, "新着なし");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn delete_history_for_reports_success_and_failure() {
        let path = std::env::temp_dir().join(format!("x5ch_rs_test_tui_history_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let history = Manager::new(&path);

        let mut t = ThreadInfo::new("1.dat");
        t.title = "テストスレ".to_string();
        t.board_url = "https://egg.5ch.io/livejupiter/".to_string();
        history.add_new_thread(&t.title, &t.board_url, &t.dat_file).await;

        let mut state = AppState { stack: vec![sample_menu()], status: String::new(), index_buffer: String::new() };
        delete_history_for(&t, &history, &mut state).await;
        assert!(state.status.contains("削除しました"));

        // 2回目は既に無いので失敗メッセージになるはず。
        delete_history_for(&t, &history, &mut state).await;
        assert_eq!(state.status, "削除対象の履歴がありません");

        let _ = std::fs::remove_file(&path);
    }
}
