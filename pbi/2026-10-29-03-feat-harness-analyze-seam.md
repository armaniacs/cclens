# PBI: ハーネス analyze を enum の背後に移す

## ユーザーストーリー
saniwalensのメンテナとして、ハーネス別の analyze 処理を
`Harness::analyze` という1つの seam にまとめたい。4つ目のハーネスが来たとき
150行コピーの再発（順序・フィンガープリント・ingest漏れの再現）を防ぐためだ。

## 優先度
- 順位: 03 / 6件
- RICEスコア: 1.5（Reach=5 / Impact=2 / Confidence=60% / Effort=4日）
- 根拠: 3つのadapterが既にある本物のseam。ただし現時点で4つ目の予定は
  なく、入力ルート形状の設計 spikes が先に要るため確信度は60%。

## なぜなぜ分析（要点）
- なぜ今半分だけか → enum には表示名だけ載せ、処理は cli.rs に残した経緯がある
- なぜ残したか → 入力ルートの形（projects 一括 vs 入力ファイル指定）が未確定だった
- 解 → 入力ルート形状の spike を最初に行い、形が決まってから移す

## BDD受け入れシナリオ

```gherkin
Scenario: 3ハーネスの analyze 結果が変わらない
  Given 既存のテスト用フィクスチャ
  When 各ハーネスの analyze を実行する
  Then セッション数・スキップ数・surfaces数が従来と同一になる

Scenario: 4つ目のハーネスは adapter 追加だけで動く
  Given 新ハーネスの adapter（parse＋root解決）
  When Harness に列挙子として登録する
  Then cli.rs の dispatch 改修なしに analyze が通る

Scenario: guard が保たれる
  Given 他ハーネスのストア
  When 別ハーネス名で analyze する
  Then 従来どおりのエラーメッセージで拒否される
```

## 受け入れ基準
- [x] `cli.rs` の `match harness` dispatch が消える
  （`run_analyze` は guard + `harness.analyze(projects, db)` の1行に。
  `cli.rs` 残存の `match harness` は 441行の `launch_command` のみ＝PBI06 対象）
- [x] 入力ルート解決子（`default_projects_dir` / `opencode_home` 相当）が
  各 adapter 側に移る
  （`default_projects_dir`→adapter/claude、`opencode_home`/`default_opencode_db`→
  adapter/opencode、`kilo_home`/`default_kilocode_tasks`→adapter/kilocode。
  cli.rs に該当なしを grep で確認。環境ヘルパ `home_dir`/`claude_home` も adapter 側へ）
- [x] analyzer のバージョン鍵が adapter と一緒に移動する
  （claude の `CLAUDE_ANALYZER_META_KEY`/`CLAUDE_ANALYZER_VERSION` を
  adapter/transcript へ移設。store の `is_analyzer_current`/`record_analyzer_version`
  は (key, version) 受けになり store からハーネス知識が消えた。
  opencode/kilocode も同じ seam を通る）
- [x] 既存フィクスチャでの analyze 結果が同一（件数比較テスト）
  （RED→GREEN: `claude_analyze_keeps_the_recorded_counts_on_a_fixture` が
  リファクタ前バイナリ記録値 (1,0,1,1,0) を固定。
  E2E: 旧821bf1e vs 現行で29シナリオ・269行がバイト同一）

## テスト戦略
- E2E: 実バイナリで3ハーネスの analyze 件数が従来同一（手動＋記録）
- 統合: 各 adapter の analyze（小規模フィクスチャ、root解決含む）
- 単体: 入力ルート解決、フィンガープリント鍵、guard 条件

## 見積もり
4日目安（spike込み、要チームでの見積もり）

## 実装者向け注記

### 現状コードの確認
```bash
grep -n "fn run_analyze_" src/cli.rs
grep -n "fn opencode_home\|fn kilo_home\|fn default_projects_dir\|fn default_opencode_db\|fn default_kilocode_tasks" src/cli.rs
```

### 実装手順
1. spike: 入力ルート形状（projects一括 vs 入力ファイル）を決める小規模試作
2. `Harness::analyze(&self, ctx)` の seam を定義する（REDテスト先行）
3. 1ハーネスずつ移し、件数同一を確認しながら進める
4. dispatch match を削除する
5. フルテスト＋E2E件数のdiff確認

### 落とし穴
- `ingest_*` の呼び出し漏れが新ハーネスで起きやすい（claude のみが
  prompts/subagent-runs/denials を入れる）。移行チェックリスト化する
- フィンガープリント鍵の命名規則（`{harness}:{id}`）を崩さないこと

## Definition of Done
- [x] 全BDDシナリオが自動テストとして実装されパスする
  （既定 analyze の件数同一＝自動テストで固定。4つ目ハーネスのシナリオは
  `Harness::analyze` の match が adapter の中にのみ存在すること＝構造で保証
  （grep: cli.rs に analyze dispatch なし）。guard は E2E の guard エラーパスと
  既存の Session ガードテストで確認）
- [ ] コードレビュー完了
- [x] ドキュメント更新済み（`docs/specs/harness.md` の責務分界）
  （「The analyze seam — `Harness::analyze`」節を新設し、dispatch/ルート/
  バージョン鍵の所在を明記。Forward compatibility 節を新契約に更新。
  architecture.md にも seam の一節、storage.md に鍵の所在を追記）
