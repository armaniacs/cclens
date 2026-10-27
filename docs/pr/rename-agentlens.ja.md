# この PR が追加するもの: ツール名は `saniwalens`、旧ストアは一緒に移動する

`cclens` はツールが Claude Code しか理解していなかった頃の名前 — Claude Code のレンズ
です。マルチハーネス対応によってこの名前は二重に誤りになりました:ツールは opencode と
kilocode のデータも読むようになり、crates.io では `cclens` という名前がすでに取得済み
です。この PR は名前が効くすべてを **`saniwalens`** に改名します — クレートとバイナリ、
CLI コマンド、ハーネス別のストアファイル、テンポラリファイルの prefix、docs。ただし旧
ディレクトリだけは放置せず、移動させます。

## ストアの移行

ストアはパース元のトランスクリプトより長生きします(`storage.md`):Claude Code は独自の
保持スケジュールでトランスクリプトを削除し、db は抽出済みの行を保持し続けます。バイナ
リの見る場所だけを変えるリネームは、その履歴を黙って孤児にします — 新しいバイナリは新
しいストアを始め、旧ストアは読まれないまま横に置かれます。

そこで新名での最初の open が、リネーム前のディレクトリ(`~/.local/state/cclens/`)を新し
い場所へ移動し、ハーネス別のファイルも改名します(`cclens.db` → `saniwalens.db`、
`cclens-opencode.db`、`cclens-kilocode.db`):

- **削除しない** — 移動であって掃除ではない。履歴はリネームを生き延びます。
- **上書きしない** — 新名の下にすでにいるストアはそのまま、旧ストアは場所を空けるため
  に取り除かれるのではなく、横に生き残ります。
- **明示的な `--db` はそのまま** — ユーザー自身が選んだパスにリネームを適用するのはこ
  ちらの役目ではありません。

## 移動したもの

- `Cargo.toml`:パッケージ名、リポジトリ(fork が正のホームになりました)、説明(ツールは
  エージェント横断)、keywords。
- `src/cli.rs`:clap のコマンド名、デフォルトのストアディレクトリ
  (`~/.local/state/saniwalens/`)、テンポラリファイルの prefix、テスト込みの旧ストア移動。
- `src/adapter/harness.rs`:ハーネス別の db ファイル名。
- `src/store.rs` / `src/core/optimize.rs`:ツール名を口にする copy("written by a newer
  saniwalens"、`saniwalens sql` の walkthrough 行)。
- `README.md`、`docs/specs/`、`docs/pr/`、`.github/workflows/`、`flake.nix`:インストール
  先(`cargo install saniwalens`、`brew install armaniacs/saniwalens/saniwalens`、
  `nix run github:armaniacs/saniwalens`)と文章中の名前。`lambdalisue` の参照として残るの
  は `nix-github-release` のみ — release workflow が再利用する別ツールで、このプロジェク
  トではありません。

## この PR の後

- `cargo install saniwalens` / `saniwalens doctor --harness kilocode`。
- GitHub リポジトリ、Homebrew tap(`armaniacs/homebrew-saniwalens`)、crates.io への公開は
  この PR の外で続きます。
