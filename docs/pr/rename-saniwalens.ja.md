# この PR が追加するもの: ツール名は `saniwalens`、旧ストア移動は連鎖する

前回の改名 PR が選んだ `agentlens` は二重に誤りでした:crates.io にはすでに 1 月に公開
された**同領域のツール**(`nguyenphutrong/agentlens`、"CLI tool to prepare codebases for
AI agents")が同じ名前を持っており、衝突は現実的で、レジストリを検索するユーザーは誤った
場所に誘導されます。名前を「空き」と判断した可用性確認は faulty でした:sparse-index の
パスを作るコマンドが URL を不正な形にして、全候補を空きと読んでいました。

この PR は名前が効くすべてを **`saniwalens`** に改名します — レジストリ・index・GitHub
のどこにも一致がなく、完全に空きです。

## 旧ストア移動は連鎖する

バイナリは二回改名されました(`cclens` → `agentlens` → `saniwalens`)。ユーザーのストアは
どちらの旧名の下にもあります。現名での最初の open が、存在する方の旧ディレクトリを移動し
ます — 中間の `agentlens` か最初の `cclens` — ハーネス別ファイルも改名して:

- **削除しない** — 移動であって掃除ではない。
- **上書きしない** — 現名の下にすでにいるストアはそのまま。
- **明示的な `--db` はそのまま。**

## あわせて

- 手動 publish 試行が `Cargo.toml` に押し付けた `version = "0.5.0"` は `0.0.0` の
  プレースホルダに戻しました — タグが記録であり、コミット済みのバージョンが出荷されるも
  のではありません。
- release formula のクラス名はツール名から生成され(`class Saniwalens < Formula`)、tap
  リポジトリもそれに合わせて改名します(`armaniacs/homebrew-saniwalens`)。
