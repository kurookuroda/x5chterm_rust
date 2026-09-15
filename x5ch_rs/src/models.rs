//! コアデータモデル。Python版 models.py に対応。
//!
//! Pythonの @dataclass はフィールド定義から自動でコンストラクタ(__init__)や
//! __repr__、比較演算子などを生成してくれる。Rustにはdataclassという専用機能は
//! ないが、struct + #[derive(...)] の組み合わせでほぼ同じことができる。
//! - Debug  … Pythonの __repr__ 相当(println!("{:?}", x) で表示できる)
//! - Clone  … Pythonでは変数代入は基本「参照のコピー」だが、Rustは既定で
//!            所有権が「移動」する。同じ値を複製したい時は明示的に .clone() する。

/// 5chの1つの板。
#[derive(Debug, Clone)]
pub struct Board {
    pub title: String,
    pub url: String,
}

/// メニュー上の1カテゴリと、そこに属する板一覧。
///
/// Pythonの `boards: list[Board] = field(default_factory=list)` は
/// 「呼び出しごとに新しい空リストを作る」ための仕掛けだったが、Rustは
/// ミュータブルなデフォルト値の共有問題(Pythonの`= []`落とし穴)自体が
/// そもそも起きない言語設計なので、素直に `Vec::new()` を渡すだけでよい。
#[derive(Debug, Clone)]
pub struct Category {
    pub title: String,
    pub boards: Vec<Board>,
}

impl Category {
    /// title必須・boardsは空リストから始める、Python版のデフォルト引数を再現するコンストラクタ。
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            boards: Vec::new(),
        }
    }
}

/// 1スレッドの識別情報と、閲覧に伴って更新される状態。
///
/// dat_fileだけ必須、他は初期値ありというPython版の非対称なデフォルトは、
/// Rustではstruct全体への#[derive(Default)]では表現しにくい(全フィールドに
/// 同列にデフォルトを要求してしまう)ため、専用のnew()コンストラクタで表現する。
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
    pub fn new(dat_file: impl Into<String>) -> Self {
        Self {
            dat_file: dat_file.into(),
            title: String::new(),
            count: 0,
            ikioi: 0.0,
            board_url: String::new(),
            last_read: 0,
            url: String::new(),
        }
    }

    /// Python版の @property has_new に対応。
    /// Rustにはプロパティ構文がないので、素直な通常メソッドとして書く。
    pub fn has_new(&self) -> bool {
        self.count > self.last_read
    }
}

/// 1レス(TUI/search/read等の軽量用途向け)。全フィールド必須。
#[derive(Debug, Clone)]
pub struct Post {
    pub num: u32,
    pub name: String,
    pub date: String,
    pub message: String,
}
