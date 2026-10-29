# PBI: 起動時解決を Session モジュールに集約する

## ユーザーストーリー
saniwalensのメンテナとして、起動時の解決（設定ファイル→フラグ→既定値→
ハーネス→ストアパス→旧ストア移行→open＋guard）を1モジュールにまとめたい。
なぜなら全コマンドの配線不具合（直近3欠陥の発生源）がここに集中しており、
今後のあらゆる変更を安くするためだ。

## 優先度
- 順位: 01 / 6件
- RICEスコア: 6.4（Reach=12 / Impact=2 / Confidence=80% / Effort=3日）
- 根拠: 最高スコア。依存なし。全11コマンド＋8 viewが恩恵を受ける。
  設定ファイル対応でこの層の配線が増えたばかりで、今やるのが最も安い。

## BDD受け入れシナリオ

```gherkin
Scenario: 既定起動が単一の呼び出しになる
  Given 設定ファイルが無い状態
  When 利用者が saniwalens doctor を実行する
  Then 従来とバイト同一の出力とストアが得られる

Scenario: 未知のプロファイルは候補付きで失敗する
  Given 設定ファイルに work だけが定義されている
  When 利用者が saniwalens --profile play doctor を実行する
  Then 処理は行われず、利用可能なプロファイル一覧を含むエラーが出る

Scenario: frozen読み取りでもguardが働く
  Given 別ハーネスのストアが存在する
  When 利用者が --frozen で別ハーネスを指定して読む
  Then 他ハーネスの行を表示せず、エラーメッセージが出る
```

## 受け入れ基準
- [x] 11のdispatch armが `Session::open` 以外の解決呼び出しを持たない
- [x] `store_path` / `resolve_db` / `migrate_legacy_store` / `config_file_path` /
  `parse_harness` / `open_for_read` / `guard_harness` の呼び出しが1箇所に集約される
  （arm からの解決呼び出しは `Session::open*` のみ。`run_analyze` 入口の
  guard は解析書き込み経路の防衛で、refactor 前から存在する own guard）
- [x] 既存テストは移動するだけで全て残る（削除は正当な理由がある場合のみ）
  （821bf1e 時点 366 → 現在 376、session.rs に移設済み）
- [x] 実出力がバイト同一（freshness行・エラー文言を含む）
  （旧 821bf1e バイナリ vs 現行を合成フィクスチャで29シナリオ・269行 diff、
  不正 --scope/--format/--profile/--harness・guard・readonly・HOME なし経路を含め同一）

## テスト戦略
- E2E: 実バイナリで doctor/analyze/inventory の代表コマンドを実行し、
  出力が従来と同一であることを確認（手動＋記録）
- 統合: 一時ディレクトリで `Session::open`（通常/frozen/旧ストア移行/設定あり無し）
- 単体: 既存の純粋関数テストを移設（resolve/merge/path解決）

## 見積もり
3日目安（要チームでの見積もり）

## 実装者向け注記

### 現状コードの確認
```bash
grep -n "resolve_db(db, &harness" src/cli.rs | wc -l   # 11 arms
grep -n "open_for_read(harness, db" src/cli.rs | wc -l  # 8 views
grep -n "^fn \(store_path\|resolve_db\|migrate_legacy_store\|config_file_path\|parse_harness\|open_for_read\|guard_harness\)" src/cli.rs
```

### 実装手順
1. `Session { harness, db_path, store }` 保持の新モジュールを用意する（Outside-In：まずE2Eで現状出力を記録）
2. 解決系の純粋関数を移す（テストごと移動、RED→GREENは移動差分ゼロで確認）
3. `run()` の dispatch 11 armを `Session::open` 呼び出しに置換する
4. 各 view の open/guard を `&Session` 受け取りに変える
5. フルテスト＋E2E出力のdiff確認

### 落とし穴
- `resolve_db` の `config_db` フォールバックと移行ガード条件（`db` と
  `config_db` 両方無しのときだけ移行）を崩さないこと
- frozen時の guard位置（open後にguardする現行動作）を変えないこと

## Definition of Done
- [x] 全BDDシナリオが自動テストとして実装されパスする
  （`resolve_cli_unknown_profile_lists_available` /
  `open_read_frozen_wrong_harness_is_refused` / Session 統合テスト群、
  既定起動のバイト同一は旧バイナリ E2E diff で確認）
- [x] テストカバレッジが基準を満たす（統合/単体すべて）（`cargo test` 376 passed）
- [ ] コードレビュー完了
- [x] リファクタリング完了（グリーン後）
  （dispatch の parse 前提順回復＋`Session::open_resolved` 分割）
- [x] ドキュメント更新済み（`docs/specs/cli.md` の解決節を Session 基準に）
  （`config_file_path` の所在を session.rs へ修正、フラグ解析順の契約を追記）
