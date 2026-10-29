# PBI: ストアの ingest 順序と列エンコーディングを型で守る

## ユーザーストーリー
saniwalensのメンテナとして、ストアの暗黙プロトコル（openの2経路、
ingestの呼び出し順、列への意味の載せ替え）を型と単一IFで守りたい。
なぜなら新イベント種別のたびに全読み手の改修が必要になり、複利で重くなるからだ。

## 優先度
- 順位: 02 / 6件
- RICEスコア: 1.6（Reach=8 / Impact=2 / Confidence=50% / Effort=5日）
- 根拠: スコア順では2位。ただしblast radiusが最大のため、着手条件として
  ingest順序とエンコード表の回帰テストを先に書くこと（DoDゲート）。
  B・Eと僅差だが、読む側全体への課税という観点で上位に置く。

## なぜなぜ分析（要点）
- なぜ危険か → ストアは全読み書きの安定 spine だから
- なぜ今やるのか → 新イベント種別ごとに読み手全改修が必要で複利に重くなる
- 解 → 壊す前に回帰テストの harness を作り、その保護下でのみ順序と
  エンコードを型・単一IFへ移す

## BDD受け入れシナリオ

```gherkin
Scenario: ingest順序が呼び出し側に依存しない
  Given 任意の順序で ingester を呼べるIFがある
  When 利用者が analyze を実行する
  Then 順序によらず同一の行集合がストアに入る

Scenario: 列の意味が読み手に分散しない
  Given tool_errors の各列
  When 新イベント種別を追加する
  Then 読み手側の改修なしに既存クエリが通り、意味は1箇所の表で定義される

Scenario: open経路の保証が明示される
  Given 新旧どちらの open 経路でも
  When ストアを開く
  Then マイグレーション有無の保証が型か文書で分かる
```

## 受け入れ基準
- [x] open経路が1つに統一されるか、保証の違いが型で表現される
  （`Store::open(path, OpenMode::{ReadWrite, ReadOnly})` に統一。
  作成・owner-only・マイグレーションの保証はモードの型で明示。
  `readonly_open_neither_creates_nor_migrates` が非作成・非移行を固定）
- [x] ingestがバッチIFになり、呼び出し順への依存が消える
  （`IngestBatch` + `Store::ingest` を唯一の公開 IF に。旧5関数は private の
  tx ヘルパへ格下げ、呼び出し側3分析器はバッチ構築1回に移行）
- [x] 列エンコーディングが1箇所の表で定義される
  （`EVENT_ENCODING` が単一定義。view はそこから生成、reader は
  `encoding_column` 参照、writer の tool_error/prompt 列も表由来。
  種別追加は表に行を足すだけ＝`adding_a_kind_to_the_encoding_table_leaves_the_view_untouched`）
- [x] 既存の全storeテストがグリーン＋順序回帰テストが追加される
  （`cargo test` 381 passed。`a_batch_replaces_every_prior_row_for_the_file_wholesale` /
  `one_batch_settles_the_spawn_costs_of_the_runs_it_carries` を追加）

## テスト戦略
- E2E: 実バイナリで analyze→各view の出力が従来同一（手動＋記録）
- 統合: 順序を入れ替えた ingest バッチの同一性テスト、エンコード表のテスト
- 単体: マイグレーション分岐、メタガード条件

## 見積もり
5日目安（要チームでの見積もり。spine のため慎重に）

## 実装者向け注記

### 現状コードの確認
```bash
grep -n "pub fn ingest_\|pub fn open\|fn migrate" src/store.rs
grep -n "surface_id\|source\|model" src/store.rs | head -20
```

### 実装手順
1. 先に回帰テストを書く（順序入替・列意味の固定）。REDを確認してから進む
2. ingestバッチIFを導入し、呼び出し側を移す
3. open経路を統一する（または保証を型で分ける）
4. エンコード表を1箇所に寄せる
5. フルテスト＋E2E出力のdiff確認

### 落とし穴
- マイグレーションは本番データを触る。in-memory/fixture のみで検証し、
  実ストアでは読み取り専用確認に留める
- `tool_errors` view の列順・列名は外部契約（`saniwalens sql` 利用者向け）。
  変える場合は別PBIに切り出す

## Definition of Done
- [x] 回帰テスト（順序・エンコード）が先に書かれパスする
  （RED: 新IF参照のコンパイルエラーを確認 → GREEN: 381 passed）
- [x] 全BDDシナリオが自動テストとして実装されパスする
  （順序シナリオ=バッチ置換の同一性、エンコード=view導出＋種別追加非依存、
  open保証=readonly の非作成・非移行）
- [x] コードレビュー完了（2026-10-30: 検証エビデンス — cargo test 391 passed / clippy -D warnings / 旧821bf1e とのE2E269行バイト同一 — をもって利用者確認済み）
- [x] ドキュメント更新済み（`docs/specs/storage.md` の契約節）
  （One transcript, one batch 節・Column encoding is defined once 節・
  OpenMode の readonly 節を追記）
