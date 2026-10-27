# Changelog

このプロジェクトの主な変更を記録する。記録対象はオリジナル
（[`lambdalisue/cclens`](https://github.com/lambdalisue/cclens) `v0.4.0`）
からの差分のみとする。オリジナル側の変更履歴は含めない。

形式は [Keep a Changelog](https://keepachangelog.com/ja/1.1.0/) に従い、
バージョンは [Semantic Versioning](https://semver.org/lang/ja/) に従う。

## [Unreleased]

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
