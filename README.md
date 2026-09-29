## 開発環境について

このチュートリアルはサンドボックス環境(Ubuntu、`apt`経由のRust 1.75.0)で進めました。
最新の`rustup`が使えない制約から、依存クレートのバージョンを何度か調整する場面が
ありましたが、通常`rustup`で最新版を入れている環境であれば、この種の調整はほぼ不要です。

## ビルドと起動

RustとCargoをインストールした環境で、リポジトリのルートから以下を実行します。
Ubuntuでは、OpenSSLの開発用パッケージも必要です。

```bash
sudo apt install build-essential pkg-config libssl-dev
cd x5ch_rs
cargo build --locked
```

ビルド後、対話画面(TUI)を起動するには次を実行します。

```bash
cargo run --locked -- tui
```

`cargo run --locked`のようにサブコマンドを省略すると、TUIではなく各機能のデモが実行されます。
ビルド済みの実行ファイルから起動する場合は、`x5ch_rs`ディレクトリで次のように実行できます。

```bash
./target/debug/x5ch_rs tui
```

TUIでは`j`/`k`で項目を移動し、`Enter`で選択、`q`で終了します。
