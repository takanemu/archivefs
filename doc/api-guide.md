# archivefs ライブラリ利用マニュアル

このマニュアルでは、archivefsのRust APIを使ってアーカイブの一覧を取得し、個別のファイルを読み込む方法を説明します。プロジェクトの概要は[README](../README.md)を参照してください。

## 依存関係

```toml
[dependencies]
lazy-archive = "0.1"
vfs-archive = "0.1"
```

`lazy-archive`はアーカイブの判定と読み込みを担当し、`vfs-archive`は形式に依存しない共通インターフェースを提供します。

## アーカイブを開いて一覧を取得する

通常は`open_archive`を使います。ファイル署名などから対応形式を判定し、形式共通の`ArchiveVfs`として返します。

```rust
use lazy_archive::open_archive;
use vfs_archive::ArchiveVfs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let archive = open_archive("archive.zip")?;

    for entry in archive.entries()? {
        let kind = if entry.is_directory() {
            "directory"
        } else {
            "file"
        };
        println!("{kind}: {} ({} bytes)", entry.path().display(), entry.size());
    }

    Ok(())
}
```

`entries()`は`ArchiveEntry`の一覧を返します。各エントリからアーカイブ内の相対パス、サイズ、ディレクトリかどうかを取得できます。ディレクトリのサイズは0です。

## ファイルの読み込みとシーク

`ArchiveVfs::open`にはエントリのパスを渡します。返されたreaderは`Read + Seek + Send + Sync`を実装した型消去済みのreaderです。

```rust
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use lazy_archive::open_archive;
use vfs_archive::ArchiveVfs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let archive = open_archive("archive.zip")?;
    let mut file = archive.open(Path::new("docs/readme.txt"))?;

    let mut prefix = [0; 32];
    let bytes_read = file.read(&mut prefix)?;
    println!("先頭部分: {}", String::from_utf8_lossy(&prefix[..bytes_read]));

    file.seek(SeekFrom::Start(0))?;
    file.seek(SeekFrom::Current(8))?;
    file.seek(SeekFrom::End(-4))?;

    Ok(())
}
```

必要であれば`read_to_end`や`read_to_string`で全内容を読み込めます。大きなファイルを扱う場合は、固定長バッファを使って必要な範囲だけ読む方法が適しています。

圧縮されたZIPエントリは、シーク先から読み直すために展開ストリームを再生成します。頻繁なランダムシークが必要なワークロードでは、このコストを考慮してください。

## 対応形式と判定

`detect_archive_kind`は形式を`ArchiveKind`として判定します。`open_archive`は同じ判定を行ったうえでVFSを構築します。

| 形式 | 判定方法 |
| --- | --- |
| ZIP、7z、RAR | ファイル署名 |
| TAR | `.tar`拡張子 |
| TAR.GZ | GZIP署名または`.tar.gz`、`.tgz`拡張子 |
| TAR.BZ2 | BZ2署名または`.tar.bz2`、`.tbz`、`.tbz2`拡張子 |
| TAR.XZ | XZ署名または`.tar.xz`、`.txz`拡張子 |
| TAR.ZST | Zstandard署名または`.tar.zst`、`.tar.zstd`拡張子 |

TAR系の署名が判定できない場合は拡張子が使われます。対応形式として判定できない場合、`open_archive`は`lazy_archive::Error::UnsupportedFormat`を返します。

## キャッシュの設定

TAR系とRARでは、読み込み対象のエントリをオンデマンドで展開し、`EntryCache`に保持します。既定値は最大256エントリ、メモリ保持閾値1 MiB、総キャッシュサイズ64 MiBです。

7zでは、solidブロック内のエントリを個別に繰り返し展開しないよう、最初のアクセス時に対象ブロック全体を一度展開し、エントリごとの範囲を記録してブロック単位でLRUキャッシュします。既定のブロックキャッシュ上限は256ブロックまたは256 MiBで、1 MiBを超える展開データは一時ファイルへ保存します。これは初回アクセス時にブロック全体を展開する代わりに、同じブロックへの後続アクセスを速くする方式です。キャッシュ追い出しや破棄で一時ファイルは削除されます。すでに開いているreaderは、読み込み終了までそのストレージを保持します。

ZIPエントリはアーカイブから直接読み込むため、`EntryCache`には格納されません。

```rust
use std::num::NonZeroUsize;

use lazy_archive::{EntryCache, open_archive_with_cache};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cache = EntryCache::with_limits(
        NonZeroUsize::new(512).expect("capacity must be non-zero"),
        1024 * 1024,
        256 * 1024 * 1024,
    );
    let archive = open_archive_with_cache("archive.7z", cache)?;

    println!("entries: {}", archive.entries()?.len());
    Ok(())
}
```

`EntryCache::with_limits(capacity, memory_threshold, max_bytes)`では、`memory_threshold`と`max_bytes`はバイト単位です。7zでは`capacity`がキャッシュするブロック数の上限、`max_bytes`が展開ブロックの合計サイズ上限として使われます。指定した上限を超えるブロックはそのアクセス中だけ保持し、LRUには登録しません。TAR系とRARでは、従来どおり`capacity`はエントリ数、`max_bytes`はエントリキャッシュの合計サイズです。`EntryCache::default`の7zブロックキャッシュ上限は256 MiBです。`EntryCache::new`と`EntryCache::with_memory_threshold`を使う場合、7zブロックの合計上限は`max_bytes`と同じ値になります。

## エラーの扱い

- アーカイブ形式を判定できない場合、`open_archive`は`lazy_archive::Error::UnsupportedFormat`を返します。
- ファイルが開けない、アーカイブが破損しているなどの問題は、`lazy_archive::Error::Io`または一覧取得・ファイル読み込み時の`std::io::Error`として通知されます。
- 存在しないエントリを開くと`std::io::ErrorKind::NotFound`になります。
- ディレクトリをファイルとして開くと`std::io::ErrorKind::InvalidInput`になります。

## 形式ごとの型を直接使う

共通オープナーではなく形式を固定したい場合は、`ZipArchiveVfs::open`、`TarArchiveVfs::open`、`TarGzArchiveVfs::open`、`SevenZArchiveVfs::open`、`RarArchiveVfs::open`などを使えます。いずれも`ArchiveVfs`を実装しており、一覧とファイルの操作方法は共通です。形式自動判定やキャッシュ設定を一か所で扱う場合は、`open_archive`または`open_archive_with_cache`を推奨します。

## CLIでアーカイブを確認する

```sh
cargo run -p archivefs-bench -- inspect archive.zip
cargo run -p archivefs-bench -- verify archive.rar
```

`inspect`は形式とエントリ情報を表示します。`verify`は一覧取得、ファイル読み込み、シークをスモークテストします。

## 性能計測CLI

```sh
cargo run -p archivefs-bench -- bench archive.zip --warmup 2 --iterations 10 open
cargo run -p archivefs-bench -- bench archive.tar.zst scan --max-entries 100 --chunk-size 65536
cargo run -p archivefs-bench -- bench archive.7z random --seed 42 --max-entries 10 --requests 200 --read-size 4096 --cache-state cold
cargo run -p archivefs-bench -- bench archive.zip --warmup 2 --iterations 10 thumbnail-read --max-images 20
```

- `open`: アーカイブのオープンとエントリ一覧取得を別々に計測
- `scan`: 通常ファイルを順次読み込み。`--chunk-size`は各`Read`呼び出しで要求する最大バッファ長
- `random`: エントリのopen、seek、小ブロックreadを計測。`--seed`で再現可能な要求列を作り、`--max-entries`で候補数を制限
- `thumbnail-read`: 画像拡張子のエントリからデータを読み込み。画像デコードやリサイズは行わない

`--warmup`は計測前の実行回数、`--iterations`は計測反復数です。`random`では`--requests`が各反復の要求数、`--read-size`が要求ごとの読み込み量です。`--cache-state cold|warm`はarchivefs内部キャッシュの状態を指定します。OSのページキャッシュやストレージ状態は制御しません。

`scan`はwarmupと全計測iterationで同じVFSを保持し、後続走査で内部キャッシュを再利用します。`random --cache-state cold`はwarmupを独立したVFSで実行し、本計測では空のキャッシュを持つVFSを要求・iteration間で共有します。`warm`は本計測前に要求対象をopenしてキャッシュを温めます。`random --max-entries N`は候補となる異なる通常ファイルの上限で、各要求では候補から重複ありで選択します。

結果には試行数、min/mean/median/p95/p99/max、throughput、読み込みbyte数、ASCIIヒストグラムが含まれます。`scan`では読み込んだ通常ファイル数、Read呼び出し数（EOF確認を含む）、データを返したRead回数、成功Readあたりの平均byte数も表示します。進捗はstderr、結果はstdoutに表示され、非TTYでは進捗表示が抑制されることがあります。異なるマシンやOSキャッシュ状態の測定値は単純比較できない場合があります。

手持ちのアーカイブを使う任意実行の統合スモークテスト:

```sh
ARCHIVEFS_TEST_ARCHIVE=/path/to/archive.7z cargo test -p archivefs-bench --test user_archive -- --ignored --nocapture
```