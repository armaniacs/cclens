# PBI: 使用差分トレンドビュー（2 期間の比較）

## ユーザーストーリー
設定変更の効果を測りたい分析者として、今の期間とその直前の同じ長さの期間を
比較して「使用が落ちた / 増えた」を見たい。なぜなら再計測のたびに全期間の
集計しか出せず、前回との比較手段が無いからだ。

## ビジネス価値
- 「先週 vs 先々週」のイベント種別・スキル別・セッション/subagent の差分が
  1 コマンドで見え、設定変更の効果測定ができる
- 測定方法: 両ウィンドウにイベントがある合成ストアで `trend` が両ウィンドウの
  数値と差分を出すこと。片側だけのストアでは「比較不能」を -100% ではなく
  明示すること

## BDD受け入れシナリオ

```gherkin
Scenario: 直近 7 日とその前の 7 日を比較する
  Given ストアに 14 日分以上のイベントがある
  When ユーザーが `saniwalens trend` を実行する
  Then 現在ウィンドウ（直近 7 日）と直前ウィンドウ（その前の 7 日）の両方の
  数値と差分が行ごとに出る
  And 2 つのウィンドウの範囲が明示される

Scenario: 直前ウィンドウにイベントが無い
  Given ストアに 7 日分しかイベントが無い
  When ユーザーが `saniwalens trend` を実行する
  Then 直前ウィンドウの比較は「比較不能」であることが明示され、
  -100% という誤読を生む表示は出ない

Scenario: 解析できない日付はエラーになる
  When ユーザーが `saniwalens trend --since 2026/10/01` を実行する
  Then コマンドは解析できない日付の旨を stderr に出力し、非ゼロで終了する

Scenario: until だけの指定はエラーになる
  When ユーザーが `saniwalens trend --until 2026-10-01` を実行する
  Then コマンドは現ウィンドウの起点が定まらない旨を stderr に出力し、
  非ゼロで終了する
```

## 受け入れ基準
- [ ] 新ビュー `trend` が現在ウィンドウとその直前の同長ウィンドウを比較する。
  既定は直近 7 日 vs その前の 7 日。`--last <N>d|h|w` で長さを、
  `--since`（+ 任意の `--until`）で現在ウィンドウを指定できる。
  `--until` 単独はエラー
- [ ] 行: 種別ごとのイベント集計（count / out_tokens / duration の差分）、
  スキル別の増減（差分の大きい順）、セッション数と subagent runs/tokens の
  合計。各行に両ウィンドウの値と差分を出す
- [ ] 増減の向きを明示する（増えた / 減った / 同じ）。直前ウィンドウに
  イベントが無い場合、その比較は「比較不能」と明示され -100% と読めない
- [ ] 2 つのウィンドウの範囲を明示する（freshness 行（stderr）、framing
  block（table のみ）、`--format json` に `window`（current / previous）を追加）
- [ ] 期間解釈は PBI 01 の規約に揃える（`YYYY-MM-DD` は JST、`--until` は
  inclusive、`since > until` はエラー、`--last` は now 起点のローリング）
- [ ] `--format table|markdown|json` / `--frozen` / `--db` を他のビューと
  同様に持つ。freshness（自動 refresh）の慣例に従う
- [ ] 既存 8 ビューと `sql` / `analyze` / `optimize` の出力は一切変わらない
  （新コマンドは追加のみ）
- [ ] `docs/specs/cli.md` の Views 表に `trend` の行を追記し、Period 節との
  関係（比較対象の 2 ウィンドウ）を書く。`docs/specs/storage.md` は変更しない
  （カタログ履歴は別候補）
- [ ] `just check`（各コマンド直接実行）と `cargo test` が green

## テスト戦略（t_wadaスタイル）

### E2Eテスト
- 両ウィンドウにイベントがあるストアで `trend --format json` が current /
  previous の `window` と行を出す
- 片側だけのストアで「比較不能」が出ること

### 統合テスト
- 2 ウィンドウ集計が started_epoch で正しく分かれる（境界、同長、前後関係）
- スキル別の増減が差分の大きい順に並ぶ

### 単体テスト
- ウィンドウ解決: 既定（7d）、`--last`、`--since`+`--until`（直前同長の導出）、
  `--until` 単独のエラー、`since > until` のエラー
- 差分の向きと「比較不能」の判定

## 実装アプローチ
- **Outside-In**: e2e（JSON 構造・比較不能）から失敗を確認し、ウィンドウ解決
  （単体）→ 2 ウィンドウ集計（統合）→ ビュー配線の順に Red-Green-Refactor
- 実装は subagent-driven-development スキル（implementer → task review →
  最終 review）で実施する

## 見積もり
5 SP（フル実装の E=3 から、使用差分のみに絞った分）

## 技術的考慮事項
- 依存関係: PBI 01 の期間機構（`TimeRange` / `range_clause`）を再利用する。
  スキーマ変更なし（イベント履歴のみ使う）
- テスタビリティ: ウィンドウ解決は純粋関数（now 注入）。Store は in_memory
- 非機能要件: 2 ウィンドウ分の集計は full scan 2 回で現行規模なら問題なし

## 実装者向け注記

### 現状コードの確認（2026-10-30 実施済み）
- コマンドは `src/cli.rs` の `Command` enum（58 行目付近）に追加する。既存
  ビューの流れ: `parse_period` → `Session::open_read` → 集計 → `eprint_window`
  → framing → 行
- PBI 01 で追加済みの機構を再利用: `src/core/period.rs`（`TimeRange` /
  `resolve`、JST 解釈、`since > until` エラー）、`src/store.rs` の
  `range_clause` / `range_clause_utc_text`
- 直前ウィンドウは現在ウィンドウと同じ長さで直前に置く（同長が比較の前提）。
  `--last` は now 起点なので、現在 = (now-N, now]、直前 = (now-2N, now-N]

### 実装手順
1. `src/core/period.rs` に 2 ウィンドウ解決を追加（`TimeRange` の組を返す
   純粋関数、now 注入）。`--until` 単独はエラー
2. `Store` に 2 ウィンドウ集計を追加（既存の集計メソッドに範囲を渡す形を
   再利用するか、`range_clause` で 2 回集計するかは実装者に任せる）
3. `src/cli.rs` に `trend` コマンドを追加（freshness / framing / JSON の
   既存慣例に従う）
4. `docs/specs/cli.md` の Views 表に `trend` の行を追加（spec-sync 規約）
5. `cargo test` → `just check`（直接実行）→ 既存ビュー出力が変わらないことを
   確認（capture の diff）

### 落とし穴
- 直前ウィンドウにイベントが無いケースを「-100%」と表示すると誤読を生む。
  「比較不能」を明示する（Reporting honesty の「近似は隠さずラベルする」）
- `--since`+`--until` のとき直前ウィンドウの導出で inclusive/exclusive を
  誤ると 1 日ずれる。`TimeRange` の境界規約（inclusive `--until`）に揃える
- 既存ビューの freshness/framing に触れると出力が変わる。`trend` の追加に
  とどめ、既存 8 ビュー・`sql` / `analyze` / `optimize` は触らない
- 2 軸問題（sessions の started_at と events の started_epoch）は PBI 01 の
  straddle テストの規約に従う。trend はイベント起点なので片軸で足りるが、
  セッション数を出すなら PBI 01 のテスト規約に揃える

## Definition of Done
- [ ] 全 BDD シナリオが自動テストとして実装されパスする
- [ ] 既存ビュー・コマンドの出力が一切変わらないことを確認
- [ ] `just check`（直接実行）と `cargo test` が green
- [ ] コードレビュー完了（task reviewer）
- [ ] docs/specs/cli.md 更新済み
