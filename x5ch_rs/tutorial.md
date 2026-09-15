# 5chterm_py → Rust移植チュートリアル(全14章)

Rustを何も知らない状態から、5ch専用ターミナルブラウザ `5chterm_py`(Python製)を
Rustに移植しながらRustを学んだ記録です。移植元: https://github.com/kurookuroda/5chterm_py

各章は「Pythonでどう書かれていたか」→「Rustではどう書くか」→「そこで学んだRust concept」
という流れで進めています。実際に動くコードは `x5ch_rs` プロジェクトとして別途お渡し済みです。

## 開発環境について

このチュートリアルはサンドボックス環境(Ubuntu、`apt`経由のRust 1.75.0)で進めました。
最新の`rustup`が使えない制約から、依存クレートのバージョンを何度か調整する場面が
ありましたが、通常`rustup`で最新版を入れている環境であれば、この種の調整はほぼ不要です。

---

## 第1章: 環境構築と `fetch.py` の移植

### 目標
1本のURLを取得して表示するだけの、最小の「動いた!」を作る。

### Pythonでの実装
`fetch.py`は`httpx.AsyncClient`でGETし、リダイレクト自動追従・gzip自動展開・
タイムアウトを`FetchError`/`NetworkFetchError`という独自例外に変換していました。

### Rustでの対応

| やりたいこと | Python | Rust |
|---|---|---|
| 非同期処理の土台 | `asyncio` | `tokio` |
| HTTP取得 | `httpx` | `ureq`(このサンドボックスの制約でreqwestの代わりに採用) |
| エラー型 | 例外クラス階層 | `enum` + `thiserror` |

```rust
// errors.rs
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
```

```rust
// fetch.rsの中核: 同期APIのureqをspawn_blockingで非同期に見せかける
async fn fetch(url: String, user_agent: String) -> Result<(Vec<u8>, String), FetchError> {
    tokio::task::spawn_blocking(move || {
        // ureqでGETし、結果をFetchErrorに変換
    })
    .await
    .expect("blockingタスクがpanicした")
}
```

### 学んだこと
- **`Result<T, E>`**: Pythonの`try/except`の代わりに、失敗しうる関数は戻り値の型で
  成功/失敗を両方表現する。`?`演算子が「エラーなら即リターン」を1文字でやってくれる。
- **所有権とmove**: `tokio::task::spawn_blocking(move || {...})`の`move`は、
  クロージャが変数の所有権を奪うという宣言。1つの値は同時に1人の持ち主しか持てない。
- **rustlsとnative-tlsの違い**: サンドボックスの通信プロキシ経由で`rustls`(自前の
  信頼済みCA一覧を持つ)がTLSエラーになったが、`native-tls`(OSの証明書ストアを
  信頼する)に切り替えたら解決した。「証明書を誰が検証するか」という設計思想の違いを
  体感できた。

---

## 第2章: `models.py` / `errors.py` の移植

### 目標
Pythonの`@dataclass`と例外階層を、Rustの`struct`と`enum`に翻訳する。

### コード

```rust
// models.rs
#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub dat_file: String,
    pub title: String,
    pub count: u32,
    pub ikioi: f64,
    pub board_url: String,
    pub last_read: u32,
    pub url: String,
}

impl ThreadInfo {
    // dat_fileだけ必須、他はデフォルト値というPython版の非対称なデフォルトを
    // 専用コンストラクタで再現(全フィールドDefaultだと表現しづらい)
    pub fn new(dat_file: impl Into<String>) -> Self { /* ... */ }

    // Python版の @property has_new は、Rustでは普通のメソッドになる
    pub fn has_new(&self) -> bool {
        self.count > self.last_read
    }
}
```

```rust
// errors.rs: 継承ではなく、1つのenumに集約する
#[derive(Debug, Error)]
pub enum X5chError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Menu(#[from] MenuError),
    // ...
}
```

### 学んだこと
- **`#[derive(Debug, Clone)]`**: `Debug`は`__repr__`相当、`Clone`は明示的な複製。
  Pythonの`b = a`は参照コピーだが、Rustの`let b = a;`は所有権の「引っ越し」(ムーブ)。
  同じ値を2箇所で使うには`.clone()`が要る。
- **継承の代わりにenum + `#[from]`**: Rustにクラス継承はない。`thiserror`の`#[from]`を
  付けておくと、下位のエラー型を`?`で伝播させた瞬間に自動で上位のenumに変換される。
  Pythonの`except X5chError`で子クラスもまとめて捕まえる、のと同じ効果。

---

## 第3章: `parse.py` の移植

### 目標
スレッドのHTMLからレス(`Post`)一覧を抽出する。

### 一番の学び: Rustの正規表現にはlookbehindがない

Python版の`H_RESTORE_PATTERN`は`(?<!h)(ttps?://)`という後読み(lookbehind)を
使っていましたが、Rustの`regex`クレートは意図的にこれを**サポートしていません**。
「どんな入力でも実行時間が入力長に対して線形になることを保証する」という設計思想
(バックトラックしないエンジン)によるものです。

対処法として、マッチ位置を`find_iter`で取得し、直前の1文字を自分でチェックする形で
「後読み相当」を手動実装しました。

```rust
fn restore_h_prefix(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    for m in H_RESTORE_PATTERN.find_iter(text) {
        result.push_str(&text[last_end..m.start()]);
        let preceded_by_h = text[..m.start()].chars().next_back() == Some('h');
        if !preceded_by_h {
            result.push('h');
        }
        result.push_str(m.as_str());
        last_end = m.end();
    }
    result.push_str(&text[last_end..]);
    result
}
```

### その他の学び
- **HTMLパース**: `selectolax`(Python) → `scraper`(Rust、内部はhtml5ever)。
  CSSセレクタで辿る感覚はほぼ同じ。
- **`html_escape::decode_html_entities`**: Pythonの`html.unescape`相当。

---

## 第4章: `menu.py` / `threads.py` の移植

### 目標
板メニューとスレッド一覧の取得。ここで設計方針を1つ固めた:
**「ネットワークI/O」と「取得済みテキストの解析」をはっきり分ける。**

```rust
// I/O担当(async, 副作用あり)
async fn get_menu_from_json(fetcher: &Fetcher, url: &str) -> Result<Vec<Category>, MenuError> { /* ... */ }

// 解析担当(同期, 純粋関数) → cargo testで単体テストできる
pub fn parse_menu_json(text: &str) -> Result<Vec<Category>, MenuError> { /* ... */ }
```

このサンドボックスは5chに繋がらないという制約がありましたが、この分離のおかげで
**ロジック自体はちゃんとテストできる**状態を保てました。

### `Fetcher`を構造体に

```rust
pub struct Fetcher { user_agent: String }
impl Fetcher {
    pub fn new(user_agent: impl Into<String>) -> Self { /* ... */ }
    pub async fn fetch(&self, url: &str) -> Result<(Vec<u8>, String), FetchError> { /* ... */ }
}
```

Pythonの`class Fetcher: def __init__(self, user_agent)`と同じ発想。「設定を持つ
オブジェクト」を構造体+implで表現するのはRustの基本パターン。

### `HistoryStore`トレイトと、Rust 1.75の新機能

```rust
pub trait HistoryStore {
    async fn get_last_read(&self, board_url: &str, dat_file: &str) -> u32;
    async fn exists(&self, board_url: &str, dat_file: &str) -> bool;
    async fn add_new_thread(&self, title: &str, board_url: &str, dat_file: &str);
}
```

traitの中に直接`async fn`を書けるのは、実は2023年末リリースのRust 1.75で
安定化されたばかりの機能(async fn in traits, AFIT)。このサンドボックスの
Rustもちょうど1.75だったので、追加クレート(`async-trait`)なしで書けた。

### 学んだこと
- **`except Exception: pass` → `if let Ok(...)`**: 「JSON失敗したら黙ってHTMLへ」は、
  `Result`を`if let Ok`でパターンマッチするだけで同じ効果になる。
- **`&mut Board`で書き換える権利を明示**: リダイレクトでboard.urlが変わる処理は、
  引数を`&mut`にすることで「この関数は本当に変更するのか」をコンパイラに伝える。

---

## 第5章: `search.py` / `nextthread.py` の移植

### 目標
全板横断検索と、次スレ自動検出。ここでコア機能(fetch/models/errors/parse/menu/
threads/search/nextthread)の移植が一通り完了。

### 収穫: 専用クレートを増やさずに済んだ

Pythonの`urllib.parse.quote_plus`相当は、既に依存に入っていた`url`クレートの
`form_urlencoded`でそのまま代用できた。

```rust
pub fn quote_plus(keyword: &str) -> String {
    url::form_urlencoded::byte_serialize(keyword.as_bytes()).collect()
}
```

### テストがバグを拾った実例

`nextthread.rs`の`TITLE_SUFFIX_PATTERN`は`- 5ch.net`のような**ハイフン/パイプ区切り**
だけを対象にしていて、`@5ch.net`には反応しない(Python版も同じ)。最初のテストで
そこを誤解したサンプルデータを書いてしまい、`cargo test`が一発で指摘してくれた。
型が通ってもロジックのバグはテストが拾う、という実例。

---

## 第6章: `history.py` の移植

### 目標
閲覧履歴のJSON永続化。ここで初めて`HistoryStore`トレイトの「本物の実装」が登場。

```rust
pub struct Manager {
    file_path: PathBuf,
    data: tokio::sync::Mutex<HashMap<String, Entry>>,
}
```

Python版もファイルI/O自体は同期(ブロッキング)で、`asyncio.Lock`は複数タスクが
同時に読み書きしないための排他制御でしかなかった。Rust版もまったく同じ設計思想で、
`tokio::sync::Mutex<HashMap<...>>`にして`std::fs`をそのまま使っている。

### 正規表現なしで済ませる判断

`normalize_url`のPython版は`re.sub`3連発だったが、やっていることは「先頭/末尾の
固定文字列を削る」だけなので`strip_prefix`/`strip_suffix`で十分だった。

```rust
fn normalize_url(raw_url: &str) -> String {
    let mut u = raw_url;
    if let Some(rest) = u.strip_prefix("https://") { u = rest; }
    else if let Some(rest) = u.strip_prefix("http://") { u = rest; }
    if let Some(rest) = u.strip_prefix("www.") { u = rest; }
    let u = u.strip_suffix('/').unwrap_or(u);
    u.replace("2ch.net", "5ch.io").replace("5ch.net", "5ch.io")
}
```

### 学んだこと
- **ジェネリクスの威力**: `threads::get_threads`や`nextthread::detect_and_add_next_thread`
  は「`HistoryStore`を実装した何か」としか要求していないので、ダミー実装(NullHistory)
  から本物の`Manager`に差し替えても、呼び出し側のコードは一切変更不要だった。

---

## 第7章: `export.py` の移植

### 目標
アーカイブ用のJSON/Markdown出力。日時処理、Cloudflareのメール難読化解除、
JSON-LDパースなど盛りだくさんの章。

```rust
// JSTのRFC3339形式に変換
fn jst() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).unwrap()
}
```

`chrono`クレートの`FixedOffset`は、Pythonの`timezone(timedelta(hours=9))`に
そのまま対応する。`strftime`系のフォーマット文字列もほぼ共通言語だった。

### モジュール間の再利用が効いた

`export.py`はPython版でも`parse.py`の関数を輸入していたが、Rust版も
`pub`にして同じ関数(`restore_h_prefix`、`inner_html`、`post_nodes`など)を
そのまま再利用できた。第3章で「lookbehind相当を手動実装」した苦労がここで報われた。

### `serde_json::Value`という選択肢

JSON-LDのパースには、事前に型を決め打ちする`struct + Deserialize`ではなく、
動的な`serde_json::Value`(Pythonの`dict`感覚で`.get()`ライクに掘れる)を使った。
**構造が決まっているデータにはstruct、構造が緩い/不確実なデータにはValue**、
という使い分け。

---

## 第8章: `browser.py`(Facade)の移植

### 目標
これまでの全モジュールを束ねる調整役。「ジェネリクスの伏線回収」の章。

```rust
pub struct Browser<H: HistoryStore> {
    history: H,
    cache_expire: Duration,
    fetcher: Fetcher,
    menu_cache: Mutex<Option<Vec<Category>>>,
    thread_cache: Mutex<HashMap<String, CachedThreads>>,
}
```

`Browser<H: HistoryStore>`のおかげで、本番用の`history::Manager`でも使い捨て
コマンド用の`history::NullHistory`でも、呼び出し側のコードを一切変えずに差し替え可能。
Pythonの`Protocol`によるダックタイピングと同じ効果を、コンパイル時の型で保証しながら
実現できた。

### 関数の再利用、その2

Python版の`browser.py`は`nextthread_mod.TITLE_TAG_PATTERN.search(...)`のように
他モジュールの正規表現を直接呼び出していたが、Rust版では第5章で「タイトル抽出」を
`nextthread::extract_title()`という1つの公開関数にまとめておいたので、ここでは
1行呼ぶだけで済んだ。**モジュールの責務をはっきり切った設計のご利益**が、章を
またいで実感できた。

---

## 第9章: `config.py` / `lock.py` の移植

### 目標
設定管理と多重起動防止。一番「OSを直接触る」章。

| やりたいこと | Python | Rust |
|---|---|---|
| ホームディレクトリ解決 | `Path.home()` | `dirs::home_dir()` |
| ファイルロック | `fcntl.flock` | `fs2::FileExt::lock_exclusive` |
| シグナル送信 | `os.kill` / `signal` | `nix::sys::signal::kill` |

### 所有権とロック解放の対応関係

Pythonの「ファイルオブジェクトがcloseかGCされるとロックが解ける」という暗黙の挙動は、
Rustでは`File`が**スコープを抜けてdropされた瞬間**に確定的に起きる。GCのタイミング
任せではなく、コンパイラが所有権の終わりを追跡して即座に解放してくれるので、
「ロックがいつ解けるか」がPythonよりむしろ読みやすいコードになった。

---

## 第10章: `webhook.py` / `cli/main.py` の移植

### 目標
Webhook送信と、非対話CLIコマンド(search/read/export/export-batch/webhook-send)。
「伏線回収、その2」の章。

### `X5chError`の`#[from]`が最大限に効いた

`search`/`read`/`export`はそれぞれ`SearchError`/`BrowserError`/`ThreadsError`と
別々の型でエラーを返してくるが、`X5chError::from(e)`一発でどれも同じ型に集約できた。

```rust
fn classify_error_type(err: &X5chError) -> &'static str {
    match err {
        X5chError::Fetch(_) => "network",
        X5chError::Browser(be) if be.message == THREAD_GONE_MESSAGE => "thread_gone",
        _ => "other",
    }
}
```

Python版の`except Exception as ex: return _classify_error_type(ex)`と同じ効果を、
型を保ったまま実現できた。

### 引数パースはあえて手動ループ

`clap`のような専用CLIパーサクレートを増やさず、Python版の
`while i < len(args)`ループをそのまま翻訳した。依存を増やさない選択も設計判断の一つ。

---

## 第11章: TUI(`tui/app.py` / `tui/screens.py`)の移植

### 目標
対話TUI。メインメニュー→板一覧→スレ一覧→スレ読みの一本道を`ratatui`で実装。
(検索モーダル・キュー/履歴管理画面・Discord連携は範囲外)

### 状態遷移ロジックを描画・I/Oから分離

このサンドボックスには対話端末(TTY)が無く、実際にキー操作を試すことはできない。
そこで「今どのキーが押されたらどう状態が変わるか」を、描画やI/Oから完全に切り離した
純粋関数として実装した。

```rust
pub fn move_selection(screen: &mut Screen, delta: isize) { /* ... */ }
pub fn scroll_pager(screen: &mut Screen, delta: isize) { /* ... */ }
pub fn activate_current(screen: &Screen) -> Action { /* ... */ }
```

`Action`列挙型は「Enterが押されたら次に何をすべきか」を表現するだけで、実際の
fetch等のI/Oはしない。おかげでターミナルが無い環境でも画面遷移ロジックそのものは
`cargo test`で検証できた(ここまでの章で繰り返してきた「I/Oとロジックの分離」パターンの、
一番効果を発揮した章)。

### `Arc<T>`へのblanket impl

TUIでは「`Browser`の中」と「メインメニューの★最近読んだスレッド一覧」の両方が
同じ履歴ストアを見る必要があるが、`Manager`自体は複製できない(`Mutex`を持つため)。

```rust
impl<T: HistoryStore + Send + Sync> HistoryStore for std::sync::Arc<T> {
    async fn get_last_read(&self, board_url: &str, dat_file: &str) -> u32 {
        (**self).get_last_read(board_url, dat_file).await
    }
    // ...
}
```

`Arc<Manager>`(所有権を共有するポインタ)で包み、そのArc自体にもHistoryStoreを
実装することで、`Browser<Arc<Manager>>`が自然に書けるようになった。

---

## 第12章: `discord.py` の移植

### 目標
Discord Bot API連携。Botトークンでチャンネル内にスレッドを作成し、そのスレッドへ
メッセージを送る(webhook.rsの単純なURL直POSTとは独立した、より重厚な送信経路)。

### `Fetcher`を後から一般化する

第10章では`post_json`(ヘッダー固定)しか無かったが、今回`post_json_with_headers`
として一般化した。

```rust
pub async fn post_json(&self, url: &str, body: String) -> Result<(u16, String), FetchError> {
    self.post_json_with_headers(url, body, Vec::new()).await
}

pub async fn post_json_with_headers(
    &self,
    url: &str,
    body: String,
    extra_headers: Vec<(String, String)>,
) -> Result<(u16, String), FetchError> {
    // ureqのリクエストビルダーに、渡されたヘッダーを1つずつセットしてから送信
}
```

既存の`post_json`はその薄いラッパーに書き換えるだけで、呼び出し元(webhook.rs)には
一切影響がなかった。所有権や型の整合性を保ったまま「後から一般化する」リファクタが、
コンパイラに守られて安全にできることを体感できた。

### `enabled()`ガードと関数の再利用

```rust
pub fn enabled(&self) -> bool {
    if self.token.is_empty() || self.token.contains("YOUR_BOT_TOKEN") {
        return false;
    }
    !self.channel_id.is_empty()
}
```

トークン未設定/プレースホルダのままなら即`false`。`send_message`はこのガードで
安全に早期returnする。またメッセージの2000字超え分割には、第10章でwebhook.rs用に
作った`split_by_runes`をそのまま再利用できた——「n文字ずつ分割する」という操作は
Webhook経由でもBot API経由でも同じなので、モジュールの責務を切っておいた恩恵が
ここでも効いた。

### 学んだこと
- Discord Bot API(`discord.com`)はサンドボックスの許可ドメイン外だったため、実通信の
  確認はできなかったが、`enabled()`のガードのおかげでロジック自体は安全に検証できた。

---

## 第13章: `transfer.py`(Discord転送Worker)の移植

### 目標
キューを監視し、順番にスレッドをDiscordへミラーするバックグラウンドワーカー。
全13章の中でもっとも並行処理の設計判断が要る章だった。

### `asyncio.Condition`をどう表現するか

Python版は「ロック+待機+通知」が一体になった`asyncio.Condition`を使っていたが、
Rustでは「状態を守る`Mutex`」と「待っている人を起こす`Notify`」を分けて持たせた。

```rust
struct Worker {
    state: Mutex<WorkerState>,
    notify: Notify,
    // ...
}
```

### `notify_one()`の"permit"が通知の取りこぼしを防ぐ

`wait_for_work()`が「キューを見て空振り→`notify.notified().await`」という流れを
踏む間に、別タスクが`enqueue()`で通知を送ってしまうと、素朴な実装では通知を逃して
永遠に待ち続けてしまう。`tokio::sync::Notify::notify_one()`は「待っている人がいなければ、
起こす権利を1つ貯めておく」という仕組みを持っているため、この競合を安全に回避できる。
待ち手が1人だけ(実行ループ自身)と分かっている場面での`notify_one()`は、Pythonの
`notify_all()`より狭い代わりに、こうした保証が付いてくる。

```rust
async fn wait_for_work(&self) -> Option<Task> {
    loop {
        {
            let mut s = self.state.lock().await;
            if s.shutdown && s.queue.is_empty() { return None; }
            if !s.queue.is_empty() && !s.suspended {
                let task = s.queue.remove(0);
                s.current_task = Some(task.clone());
                return Some(task);
            }
        }
        self.notify.notified().await; // ここでの取りこぼしをpermitが防ぐ
    }
}
```

`wait_until_done()`だけは、正確な条件変数ではなく短い間隔のポーリングに割り切った
(複数種類の待ち手が同じ`Notify`を共有すると、上記の保証が崩れるため)。

### `Arc<Self>`でバックグラウンドタスクに自分自身を持たせる

```rust
pub fn start(self: &Arc<Self>) {
    let worker = Arc::clone(self);
    *self.run_handle.lock().unwrap() = Some(tokio::spawn(async move { worker.run_loop().await }));
}
```

Pythonの`asyncio.create_task(self._run())`が暗黙にやっている「タスクが自分自身(self)を
生かし続ける」ことを、Rustでは所有権の共有(`Arc`)として明示的に書く必要がある。

### 型情報を失った箇所は正直に書く

`BrowserError`はエラー理由を文字列に埋め込む設計にしていたため(第8章)、Worker側で
「ネットワークエラーかどうか」をメッセージのプレフィックスで判定する簡略化をした。
Python版の`isinstance(ex, NetworkFetchError)`ほど厳密ではないが、トレードオフを
コードコメントで明記している。

### 学んだこと
- 並行処理のコードは「動いているように見える」だけでは不十分で、通知の取りこぼしのような
  競合は静かに壊れる。`Notify`のpermit機構のようなライブラリ側の保証を正しく理解して
  使うことが、Pythonの`asyncio.Condition`を素朴に置き換えるより重要だった。
- デモを書く際、実ネットワークに繋がらない環境で「失敗→再試行」ループを本番の遅延
  (30秒)のまま起動すると、デモ自体がハングしかけた。テスト/デモ用に遅延を注入できる
  設計(`with_delays`コンストラクタ)にしておいたおかげで安全に確認できた。

---

## 第14章: TUIの検索モーダル・キュー管理・履歴管理画面

### 目標
第11章で一本道(メインメニュー→板一覧→スレ一覧→スレ読み)だけに絞っていたTUIに、
検索モーダル・転送待機列(キュー)管理・閲覧履歴管理の3画面を追加する。

### `Screen::Search`は「リスト」ではなく「入力バッファ」

他の画面はどれも`selected: usize`で1項目を指すだけだったが、検索画面は文字列を
1文字ずつ組み立てる必要がある。

```rust
pub enum Screen {
    // ...
    Search { input: String },
    QueueManage { tasks: Vec<transfer::Task>, selected: usize },
    HistoryManage { items: Vec<RecentThread>, selected: usize },
}
```

`run()`のキー処理ループの先頭で「今Search画面なら、通常のナビゲーションキー
(j/k/Enter/bなど)ではなく文字入力として扱う」という分岐を先に入れることで、
既存の画面遷移ロジックを汚さずに済んだ。

```rust
if let Screen::Search { input } = state.top_mut() {
    match key.code {
        KeyCode::Esc => { state.pop(); }
        KeyCode::Backspace => { input.pop(); }
        KeyCode::Char(c) => { input.push(c); }
        KeyCode::Enter => { /* browser.search_global(&keyword) を実行 */ }
        _ => {}
    }
    continue; // 通常のナビゲーション処理には進まない
}
```

Python版の`SearchModal`は`ModalScreen`という独立した仕組みだったが、Rust版では
「入力中は割り込み処理する」という形で同じ効果を得ている。

### 既存の部品がそのまま使えた

`QueueManage`/`HistoryManage`画面の中身は、新しいロジックをほとんど書く必要が
なかった。`Worker::queue_list()`/`delete_at()`(第13章)、`Manager::get_recent_threads()`/
`delete_thread()`(第6章)を呼び出すだけで、TUIは「結果を画面に映すだけの薄い層」に
なっている。「Enterで今の選択を処理する」という`Action`列挙型の抽象化(第11章)も、
画面の種類が増えて破綻することなく、`DeleteQueueAt`/`DeleteHistoryEntry`を
足すだけで自然に合流できた。

```rust
pub enum Action {
    // ...
    DeleteQueueAt(usize),
    DeleteHistoryEntry { board_url: String, dat_file: String },
}
```

### 気づいて直した設計ミス: `Browser`を2つ作っていた

実装の途中で、TUIのナビゲーション用と`Worker`用に別々の`Browser`インスタンスを
作ってしまっていることに気づいた。Python版は

```python
self.worker = Worker(browser, self.discord, history, config.queue_file)
```

のように、アプリ本体(TUI)と`Worker`で**同じ`browser`オブジェクト**を共有しており、
メニュー/スレ一覧キャッシュも両者で共通になる設計だった。Rust版も
`Arc<Browser<Arc<Manager>>>`を1つだけ作り、`Worker`とTUIの両方に`.clone()`
(参照カウントを増やすだけの軽い操作)で渡す形に直した。

```rust
let browser = Arc::new(Browser::new(user_agent, history.clone(), cache_expire));
let worker = Arc::new(Worker::new(browser.clone(), discord_mgr.clone(), history.clone(), queue_file));
tui::run(browser, history, discord_mgr, worker).await
```

これは第1章のrustls→native-tls切り替えや、第5章でテストが拾ったバグと同じ
「移植の途中でPython版との食い違いに気づいて直す」パターン。1対1翻訳では
見落としがちな「どのオブジェクトを誰と共有しているか」という設計意図は、
コードを読むだけでなく実際に動かして初めて気づくことも多い。

### 学んだこと
- **キュー管理画面を試すには、キューに何か入れる手段が要る**: ThreadList画面に
  `m`キー(Discord転送予約)を足して、`Worker::enqueue()`を実際に呼べるようにした。
  画面単体ではなく「その画面が意味を持つために必要な導線」まで含めて考える必要があった。
- **Arcの`clone()`はコストが低い**: 「同じものを複数箇所で使い回したい」という
  Pythonなら当たり前の要求が、Rustでは`Arc`という型を使うという判断そのものが
  「これは複数の所有者を持つ」という設計意図の表明になる。

---

## まとめ

13章を通して、次のRustの考え方を実際に手を動かしながら学んだ:

- **所有権とムーブ**: 暗黙のコピーで意図せずメモリを共有してバグる、が構造的に
  起きにくい。`Clone`で複製、`&`/`&mut`で借用、`move`で所有権譲渡。
- **`Result`と`?`**: 例外機構なしで、成功/失敗を型として扱う。
- **継承なしの多態性**: `enum` + `#[from]`(エラー階層)、trait(ダックタイピングの
  型付き版)、ジェネリクス(`<H: HistoryStore>`)。
- **I/Oとロジックの分離**: 純粋関数に分けておくと、ネットワークに繋がらない環境でも
  `cargo test`だけでロジックを検証できる。この方針は全14章で一貫して効果を発揮した。
- **regexクレートの割り切り**: lookbehind非対応など、Pythonの`re`より表現力は
  落ちるが「必ず線形時間で終わる」保証と引き換え。素の文字列操作で済む場面も多い。
- **`Mutex` + `Notify`で条件変数を組み立てる**: Pythonの`asyncio.Condition`のような
  「ロック+待機+通知」が一体になった機能は、Rustでは責務ごとに部品を組み合わせて作る。
  `notify_one()`のpermit機構のような、ライブラリが保証してくれる性質を正しく理解する
  ことが、素朴な移植より重要だった。

### 未着手の範囲

- ThreadList画面のWebhook個別送信(`w`)・Export(`e`/`E`)・履歴削除(`H`)・番号指定コマンド
- Discord Bot APIの実通信確認(`discord.com`がサンドボックスの許可ドメイン外だったため、
  `enabled()`ガード経由の安全な早期returnのみ確認済み)

これらは次の機会に。ここまでで、`5chterm_py`の全モジュールの移植ロジックは一通り
完了している(ThreadList画面の一部コマンドを除く)。