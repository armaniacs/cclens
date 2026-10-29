# PBI: 設定カタログ走査を単一IFに集約する

## ユーザーストーリー
saniwalensのメンテナとして、設定カタログの走査（global→project、
`seen` による symlink 重複排除）を1つのIFに集めたい。新 surface 追加が
3ファイル×2関数の改修になる現状では、受け渡し漏れの黙った二重計上が
構造的に起きるためだ。

## 優先度
- 順位: 05 / 6件
- RICEスコア: 0.8（Reach=4 / Impact=1 / Confidence=60% / Effort=3日）
- 根拠: 呼び出し側が `Scope` 構築・`seen` 所有・ループを担う seam リークは
  実在。ただし現状動いており、緊急性は低い。

## BDD受け入れシナリオ

```gherkin
Scenario: カタログ結果が変わらない
  Given symlink を含むテスト用設定ツリー
  When inventory を実行する
  Then surface一覧と重複排除結果が従来同一になる

Scenario: 読めないディレクトリの警告が保たれる
  Given 読めない設定ディレクトリ
  When analyze を実行する
  Then 従来どおりの警告が出て処理が続行する

Scenario: 新 surface 種別はデータ追加で足りる
  Given 新しい surface 種別の定義（配置・id規則・load mode）
  When カタログ走査に登録する
  Then 走査ロジックの改修なしに一覧に出る
```

## 受け入れ基準
- [ ] `catalog_surfaces(harness, roots)` 形式の単一IFができる
- [ ] `Scope` 構築と `seen` 所有が IF の内側に移る
- [ ] symlink 重複排除のテストが残る（fixture 付き）
- [ ] inventory/waste の実出力が従来同一

## テスト戦略
- E2E: 実バイナリで inventory/waste の diff（手動＋記録）
- 統合: symlink 付き fixture での重複排除テスト、読めない dir テスト
- 単体: id 規則、load mode 判定

## 見積もり
3日目安（要チームでの見積もり）

## 実装者向け注記

### 現状コードの確認
```bash
grep -n "fn read_global_surfaces\|fn read_project_surfaces\|session_roots\|seen" src/cli.rs src/adapter/config.rs src/adapter/opencode.rs src/adapter/kilocode.rs | head -30
```

### 実装手順
1. 現状カタログのゴールデンを固定する（fixture→期待一覧）
2. 走査 IF を定義し、global→project パスを内側へ一段ずつ移す
3. per-harness の site 定義をデータ化する
4. cli.rs 側の呼び出しを IF 呼び出し1行に縮める
5. フルテスト＋実出力のdiff確認

### 落とし穴
- `seen` の所有を移す際、二重計上テスト（symlink fixture）が必ず
  グリーンであることを先に確認する
- `.kilorules` や `kilo.json instructions[]` のような特殊 source を
  落とさないこと（種別ごとの読込テストで担保）

## Definition of Done
- [ ] 全BDDシナリオが自動テストとして実装されパスする
- [ ] コードレビュー完了
- [ ] ドキュメント更新済み（仕様変更があれば `docs/specs/config-format*.md`）
