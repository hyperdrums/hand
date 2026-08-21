# H.A.N.D

Windows 向けの軽量なクリップボード履歴・お気に入りランチャーです。

## 主な機能

- テキストとファイルのクリップボード履歴
- パスを表示する通常のお気に入り
- Windows のファイルアイコンと任意の表示名で登録するアイコンお気に入り
- ファイル、フォルダ、URL、VS Code ワークスペースの起動

## ビルド

Rust 1.97 以降をインストールした Windows 環境で実行します。

```powershell
cargo build --release
```

実行ファイルは `target\\release\\hand.exe` に生成されます。

## 開発用コマンド

```powershell
cargo fmt
cargo check
```

ログ UI は標準ビルドでは含まれません。必要な場合だけ次のように有効化します。

```powershell
cargo build --release --features log-ui
```
