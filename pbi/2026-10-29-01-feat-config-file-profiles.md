# PBI: 設定ファイルとプロファイル切替

## ユーザーストーリー
マルチハーネスを使い分けるユーザーとして、既定のハーネスとストアを
設定ファイルに置き、`--profile` で切り替えたい。なぜなら毎回
`--harness` を打つ手間をなくし、環境別（仕事用・個人用など）の使い分けを
確実にしたいからだ。

## ビジネス価値
- 毎回のコマンド入力が短くなる（`--harness` 省略可）
- 環境の取り違え（仕事の分析を個人のストアに書く等）を構造的になくす
- 測定方法：設定ありで `saniwalens doctor` が `--harness` 無しに意図した
  ハーネスで動き、`--profile` 切替でストアが切り替わること

## BDD受け入れシナリオ

```gherkin
Scenario: 既定ハーネスが適用される
  Given ~/.config/saniwalens/config.toml に default_harness = "kilocode" とある
  When ユーザーがフラグ無しで saniwalens doctor を実行する
  Then kilocode 用のストアに対して分析・表示が行われる

Scenario: プロファイルでハーネスとストアを切り替える
  Given config.toml に [profiles.work]（harness = "claude"、db = "work.db"）がある
  When ユーザーが saniwalens --profile work doctor を実行する
  Then claude 用の分析が config.toml と同じ場所の work.db に対して行われる

Scenario: 存在しないプロファイル名は候補付きで失敗する
  Given config.toml に work しか定義されていない
  When ユーザーが saniwalens --profile play doctor を実行する
  Then 処理は行われず、利用可能なプロファイル一覧を含むエラーが表示される

Scenario: 壊れた設定ファイルは黙って無視しない
  Given config.toml が TOML として壊れている
  When ユーザーが saniwalens doctor を実行する
  Then 従来動作にフォールバックせず、エラーで終了する
```

## 受け入れ基準
- [ ] 設定ファイルが無い状態は従来とバイト同一の挙動（既存テストが保証）
- [ ] `--harness` / `--db` の明示指定は設定より常に優先される
- [ ] 相対 `db` は config.toml のあるディレクトリ基準で解決される（cwd 基準ではない）
- [ ] `~` は展開しない（書いたら文字通りに扱う）
- [ ] 未知のキーは無視される（将来の拡張で壊さない）
- [ ] `docs/specs/cli.md` に設定ファイル節が追記されている

## テスト戦略（t_wadaスタイル）

### E2Eテスト（最小限）
- 手動確認：合成 config + 一時ディレクトリで上記4シナリオを実バイナリで実行
  （本リポジトリは lib 単体テスト中心のため、E2E は手動検証＋記録とする）

### 統合テスト（中程度）
- 解決関数のテスト（一時ディレクトリ使用、環境変数に触らない）：
  flag×profile×既定値×無しの組合せ、相対 db 解決、異常系3種のエラー

### 単体テスト（多数）
- TOML パース：正常形、未知キー無視、型不正の検出
- 優先順位マージの純粋関数：flag > profile > 既定値 > ビルトイン
- 相対パス結合：config 配置ディレクトリ基準であること

## 実装アプローチ
- **Outside-In**：E2Eシナリオ（手動）で現状が無視することを確認 → 統合テスト（失敗）→ 単体テスト（失敗）→ 実装 → グリーン → リファクタ
- **Red-Green-Refactor**：TDDサイクルを各レイヤーで適用
- **リファクタリング**：グリーンになるたびに品質改善（既存 `store_path` / `resolve_db_from` の形に寄せる）

## 見積もり
3pt 目安（TDD・spec更新込み、要調整）

## 技術的考慮事項
- 依存関係：`toml` クレート追加（`serde` 連携済みのため最小）
- テスタビリティ：解決ロジックは純粋関数＋引数化した I/O 境界に切る（`resolve_db_from` と同型）
- 非機能要件：設定ファイルが無ければ従来動作（性能影響なし）

## 実装者向け注記

### 現状コードの確認（着手前に必ず実行すること）
```bash
# 設定ファイル関連の既存実装がないことを確認
grep -rn "config\.toml\|--profile\|default_harness" src/ docs/specs/cli.md
# 既存の解決関数の形を確認
grep -n "fn store_path\|fn resolve_db_from\|fn parse_harness" src/cli.rs
```

確認済み（2026-10-29）：`config.toml`・`--profile`・`default_harness` は未実装。
`XDG_CONFIG_HOME` の参照は harness 側設定の読み取りのみ。

### 実装手順
1. E2Eシナリオで現状が設定を無視することを確認する（手動・記録のみ）
2. 解決関数の統合テストを書く（失敗を確認：関数が存在しない）
3. パース・マージ・相対結合の単体テストを書く（失敗を確認）
4. 新規モジュールに実装する（TOML読み・解決）＋ `toml` 依存追加
5. `src/cli.rs` に `--profile` を足し、解決結果を `parse_harness`／`store_path` へ配線する
6. グリーン化 → リファクタ（既存の形に寄せる）
7. `docs/specs/cli.md` に設定ファイル節を追記する（同一変更内）

<!-- 手順は Outside-In（E2E→統合→単体→実装）の縦スライス順。 -->

### 落とし穴
- 相対 `db` の基準は **config.toml のあるディレクトリ**。cwd 基準にすると要件違反（最重要）
- clap の `--profile` は `--harness` と同じくグローバル指定にする
- 未知キーは `deny_unknown_fields` を付けない（無視が仕様）
- 決定済み事項の再議論禁止：形式は TOML、キーは harness＋db のみ、優先順位は flag > profile > 既定値 > claude

## Definition of Done
- [ ] 全BDDシナリオが検証済み（自動テスト＋手動E2E記録）
- [ ] テストカバレッジが基準を満たす（統合/単体すべて）
- [ ] コードレビュー完了
- [ ] リファクタリング完了（グリーン後）
- [ ] ドキュメント更新済み（`docs/specs/cli.md`、必要なら README）
