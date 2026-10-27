# この PR が追加するもの: マルチハーネス対応（第一弾として opencode）

`saniwalens` はコーディングエージェントの利用実績を解析し、設定のどこを最適化できるかを
見せるツールです。コンテキストトークン・経過時間・摩擦（失敗の繰り返し）というコスト
を、各設定面（skills、rules、hooks、MCP、agents、`CLAUDE.md` / `AGENTS.md` など）の
実際の使用状況と突き合わせます。

これまで理解できたのは **Claude Code だけ**でした。トランスクリプトも設定も
`~/.claude/...` の Claude Code 形式のみを読み、レポートの文言も Claude Code 前提で
書かれています。この PR はツールを他のエージェントに一般化する第一歩で、対象は
**opencode** です。既存の Claude Code ユーザーへの影響はゼロです。

## 課題

元のツールには Claude Code の成果物がハードコードされています。

- 入力: セッションのトランスクリプトと設定を `~/.claude/...` から Claude Code 形式で
  のみ読みます。
- レポート: 「Claude Code」「`~/.claude`」「`CLAUDE.md`」「`.claude / CLAUDE.md`」と
  いう文言が、どのエージェントのデータに対しても固定で出ます。
- AI 支援の最適化: `optimize` は `claude` 自身を起動するので、Claude Code が無い
  環境ではそもそも動きません。

セッションが opencode 側にあるユーザーには正しいアドバイスができません。データを
読めず、読めたとしても指す先のファイルが違います。

## 追加したもの

### `--harness` フラグとハーネスごとの分離

- `--harness claude` がデフォルトで、挙動は従来と完全に同じです。`--harness opencode`
  で全コマンドが opencode のデータに切り替わります。
- ハーネスごとに**独立した SQLite ストア**に解析結果を書きます（`saniwalens.db` と
  `saniwalens-opencode.db`）。2 つのエージェントの解析結果が混ざることはありません。
- 間違ったハーネスでストアを読むと、黙って混ぜるのではなくヒント付きで失敗します
  （「このストアには opencode のセッションが入っています — `--harness opencode` で
  再実行するか、別の `--db` を指定してください」）。frozen 読み取りも同様にガードし、
  不正な値は取り得る値とともに拒否します。

### opencode アダプタ

新規の `src/adapter/opencode.rs` が opencode の形式に関する知識をすべて担います。
Claude Code アダプタの役割をそのまま写したものです。

- セッションは**opencode 自身のストア**（`~/.local/share/opencode/opencode.db` の
  `session` / `message` / `part` テーブル）から読みます。トランスクリプトファイルでは
  ありません。part をドメインレコードにし、Claude 経路と同じ正規化イベント（spans、
  usage、tool errors、work events）へ流し込み、中間ストアの先は一切変えません
  （`docs/specs/architecture.md`）。
- 設定面は opencode が公式に定めている場所から読みます。グローバルの
  `~/.config/opencode`（commands、agents）、プロジェクトの `.opencode`、`@import` 込み
  の `AGENTS.md`、そして**公式ドキュメントにある 6 箇所の skills 配置**。
- 上流がバージョン管理していない形式には緩く対応します。未知の part 形態でも
  tool・target・excerpt は失わず、黙って消えることがありません。コスト行はターン終了
  時刻で打刻するので、末尾の part が負の期間を生みません。
- skills は 6 箇所の配置を **canonical path で重複排除**して読みます。配置同士が
  symlink で 1 つの共有ディレクトリに届くことがあるためです（実際に確認できた形は
  `.opencode/skills -> ../.agents/skills`）。名前で排除すると、共有定義がどちらかの
  スコープで「未使用」に見えてしまい、カタログが捏造する wedge になります。

### レポート全体をハーネスの言葉に合わせる

`Harness` 抽象（src/adapter/harness.rs）が各ハーネスに必要な語彙を持つので、レポート
が Claude Code 前提の文言で固定されなくなりました。

- 表示名（"Claude Code" / "opencode"）、冠詞、instruction file（`CLAUDE.md` /
  `AGENTS.md`）、プロジェクト設定の参照先（`.claude / CLAUDE.md` と
  `.opencode / AGENTS.md`）、グローバル設定の参照先（`~/.claude` と
  `~/.config/opencode`）、コスト説明の主語。
- ハマり行（thrash）はエージェント中立です。"the agent got stuck re-editing ..." と
  いう文言で、opencode 出力に Claude 固有の語は残っていません。
- `optimize` のプロンプト、ブリーフィング、ガードメッセージも同じ経路で描画します。

### ハーネス自身のエージェントを起動する AI 支援の最適化

`optimize` が**選択したハーネスのエージェント**を、解析結果を渡して起動します。

- Claude Code: 従来どおり。対話セッションで `claude` を起動し、ユーザーが計画を
  承認してからのみファイルを変更します。
- opencode: **ヘッドレス**の `opencode run --format json` セッション。プロンプトには
  無変更の命題を入れます（計画が最終出力であり、承認するユーザーは席にいない）。
  起動は成功を仮定せず、opencode 自身のエラーメッセージで検証します。
- ヘッドレスのターミナルは静かです。ナレーションと最終計画は stdout へ
  （パイプフレンドリー、`| pbcopy`）、静かな時間帯は薄いハートビート行が 1 行出るので
  停止しているようには見えず、生の行とアドバイザの stderr は非公開の `0600` ログへ
  書き、パスを冒頭で表示します。後続の PR（`feature-activity-indicator`、この PR に
  積むスタック構成）がハートビートをライブのインプレース・ステータス行 — スピナー、
  今の作業、経過時間 — に置き換えます。

### 実データでの検証から見つかった堅牢化

全機能を実際の opencode ストア（8 プロジェクト、91 セッション）で検証しました。
試行で見つかった 4 つの修正をすべて含みます。

- 実際のエラー文言から**繰り返し失敗のカテゴリ**を追加しました。`binary-file`
  （エンコーディング不一致であって、ファイルが無いわけではない）と `web-error`
  （取得した URL が 404 を返す）。分類できなかった 8 件のうち 6 件が、対処法付きの
  名前付き行になりました。
- **存在するのに読めない**カタログディレクトリは、空として黙るのではなく警告します。
  macOS は `~/Documents` をアプリごとの許可で守るため、そこにあるプロジェクトは
  「skills 未設定」と静かに読めてしまい、稼働中の面がすべて orphaned usage に見え
  ました。存在しないディレクトリは黙ったまま（スコープが単に無いだけの場合が多い）、
  それ以外の読み取り失敗ではディレクトリ名付きの警告を 1 行出します。
- 常時コンテキストの**誠実な帰属**: floor からグローバル設定を引いた残りをすべて
  「ハーネス自体」とするのは不正確です。大半はユーザー自身の skills の description と
  `AGENTS.md` です。コピーはそれを言うようにし（推定が floor を覆う場合はその旨を
  伝え、「0」を出しません）、prompts ビューは解析済みストアに「`analyze` を再実行
  せよ」と回り道をさせなくなりました。

## 意図して外したもの

- **DSH と kilocode のアダプタ。** `--harness` フラグとストア分離は準備済みですが、
  両者のセッション・設定の形式はまだ確定していません。推測した形式に対して書いた
  アダプタはノイズを測るだけです。
- **opencode の MCP 面。** 観測した設定に MCP 定義がなく、MCP 名前空間のツールも
  利用サンプルに現れなかったため、server と tool の結合を確定できません。
- **opencode の permission 面。** 権限テーブル自体は実在しますが、実行時の拒否信号が
  結合先に無く、全項目が「未使用」に見える偽の wedge を生みます。
- **opencode でのグローバル `AGENTS.md`。** opencode が読み込むか未検証のため、
  プロジェクト側のファイルのみ走査します。

いずれも理由付きで `docs/specs/config-format-opencode.md` に記録しています。

## 互換性

- `--harness` 無し（または `--harness claude`）では挙動は変わりません。同じ入力、
  同じレポート、同じ `optimize` です。
- 共有コア（レコード → イベント → 集約、surface と usage の結合）はハーネス非依存で、
  ハーネスの知識はアダプタ層にしかありません（`docs/specs/architecture.md`）。
- ストアのスキーマにマイグレーションは要りません。ハーネスごとに別の DB ファイルを
  書くだけです。

## 検証方法

- ゲート全体が通ります。`cargo fmt --check`、`cargo clippy --all-targets --all-features
  -- -D warnings`、`cargo test` — **301 テスト**（このブランチの前は 247）。
- 全レポートビュー、ガード、両 `optimize` 起動モードを実データの opencode ストア
  （8 プロジェクト、91 セッション）で実行しました。flag 無しの Claude Code 経路も
  実データの Claude ストア（367 セッションを再解析、挙動無変更）で再確認しています。
- 各部の設計意図は spec-first で記録してあります。`docs/specs/harness.md`
  （opencode での `optimize` 契約、ハーネス名を含むレポート文言）、
  `docs/specs/session-format-opencode.md`（セッションストアの対応）、
  `docs/specs/config-format-opencode.md`（設定面と、理由付きの除外一覧）、および
  `docs/specs/cli.md` と `docs/specs/surfaces.md` への追記。
- 設計に至った探索の記録（kilocode のトークン調査を含む）は
  `docs/exploration/other-harness.md` にあります。

## コミット対応

17 コミットがレイヤーごとに積んであり、各コミット時点でゲートが通っています。

- `1910e1f`、`2ba235b`、`4211c9c` — 探索メモ（形式、意思決定、kilocode のトークン
  調査）と英語への書き直し
- `422d4f2`、`36694fc` — opencode セッションのドメインレコード化。ターン終了での
  打刻により、末尾の part が負の期間を生めないように
- `2c7cab7` — `--harness` フラグ、ストア分離、opencode での analyze
- `515c9fd` — opencode の設定カタログ（commands、agents、`AGENTS.md`）
- `f3508e3` — 観測した opencode のエラー文言の分類
- `c5a8579` — レポートとブリーフィングをハーネスの言葉に
- `1746587`、`7e54fa8`、`7092a60` — opencode でのヘッドレス `optimize`:
  起動、静かなターミナル、ネストした part の描画修正
- `fc3fec1`、`a1a5d3b`、`3945884`、`9abdfeb` — 実データ検証による堅牢化: 失敗
  カテゴリの追加、6 箇所の skills 配置と canonical 重複排除、読めないディレクトリの
  警告、誠実な帰属
