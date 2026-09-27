# archivefs

`archivefs` は、ZIP、TAR、7z、RARなどのアーカイブ形式を統一した仮想ファイルシステムとして扱うためのRustワークスペースです。

## クレート

- `vfs-archive`: アーカイブ形式に依存しない `ArchiveVfs`、`ArchiveEntry`、`ReadSeek` を提供します。
- `lazy-archive`: `open_archive`による形式自動判定と、`ZipArchiveVfs`、`TarGzArchiveVfs`、`TarArchiveVfs`、`SevenZArchiveVfs`、`RarArchiveVfs`、解凍済みエントリを保持するスレッドセーフなLRUキャッシュを提供します。
- `archivefs-bench`: アーカイブAPIの利用サンプルと、端末向けの性能計測CLIを提供します。

## CLIサンプルと計測

形式判定とエントリ情報を表示します。

```sh
rtk cargo run -p archivefs-bench -- inspect archive.zip
```

アーカイブopenと一覧取得を分けて計測します。

```sh
rtk cargo run -p archivefs-bench -- bench archive.zip --warmup 2 --iterations 10 open
```

順次読み込み、再現可能なランダムアクセス、画像候補エントリの読み込みも計測できます。

```sh
rtk cargo run -p archivefs-bench -- bench archive.tar.zst scan --max-entries 100 --chunk-size 65536
rtk cargo run -p archivefs-bench -- bench archive.7z random --seed 42 --max-entries 10 --requests 200 --read-size 4096 --cache-state cold
rtk cargo run -p archivefs-bench -- bench archive.zip thumbnail-read --max-images 20
```

結果は端末にレイテンシ統計（min/mean/median/p95/p99/max）、throughput、読み込みbyte数、ASCIIヒストグラムとして表示します。`random --max-entries N`はランダムアクセス候補に使う異なる通常ファイルを最大N件に制限します。候補はseedで決まる部分集合ですが、各要求はその候補から重複ありで選択します。`--requests`は1反復あたりの要求回数、`--iterations`は本計測反復数です。OSのページキャッシュは制御しないため、実行環境やストレージ状態が異なる結果は直接比較できない場合があります。`thumbnail-read`は画像データの取得までを計測し、画像デコードやリサイズは行いません。

`bench scan`はwarmupと全計測iterationで同じVFSを保持し、後続走査でarchivefsのLRU cacheを再利用します。`bench random --cache-state cold`はwarmupを独立VFSで実行し、本計測開始時に空のcacheを持つVFSを作って要求・iteration間で共有します。`--cache-state warm`は本計測前に要求対象をopenしてcacheを温めます。これらはarchivefs内部cacheの状態を表し、OSページキャッシュは制御しません。

`EntryCache`の既定値は最大256エントリ、メモリ保持閾値1 MiB、総cache byte上限64 MiBです。件数またはbyte上限を超えるとLRU順に追い出され、単一entryが総byte上限を超える場合はcacheされません。`open_archive_with_cache()`と`EntryCache::with_limits()`で用途に合わせて設定できます。

```rust
use std::num::NonZeroUsize;
use lazy_archive::{EntryCache, open_archive_with_cache};

let cache = EntryCache::with_limits(
	NonZeroUsize::new(512).expect("capacity must be non-zero"),
	1024 * 1024,
	256 * 1024 * 1024,
);
let archive = open_archive_with_cache("archive.7z", cache)?;
```

ZIP entryは直接streamするため、このentry cacheには格納されません。

`bench scan`では指定したchunk sizeに加え、読み込んだ通常ファイル数、`Read`呼び出し数（EOF確認を含む）、データを返したRead回数、成功Readあたりの平均byte数も表示します。chunk sizeは各`Read`呼び出しで要求する最大バッファ長であり、圧縮デコーダ内部のブロックサイズを直接変更する設定ではありません。サイズを大きくすると呼び出し回数が減る傾向がありますが、実際の返却サイズはReaderや形式によって異なるため、throughputと平均byte/readを合わせて比較してください。

計測中はstderrに進捗spinner/barを表示し、現在のwarmup/iterationと処理中のファイルまたはrandom要求を示します。結果や統計はstdoutへ出力します。非TTY出力では進捗表示が抑制されることがあります。

対応形式や手元のアーカイブを、一覧・読み込み・seekでスモーク確認できます。

```sh
rtk cargo run -p archivefs-bench -- verify archive.rar
ARCHIVEFS_TEST_ARCHIVE=/path/to/archive.7z rtk cargo test -p archivefs-bench --test user_archive -- --ignored --nocapture
```

## 開発

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

実装は、アーカイブを開いた時点で全ファイルを展開せず、必要なエントリを開くタイミングで読み込みます。ZIPとTAR系形式は解凍元からチャンク単位で`EntryCache`へ直接書き込み、小さいエントリはメモリ、大きいエントリは一時ファイルへ保存します。TAR系は非圧縮、GZIP、BZ2、XZ、ZSTDに対応しています。大きいエントリの読み込みには`memmap2`を使用し、マップ済みデータを`Read + Seek`で提供します。キャッシュはエントリ数と総バイト数の両方で制限され、LRUで古いエントリを追い出します。7zは`for_each_entries`でソリッドブロックを対象エントリまで逐次解凍し、RARは純Rustの`rars`でRAR 1.3からRAR5系を読み込みます。
