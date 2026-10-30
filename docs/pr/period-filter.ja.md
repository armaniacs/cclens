# この PR が追加するもの: レポートビューの期間フィルタ（--since / --until / --last）

`saniwalens` はコーディングエージェントの使用状況を分析し、設定のどこを
最適化できるかを見せる CLI です。ストアは全セッション・全イベントの歴史を
保持しているのに、レポートは常に全期間の集計しか出せませんでした。「先週は
いくら使ったか」「設定変更は効いたのか」を聞く手段が無い。この PR は 8 つの
レポートビューに `--since` / `--until` / `--last` を追加し、集計を期間に
絞れるようにします。

このブランチには他に、この PBI を生んだブレインストーミングの候補台帳
（`pbi/2026-10-30-00-backlog.md`）と、実装をサブエージェント駆動（タスクごとに
implementer サブエージェント → task review → 最終に全体 review）に固定する
AGENTS.md の規約が乗っています。

## 問題

ストアは歴史を保持する（だから再 analyze は増分で済む）のに、レポートは期間を
選べませんでした。`usage` は `--by` で時間バケットにできるのに範囲を絞れず、
`waste` / `inventory` の「未使用」は全期間評価です。設定変更の効果が見えない —
「X を変えたあと失敗は減ったか」に答えが無い。最も近いツール ccusage は
`--since` / `--until` / `--last` で答えますが、saniwalens には何も無かった。

## 追加したもの

### 純粋な期間コア

`src/core/period.rs` が 3 フラグを UTC エポックの `TimeRange` にパースします。
`--since` / `--until` は `YYYY-MM-DD` を提示タイムゾーン（既定 JST — バケット
ラベルと同じ規約で、UTC 21:00 のイベントは JST 翌日）で解釈し、`--until` は
その日のいっぱいまで含みます。`--last <N>d|h|w` は注入した now からの
ローリングウィンドウで、clap レベルと `resolve` 内側の二重に
`--since` / `--until` と排他です。`since > until` はエラー — 空のレポートは
「使用が無い」と誤読されるためです。

### ストアへの範囲絞り込み

16 の集計メソッドが範囲を受け取ります。2 つのヘルパー（イベントの
`started_epoch` 用 `range_clause`、一様な RFC 3339 UTC テキスト列用
`range_clause_utc_text`）がウィンドウを SQL にする唯一の場所で、query が
フラグで分岐することはありません。inventory のカタログ半分は絞りません —
カタログは現在の設定なので usage 半分だけが絞られ、「未使用」は「ウィンドウ内で
未使用」と読めます。未スタンプのセッションは期間内のセッション集計から除かれ、
フラグ無しでは数え続けます（`docs/specs/cli.md` に明記）。

### 3 チャネルでの開示

引かれたウィンドウは stderr の freshness 行（全形式）、table の framing
block（table のみ — markdown は素のテーブルのまま）、`--format json` の
`window` キー（既存キーは触らない）で明示されます。フラグ無しでは何も変わり
ません: 開示ヘルパーはすべてウィンドウが引かれたときだけ発火し、無指定出力は
変更前のバイナリとバイト同一です。

### 意図して変更しないもの

`sql` は生 SQL の脱出ハッチ（WHERE は自分で書く）、`analyze` は全歴史を
取り込み続け、`optimize` はフラグを取らず、briefing は全期間の合計を保ちます
（フラグ拒否テストで固定）。

## 検証

- `cargo test` — 418 passed, 0 failed: `core::period` の単体 13 テスト（JST の
  日境界、月末、ローリングウィンドウ、パースエラー）、実 in-memory ストアでの
  統合テスト（inclusive な `--until`、ミリ秒小数の跨ぎ、ウィンドウ付き
  LEFT JOIN でもカタログ行が消えない）、2 軸の subagent 数値を固定する
  straddle テスト。
- `cargo clippy --all-targets --all-features -- -D warnings` と
  `cargo fmt --check` — クリーン。
- バイト同一: 無指定出力が capture 60 ファイル（8 ビュー × table/json ×
  stdout / stderr / 終了コード）で変更前バイナリとバイト同一。
- レビュー経緯: サブエージェント駆動（implementer → task review → 最終全体
  review → fix → re-review、すべて承認）。最終 review の Important —
  `--until` を跨ぐセッションで subagent の分裂表記が虚偽の理由を出す件 — は
  ウィンドウ gate の文言で修正し、2 つの時刻軸は統一せず straddle テストで
  固定しています。
