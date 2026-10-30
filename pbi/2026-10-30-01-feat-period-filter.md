# PBI: レポートビューの期間フィルタ（--since / --until / --last）

## ユーザーストーリー
エージェント設定を最適化したい分析者として、レポートを期間で絞りたい。
なぜならストアは歴史を保持しているのにレポートは常に全期間の集計しか出せず、
設定変更の前後で使用がどう変わったかを測れないからだ。

## ビジネス価値
- 「先週だけの waste」「設定変更後 7 日間の failures」が引け、設定変更の
  効果測定ができるようになる
- 測定方法: 同一ストアに対し `--since` / `--until` / `--last` 付きの `usage`
  がウィンドウ内のイベントだけで集計すること。フラグ無しの出力は現行と
  バイト同一であること

## BDD受け入れシナリオ

```gherkin
Scenario: 期間を指定して usage を絞る
  Given ストアに 2026-10-01 から 2026-10-31 のイベントがある
  When ユーザーが `saniwalens usage --since 2026-10-01 --until 2026-10-07` を実行する
  Then 集計はその 7 日分のイベントだけで計算される
  And レポートが評価ウィンドウ（since 2026-10-01、until 2026-10-07）を明示する

Scenario: --last で直近ウィンドウを指定する
  Given ストアに今日までのイベントがある
  When ユーザーが `saniwalens usage --last 7d` を実行する
  Then 集計は現在時刻から 7 日前以降のイベントだけで計算される
  And --last は --since / --until と同時に指定できない

Scenario: 解析できない日付はエラーになる
  When ユーザーが `saniwalens usage --since 2026/10/01` を実行する
  Then コマンドは解析できない日付の旨を stderr に出力し、非ゼロで終了する
  And usage の表は出力されない

Scenario: since が until より後ならエラーになる
  When ユーザーが `saniwalens usage --since 2026-10-07 --until 2026-10-01` を実行する
  Then コマンドは空のウィンドウをエラーとして非ゼロで終了する
  （空のレポートは「使用が無い」という誤読を生むため、暗黙に空表を出さない）

Scenario: フラグ無しの出力は現行と同一
  Given ストアが既にある
  When ユーザーがフラグ無しで各ビューを実行する
  Then 出力はこの変更前のバイナリとバイト同一である
```

## 受け入れ基準
- [ ] 8 つのレポートビュー（usage / inventory / waste / overhead / prompts /
  failures / stuck / doctor）に `--since` / `--until` / `--last` が付き、集計が
  期間に絞られる
- [ ] `sql` / `analyze` / `optimize` は変更しない（sql は生 SQL が脱出ハッチ、
  analyze は全歴史を取り込む、optimize は別節の命令）
- [ ] `--last` は `<N>d` / `<N>h` / `<N>w` を受け付け、now からのローリング
  ウィンドウを意味し、`--since` / `--until` と排他（clap conflict）
- [ ] `--until` は指定日の一日いっぱいまで含む（inclusive）。日付は既定の
  提示タイムゾーン（JST、bucket.rs と同じ規約）で解釈し、比較は UTC エポック
- [ ] ウィンドウ指定時、レポートは評価ウィンドウを明示する（freshness 行
  （stderr、全形式）、framing block（table のみ）、`--format json` には
  `window` を追加。既存 JSON キーは変えない）
- [ ] フラグ無しの各ビュー出力は変更前とバイト同一
- [ ] `docs/specs/cli.md` の Views 配下に期間フィルタの節を追記し、
  Reporting honesty の「Unused はウィンドウを明示」契約と整合させる
- [ ] `just check`（rustfmt + clippy -D warnings）と `cargo test` が green

## テスト戦略（t_wadaスタイル）

### E2Eテスト
- 合成ストアに対し `usage --since ... --format json` がウィンドウ内行だけを返す
- フラグ無しの views が変更前バイトと同一（既存の E2E 比較手順の踏襲）

### 統合テスト
- `Store` の範囲付き集計が `started_epoch` で正しく絞る（境界の含む/含まない）
- inventory は usage 半分だけが絞られ、カタログ（現在の設定）行は消えない

### 単体テスト
- 期間パース: `--since 2026-10-01` → JST 当日 00:00 の UTC エポック
- `--last 7d / 24h / 2w` のパースと now 注入による計算
- 境界値: 窓の両端、月末、UTC 21:00 は JST 翌日になる境界
- 例外: 不正な日付形式、不正な `--last` 形式、`since > until`、排他違反

## 実装アプローチ
- **Outside-In**: E2E（フラグ → 出力）から失敗を確認し、期間パース（単体）→
  Store 集計（統合）→ ビュー配線の順に Red-Green-Refactor
- 実装は subagent-driven-development スキル（implementer サブエージェント →
  task review → 最終 review）で実施する

## 見積もり
5 SP（台帳の E=2 日）

## 技術的考慮事項
- 依存関係: なし（`core::bucket` は既存だが、期間は別の型として保持する）
- テスタビリティ: 期間パースは純粋関数（I/O なし、now は注入）。Store は
  in_memory に合成データを入れて検証
- 非機能要件: WHERE 追加は full scan でも現行規模（イベント数万）で問題なし

## 実装者向け注記

### 現状コードの確認（2026-10-30 実施済み）
```bash
grep -rn '\-\-since\|--until\|--last\|TimeRange\|period_filter' src/
# マッチなし → 未実装。既存 11 コマンド
# （analyze / usage / inventory / waste / overhead / prompts / failures /
#   stuck / doctor / sql / optimize）に期間フラグは無い
```

### 実装手順
1. `src/core/` に期間型を追加: `--since` / `--until` / `--last` をパースし
   `(Option<i64> start_epoch, Option<i64> end_epoch)` を返す純粋関数。
   日付は `JST_OFFSET_SECS` で解釈。`--last` は now を注入して計算。
   `since > until` はエラー
2. clap に 3 フラグを 8 ビューへ追加。`--last` は `--since` / `--until` と
   `conflicts_with`
3. `Store` の集計クエリに範囲を渡す。WHERE 断片を組み立てるヘルパーを 1 つ
   作り、イベント集計は `started_epoch` で絞る。セッション集計は
   `started_at`（一様な RFC3339 UTC、`epoch_ms_to_rfc3339` 由来）のテキスト
   比較か events 経由の JOIN。全 query への個別分岐は避ける
4. 各ビュー出力にウィンドウを明示: freshness 行（stderr）、framing block
   （table のみ）、JSON の `window`。無指定時は表示を一切変えない
5. `docs/specs/cli.md` の Views 配下に期間の節を追記（spec-sync 規約:
   同一変更で更新）
6. `cargo test` → `just check` → フラグ無し出力のバイト同一確認

### 落とし穴
- `--until` は「その日いっぱい」まで含む。日付→エポック変換は必ず JST で
  行う（UTC 21:00 は JST 翌日になる境界）。bucket_label と同じ規約
- doctor の COST セクションなど「セッション数」を出す箇所は、セッションの
  絞り方（started_at 比較か events 経由か）で数が変わる。境界は単体テストで
  固定する
- inventory は「カタログ（現在）× usage（歴史）」の結合。絞るのは usage 半分
  だけで、カタログ行を消さない。unused の表示は「ウィンドウ内で未使用」と
  読める文言にする
- freshness 行・framing block への追加は piped stdout を壊さないこと。
  JSON は `window` を足すが既存キーは変えない

## Definition of Done
- [ ] 全 BDD シナリオが自動テストとして実装されパスする
- [ ] フラグ無し出力がバイト同一であることを確認
- [ ] `just check` と `cargo test` が green
- [ ] コードレビュー完了（task reviewer）
- [ ] docs/specs/cli.md 更新済み
