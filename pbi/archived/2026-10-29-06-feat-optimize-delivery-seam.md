# PBI: optimize の配信手段を seam で差し替える

## ユーザーストーリー
saniwalensのメンテナとして、briefing の「作る」と「届ける」を分けて
配信手段（対話起動/headless/print）を差し替え可能にしたい。kilo の
承認ゲートのような配信 quirk が構成ロジックを汚すためだ。

## 優先度
- 順位: 06 / 6件
- RICEスコア: 0.7（Reach=2 / Impact=1 / Confidence=70% / Effort=2日）
- 根拠: 最小 blast radius。次の起動 quirk が来たときにやればよい明示的
  defer 対象。現状は動いている。

## BDD受け入れシナリオ

```gherkin
Scenario: claude への対話起動が変わらない
  Given briefing ができている
  When optimize を実行する
  Then 従来と同一の引数で claude が起動する（配信のモックで検証）

Scenario: headless 配信がログに流れる
  Given --format json での実行
  When headless で実行する
  Then raw ストリームが 0600 ログに残り、概要だけが表示される

Scenario: --print は標準出力に全文を出す
  Given --print 指定
  When optimize を実行する
  Then briefing 全文が標準出力に出る
```

## 受け入れ基準
- [x] `deliver(briefing, mode)` の seam ができる
  （`deliver(DeliveryPlan, &mut impl AdvisorLaunch)` が唯一の配信経路。
  plan が payload（print=全文 / launch=render_briefing＋ポインタ組立）と
  mode・launch 形状を運ぶ。0600 テンポラリの作成・削除タイミングもこの中）
- [x] Interactive/Headless/Print が adapter として差さる
  （`DeliveryMode` の3値と `AdvisorLaunch` trait（`direct` = stdio 引き継ぎ、
  `headless` = ログ付きパイプ）。本番は `ProcessAdvisor`、
  テストは記録モック `RecordingAdvisor` を差して argv を検証）
- [x] `launch_command` が配信選択に縮む（dead な `Option` が消える）
  （3ハーネスすべて Some だった Option を外しタプルを直接返す。
  到達不能だった kilo 拒否の bail とコメントを削除。
  モード選択は `delivery_mode(print, run_mode)` に集約し単体テストで固定）
- [x] `HarnessCopy` の field-by-field 構築が消える
  （`Harness::copy()` を adapter に移設、optimize は `sess.harness.copy()` の1行）

## テスト戦略
- E2E: 実バイナリで3モードの起動確認（対話は起動確認まで、手動＋記録）
- 統合: 配信モックに briefing を渡し、argv/ログ出力を検証
- 単体: モード選択ロジック（harness×flag）

## 見積もり
2日目安（要チームでの見積もり）

## 実装者向け注記

### 現状コードの確認
```bash
grep -n "fn launch_command\|fn run_headless\|RunMode\|fn write_private_tempfile" src/cli.rs | head
```

### 実装手順
1. 配信のテストダブルを書ける形に `deliver` IF を切る（RED）
2. 3 adapter を1つずつ移す（起動引数の同一性テスト付き）
3. dead な `Option` と field-by-field 構築を消す
4. フルテスト＋実起動の確認

### 落とし穴
- `--print` の temp ファイル経路（0600）と削除タイミングを変えないこと
- headless の `--auto` 付与条件（kilo の承認ゲート）を崩さないこと

## Definition of Done
- [x] 全BDDシナリオが自動テストとして実装されパスする
  （RED→GREEN: `the_print_delivery_writes_to_stdout_and_launches_nothing` /
  `the_interactive_delivery_hands_the_double_the_measured_launch`（従来同一の
  argv＋実行後もファイルが残らない）/ `headless_routes_through_the_logged_adapter_unless_verbose` /
  `the_delivery_mode_is_the_flag_over_the_launch_shape`。
  --print の出力全文は旧821bf1e バイナリとのE2E269行のバイト同一で確認。
  実バイナリでの対話/ヘッドレス実起動は手動確認＝「ユーザー作業」に分離）
- [x] コードレビュー完了（2026-10-30: 検証エビデンス — cargo test 391 passed / clippy -D warnings / 旧821bf1e とのE2E269行バイト同一 — をもって利用者確認済み）
- [x] ドキュメント更新済み（仕様変更があれば `docs/specs/cli.md`）
  （optimize 節の I/O 分界に deliver seam・DeliveryMode・AdvisorLaunch/
  ProcessAdvisor・Harness::copy を追記）
