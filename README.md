# archivefs

archivefsは、ZIP、TAR、7z、RARなどのアーカイブを、Rustから共通の仮想ファイルシステムとして扱うライブラリです。アーカイブ内のファイル一覧を取得し、必要なファイルを遅延して読み込めます。

## 特徴

- 複数のアーカイブ形式を`ArchiveVfs`インターフェースで統一
- ファイル一覧とメタデータを取得し、個別のファイルを`Read + Seek`で読み込み
- 必要なエントリをオンデマンドで読み込むため、アーカイブ全体の一括展開が不要
- 展開データ用の有界LRUキャッシュ。メモリと一時ファイルを使い分け
- Linuxを主な対象とするRustワークスペース

## 対応形式

ZIP、TAR、TAR.GZ、TAR.BZ2、TAR.XZ、TAR.ZST、7z、RARに対応します。TAR圧縮形式はファイル署名と拡張子を使って判定します。

## クレート

- `vfs-archive`: 形式共通の`ArchiveVfs`、`ArchiveEntry`、`ReadSeek`を提供
- `lazy-archive`: 形式判定、共通オープナー、各形式の遅延読み込み実装を提供
- `archivefs-bench`: アーカイブの確認と性能計測を行うCLI

## クイックスタート

`Cargo.toml`に依存関係を追加します。

```toml
[dependencies]
lazy-archive = "0.1"
vfs-archive = "0.1"
```

```rust
use std::io::Read;
use std::path::Path;

use lazy_archive::open_archive;
use vfs_archive::ArchiveVfs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let archive = open_archive("archive.zip")?;

	for entry in archive.entries()? {
		println!("{} ({} bytes)", entry.path().display(), entry.size());
	}

	let mut file = archive.open(Path::new("docs/readme.txt"))?;
	let mut contents = String::new();
	file.read_to_string(&mut contents)?;
	println!("{contents}");

	Ok(())
}
```

詳しいAPIの使い方、キャッシュ設定、エラー処理、CLIの利用方法は[ライブラリ利用マニュアル](doc/api-guide.md)を参照してください。

## CLI

```sh
cargo run -p archivefs-bench -- inspect archive.zip
cargo run -p archivefs-bench -- verify archive.rar
```

性能計測は`open`、`scan`、`random`、`thumbnail-read`の各シナリオを利用できます。オプションと結果の見方はマニュアルに記載しています。

## 開発

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```
