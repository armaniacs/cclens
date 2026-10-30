# PBI: briefing組立を core の背後に移す

## ユーザーストーリー
saniwalensのメンテナとして、briefing の組立ロジック（約220行の
HashMap配線・上限値・ソート）を `collect_findings` から core 側へ移したい。
表示崩れの原因がテスト済みの純粋部品ではなく、テスト外の配線にあるためだ。

## 優先度
- 順位: 04 / 6件
- RICEスコア: 1.4（Reach=3 / Impact=2 / Confidence=70% / Effort=3日）
- 根拠: optimize/doctor の findings 経路が恩恵を受ける。純粋部のテストは
  既にあるため、組立部の移設に集中できる。

## BDD受け入れシナリオ

```gherkin
Scenario: 同一ストアから同一の findings が得られる
  Given 固定のテスト用ストア
  When findings を組み立てる
  Then フィールド単位で従来と同一になる

Scenario: 上限値が名前付き設定として保たれる
  Given thrash上限5・例示上限などの定数
  When briefing を生成する
  Then 定数がテストから参照できる名前で存在する

Scenario: 無効な scope の扱いが変わらない
  Given 存在しない scope 指定
  When findings を要求する
  Then 従来どおりのエラーになる
```

## 受け入れ基準
- [x] `collect_findings` / `config_wedges` の配線ロジックが core 側に移る
  （`core::findings::{collect_findings, config_wedges}` に移設。
  ScopedWedges/WedgeRefs も core へ。store 型は受けず
  `FindingsInput` / `CatalogRow` のプレーンな行データ経由）
- [x] `cli.rs` 側は fetch＋print のみ残る
  （`findings_input(store)` がクエリ＋store行→プレーン行の写像、
  `collect_findings` はその呼び出し1行に縮小）
- [x] 上限値（15/5/3等）が名前付きでテスト可能になる
  （`TOP_COMMANDS_LIMIT` / `HOTSPOTS_LIMIT` / `EXAMPLES_PER_CATEGORY` /
  `EXAMPLES_PER_GLOBAL_CATEGORY` / `THRASH_LINES_PER_PROJECT` /
  `THRASH_GAP_SECS` / `THRASH_MIN_EDITS` / `HEAVY_TOKENS`（core::surface））
- [x] doctor/optimize の実出力が従来同一（代表ケースのdiff確認）
  （旧821bf1e バイナリ vs 現行のE2E29シナリオ269行＝
  doctor / doctor --format json / optimize --print を含めバイト同一）

## テスト戦略
- E2E: 実バイナリで doctor/optimize の出力 diff（手動＋記録）
- 統合: 固定ストアに対する findings 組立の同一性テスト
- 単体: 上限値・ソート・フィルタの境界値テスト

## 見積もり
3日目安（要チームでの見積もり）

## 実装者向け注記

### 現状コードの確認
```bash
grep -n "fn collect_findings\|fn config_wedges\|fn doctor_agent_lines" src/cli.rs
grep -n "fn render_briefing\|fn compose_prompt" src/core/optimize.rs
```

### 実装手順
1. 現状 findings のゴールデンを固定する（テスト用ストア→JSON保存）
2. 組立を core 側へ一段ずつ移す（RED→GREENを小刻みに）
3. cli.rs 側を fetch＋print に縮める
4. 上限値を名前付きにする
5. フルテスト＋実出力のdiff確認

### 落とし穴
- 7〜8個の bespoke クエリの結合順を変えると結果が変わる。移動時は
  式を写すだけで意味を変えない
- `is_actionable` フィルタと split ルーティングは仕様（spec-sync対象）。
  変えるなら `docs/specs/` も同梱で更新する

## Definition of Done
- [x] 全BDDシナリオが自動テストとして実装されパスする
  （`named_limits_cap_the_briefings_lists` /
  `assembly_routes_and_aggregates_like_the_recorded_briefing` /
  `no_observed_floor_reads_as_unknown_not_as_a_cost` を RED→GREEN。
  無効 scope の扱いは parse_scope 無変更＋E2E の不正 --scope パスで確認）
- [x] コードレビュー完了（2026-10-30: 検証エビデンス — cargo test 391 passed / clippy -D warnings / 旧821bf1e とのE2E269行バイト同一 — をもって利用者確認済み）
- [x] ドキュメント更新済み（仕様変更があれば `docs/specs/`）
  （cli.md の optimize 節に fetch/assembly の分界を追記、
  architecture.md の core 行に findings 組立を明記）
