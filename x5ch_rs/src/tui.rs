//! 対話TUI。Python版 tui/app.py・tui/screens.py に対応。
//!
//! メインメニュー→板一覧→スレ一覧→スレ読みの一本道に加えて、検索モーダル・
//! キュー管理画面・履歴管理画面を実装している。Webhook個別送信・Export・
//! 番号指定コマンド(w/e/E/mの数値プレフィックス)は範囲外。
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
use crate::discord;
use crate::history::{Manager, RecentThread};
use crate::models::{Board, Category, Post, ThreadInfo};
use crate::threads::HistoryStore;
use crate::transfer::{self, Worker};

const DEFAULT_STATUS: &str =
    "j/k:移動 Enter:選択 b/Esc:戻る r:再読込 s:検索 t:キュー管理 H:履歴管理 m:送信予約 q:終了";

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

/// 「今選ばれている行を決定(Enter)した」結果、次に何をすべきかを表す。
/// I/Oが要るケース(スレ一覧やレスの取得、削除)は、ここでは実行せずに呼び出し側へ委譲する
/// —この分離のおかげで、activate_currentは純粋関数のままユニットテストできる。
#[derive(Debug, Clone)]
pub enum Action {
    None,
    PushBoardList(Category),
    LoadThreadsForBoard(Board),
    LoadPostsForThread(ThreadInfo),
    DeleteQueueAt(usize),
    DeleteHistoryEntry { board_url: String, dat_file: String },
}

pub fn activate_current(screen: &Screen) -> Action {
    match screen {
        Screen::MainMenu { entries, selected } => match entries.get(*selected) {
            Some(MenuEntry::Recent(t)) => Action::LoadPostsForThread(t.clone()),
            Some(MenuEntry::Category(c)) => Action::PushBoardList(c.clone()),
            None => Action::None,
        },
        Screen::BoardList { category, selected } => match category.boards.get(*selected) {
            Some(b) => Action::LoadThreadsForBoard(b.clone()),
            None => Action::None,
        },
        Screen::ThreadList { threads, selected, .. } => match threads.get(*selected) {
            Some(t) => Action::LoadPostsForThread(t.clone()),
            None => Action::None,
        },
        Screen::QueueManage { tasks, selected } => {
            if tasks.is_empty() {
                Action::None
            } else {
                Action::DeleteQueueAt(*selected)
            }
        }
        Screen::HistoryManage { items, selected } => match items.get(*selected) {
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
                .map(|e| match e {
                    MenuEntry::Recent(t) => ListItem::new(format!("★ {}", t.title)),
                    MenuEntry::Category(c) => ListItem::new(c.title.clone()),
                })
                .collect();
            (items, *selected)
        }
        Screen::BoardList { category, selected } => {
            let items = category
                .boards
                .iter()
                .map(|b| ListItem::new(b.title.clone()))
                .collect();
            (items, *selected)
        }
        Screen::ThreadList { threads, selected, .. } => {
            let items = threads
                .iter()
                .map(|t| {
                    let mark = if t.has_new() { "● " } else { "  " };
                    ListItem::new(format!("{mark}{} ({}) 勢い{:.1}", t.title, t.count, t.ikioi))
                })
                .collect();
            (items, *selected)
        }
        Screen::QueueManage { tasks, .. } => {
            let items = if tasks.is_empty() {
                vec![ListItem::new("(待機中のタスクはありません)")]
            } else {
                tasks.iter().map(|t| ListItem::new(t.title.clone())).collect()
            };
            (items, 0)
        }
        Screen::HistoryManage { items, .. } => {
            let list_items = if items.is_empty() {
                vec![ListItem::new("(履歴はありません)")]
            } else {
                items
                    .iter()
                    .map(|rt| {
                        ListItem::new(format!(
                            "{} (Read: {})",
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

    let footer = Paragraph::new(state.status.as_str());
    frame.render_widget(footer, chunks[1]);
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

    let main_menu = load_main_menu(&browser, &history).await;
    let mut state = AppState {
        stack: vec![main_menu],
        status: DEFAULT_STATUS.to_string(),
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
            // ThreadList上で'm'を押すと、ハイライト中のスレッドをDiscord転送キューへ
            // 予約する(Python版 ThreadListScreen.action_enqueue に対応)。
            KeyCode::Char('m') => {
                if let Screen::ThreadList { threads, selected, .. } = state.top() {
                    if let Some(t) = threads.get(*selected).cloned() {
                        if !discord_mgr.enabled() {
                            state.status = "Discordトークン/チャンネルIDが未設定です".to_string();
                        } else {
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
                    }
                }
            }
            KeyCode::Enter => match activate_current(state.top()) {
                Action::None => {}
                Action::PushBoardList(category) => {
                    state.push(Screen::BoardList { category, selected: 0 });
                }
                Action::LoadThreadsForBoard(mut board) => {
                    state.status = "読み込み中...".to_string();
                    terminal.draw(|f| draw(f, &state))?;
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
                    terminal.draw(|f| draw(f, &state))?;
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
            },
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
    fn app_state_pop_refuses_to_close_last_screen() {
        let mut state = AppState { stack: vec![sample_menu()], status: String::new() };
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
}
