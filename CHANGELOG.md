# Changelog

このプロジェクトの主な変更を記録する。記録対象はオリジナル
（[`lambdalisue/cclens`](https://github.com/lambdalisue/cclens) `v0.4.0`）
からの差分のみとする。オリジナル側の変更履歴は含めない。

形式は [Keep a Changelog](https://keepachangelog.com/ja/1.1.0/) に従い、
バージョンは [Semantic Versioning](https://semver.org/lang/ja/) に従う。

## [Unreleased]

### Changed

- 起動時解決（設定→フラグ→既定値→ハーネス→ストアパス→旧ストア移行→
  open＋guard）を `Session` モジュールに集約（PR #13）。全コマンドの配線が
  1つの seam になり、不正フラグのエラー表示も従来どおりになった。
- store の ingest を1トランザクションの `IngestBatch` + `Store::ingest` に
  集約し、ストアの open 経路を `Store::open(path, OpenMode)` に統一。
  列オーバーロードの意味の定義を `EVENT_ENCODING` の単一テーブルへ移した。
- `analyze` のハーネス別 dispatch を `Harness::analyze` に集約。入力ルート
  解決と analyzer バージョン鍵を各 adapter へ移し、`cli.rs` は
  dispatch を持たなくなった（4つ目のハーネスは adapter 追加だけで通る）。
- briefing 組立を純粋な `core::findings` へ移設し、上限値（例示数・
  thrash 行数・heavy トークン閾値など）を名前付き定数にした。
- 設定カタログ走査を `catalog_surfaces(harness, roots)` 単一 IF に集約し、
  サイト定義を `Site` のデータ行にした（新 kind は行を足すだけ）。
- `optimize` の配信を `deliver` seam（`DeliveryMode` / `AdvisorLaunch`）に
  切出し、到達不能だった `Option` と `HarnessCopy` の手組みを削除。
- 上記の構造変更は出力が従来と同一: 旧 `821bf1e` バイナリとのE2E出力
  269行がバイト同一、`cargo test` 391件 green。PBI 01-06 は
  `pbi/archived/` にアーカイブ。

## [0.5.1] - 2026-10-29

### Added

- 設定ファイル（`~/.config/saniwalens/config.toml`）に対応。
  既定ハーネス・既定ストアと名前付きプロファイル（`--profile` で選択）
  を持てる。相対 `db` は config ファイルのあるディレクトリ基準で解決する。

### Changed

- `saniwalens --version` がビルド元の版を名乗る（git タグ由来。ソース
  ビルドの `0.0.0` 表示を解消）。
- `keywords` の `claude` を `coding-agent` に（マルチハーネスの位置づけに合わせる）。

### Fixed

- トークン無しでも CI がグリーンになるよう cachix 手順を条件化。
  リリース側の nix ジョブも同型でスキップする。

## [0.5.0] - 2026-10-28

### Added

- `--harness claude | opencode | kilocode` によるマルチハーネス対応。
  ハーネスごとに独立した SQLite ストアを持ち、分析結果が混ざらない。
- Kilo Code 対応：エディタ `globalStorage` のタスクファイル読み取り、
  トークン行の結合、対話的な `optimize`（`kilo --prompt` セッション起動）。
- opencode 対応：opencode ストアの読み取り、設定カタログ、
  headless / interactive の `optimize` 経路。
- 設定カタログ：commands、agents、skills、`AGENTS.md`、`.kilorules`、
  `kilo.json` の `instructions[]` を計測し、`inventory` / `waste` で
  実際の使用状況と突き合わせる。
- 旧ストアの自動移行：初回起動時に `cclens/` または `agentlens/` を
  `saniwalens/` へ移動し、ハーネス別ファイルも改名する。削除も上書きも
  しない。明示的な `--db` は対象外。
- crates.io への trusted publishing 設定。

### Changed

- ツール名を `saniwalens` に改名。`agentlens` は crates.io で同領域の
  別ツールが取得済みだったため、再改名した。
- 製品文面を coding-agent 横断に汎用化（`AGENTS.md`、README 冒頭と対象範囲、
  クレート文書、flake の説明、アーキテクチャ仕様の一文）。
- GitHub リポジトリを `armaniacs/saniwalens` に、Homebrew tap を
  `armaniacs/homebrew-saniwalens` に。
- `LICENSE` に fork の著作権表示を追加（元の表示は維持）。

### Removed

- Claude Code プラグイン機構を削除（marketplace manifest、
  `plugins/cclens` パック、README の節）。saniwalens はマルチハーネスで、
  Claude Code は主戦場ではない。Claude 専用の用途にはオリジナルの
  `cclens` を案内する。

### Fixed

- ツール失敗の抜粋が JSON エンベロープ全体のまま分類されていた問題を修正。
  実際に運ばれた読めるメッセージで分類するよう unwrapping する。
- work イベントの `started_at` が空のままだった問題を修正。
- 未知のツール名が path-not-found に誤分類されていた問題を修正。
