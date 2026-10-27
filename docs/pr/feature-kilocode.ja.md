# この PR が追加するもの: kilocode ハーネス(マルチハーネス対応に積む第二弾)

`agentlens` はコーディングエージェントの利用実績を解析し、設定のどこを最適化できるかを
見せるツールです。積み替えの土台 PR(`feature-otherharness`)が `--harness` フラグと
ハーネス単位の分離によってツールを他のエージェントへ一般化し、第一弾として opencode を
対応させました。この PR は同じフラグの下に第二のハーネスとして **kilocode**
(Kilo Code / `kilo` CLI)を追加します — `--harness kilocode`。claude と opencode の
挙動は一切変わりません。

## 問題

Kilo Code にセッションがあるユーザーは何も得られませんでした。ツールは kilocode の
データをまったく読めません。しかも kilocode のトランスクリプトは Claude Code の経路が
決して見ない場所 — `~/.claude/projects` のトランスクリプトでも opencode の SQLite
ストアでもなく、エディタの `globalStorage` タスクファイル — にあります。

## 追加したもの

### kilocode アダプタ

新しい `src/adapter/kilocode.rs` が kilocode の形式に関する知識をすべて担います
(`docs/specs/session-format-kilocode.md`)。opencode アダプタと同じ役割です。

- セッションは**エディタの globalStorage タスクファイル**から読みます
  (`Library/Application Support/Code/User/globalStorage/kilocode.kilo-code/tasks/<uuid>/`):
  `api_conversation_history.json`(Anthropic 形状のメッセージ)と、トークン用の
  `ui_messages.json`。フィンガープリントはタスク単位(`kilocode:<uuid>`、最大 mtime +
  合計サイズ)なので、変わっていないタスクは再解析でスキップされます。
- トークンは `ui_messages.json` の `say=api_req_started` 行から時刻順に結合します
  (`tokensIn/tokensOut/cacheWrites/cacheReads/cost/usageMissing`)。ウィンドウごとの
  session_start が 1 行、オーバーヘッドの床になります。
- ツール失敗は**ツールが運んできた読めるメッセージ**として読みます。実測の
  `tool_result` は失敗を text block 内の JSON エンベロープに包み、さらに
  `environment_details` 装飾 block を後ろに付けます。結合した blob を丸ごと保存して
  いた頃は二重に誤分類していました(envelope 自体の "does not exist" が path 語彙に、
  装飾のファイル名が timeout に一致)。各 block を解凍し、装飾を落とし、カテゴリは
  excerpt と同じテキストから取ります。
- セッションは `execute_command` が運んできた最初の絶対 `cwd` を root にします —
  タスクごとにちょうど 1 つの値しか持たないことを実測済み — これで 92 タスクのうち
  68 が 1 プロジェクトに root し、ノイズになりません。

### kilocode config カタログ

`docs/specs/config-format-kilocode.md` が設定半分をカタログします。実在する
`~/.config/kilo/` ツリーと実プロジェクトで観測した内容です: commands、agents、skills
サイト(canonical path で重複排除)、グローバルとプロジェクトの `AGENTS.md`、
`.kilorules`、そして `kilo.json` の `instructions[]` の各ファイル — kilo 独自の追加
で、実測ではプロジェクトが gitignore 済みの `AGENTS.local.md` をこの配列で指していま
した。この経路でしか届かないファイルはカタログからは見えないのに毎セッション課金され
ます。MCP と permissions は v1 の外です:kilocode は `mcp__` ツール呼び出しをゼロ回
発行し、拒否マーカーもないため、どちらのカタログも「未使用」なくさびを捏造するだけで
す。実ストアで計測:83 サーフェスをカタログし、`waste`/`inventory` が突き合わせます。

### `optimize` は対話的 kilo セッションを開く

`--harness kilocode optimize` は `kilo --prompt <ポインタ>` を起動します — claude
起動と同じ体験で、ユーザーが同席してブリーフィング読みを承認できます(opencode と違
い kilo は全ツール呼び出しにゲートがあるため、headless では `--auto` が必要 — 動作
することも、なしでは死ぬことも実測済み)。この試行で agentlens 自身の実欠陥 3 件が見つか
り修正されました:上記の excerpt 欠陥、未知のツール名が path-not-found に誤分類された
件("unknown tool" を tool-not-available 語彙に追加)、そして work events が epoch を
持ったまま `started_at` を空のまま残していた件 — これは kilocode 固有ではなく共有の
store 欠陥でした。

## コミット

- `29d5830` — kilocode タスクファイルのパース(spec `session-format-kilocode.md`)
- `aebf8a1` — `--harness kilocode` analyze 配線
- `7b3cf65` — `execute_command` の cwd でセッションを root
- `6d3d599`, `65d81f3`, `73bbf6b`, `01ee28b` — optimize 起動:拒否、kilo 起動配線、
  headless 経路、そして対話的セッション
- `81b80ea`, `186575a`, `d483232` — 実データウォークスルーの堅牢化:読める
  タイムスタンプ、誠実なツール名分類、excerpt 修正、所見の記録
- `5bf22d1`, `996db74` — config カタログ(spec、実装)
- `97aa71a`, `44cd78c`, `b2ba261` — lint、誠実な欠落の注記、README
