# rust-image-viewer

Rust と egui で作成したシンプルな Windows 用画像ビューアです。

## ダウンロード

ビルド済みの実行ファイルは以下からダウンロードできます。

- [rust-image-viewer.exe をダウンロード](https://github.com/NoGinger333/rust-image-viewer/releases/tag/v1.3.0)


※ ダウンロードした `.exe` ファイルをそのまま実行して使用できます。

## 主な機能

- 画像の閲覧・ページ切り替え（マウスホイール / 矢印キー）
- ズーム・パン移動（Ctrl + マウスホイール）
- サイドバーでのフォルダ内画像一覧表示
- 画像の回転・上下左右反転
- ダークモード / ライトモード切り替え
- 画像ファイルのドラッグ＆ドロップ対応

## 操作方法

| 操作 | キー / マウス |
| --- | --- |
| 前 / 次の画像 | `←` / `→` または マウスホイール |
| 拡大・縮小 | `Ctrl` + マウスホイール |
| リセット | `R` キー |
| ウィンドウフィット | `F` キー |
| ファイルを開く | `Ctrl` + `O` |

## ビルド方法

```bash
git clone https://github.com/NoGinger333/rust-image-viewer.git
cd rust-image-viewer
cargo build --release
```

`target/release/rust-image-viewer.exe` に実行ファイルが生成されます。
