# この PR がすること: バイナリの版を git から刻み、keywords を汎用化

- ソースビルドの `saniwalens --version` が `0.0.0` プレースホルダを表示していた。
  新設の `build.rs` が `git describe --tags` を読み（先頭の `v` を除去）、
  コンパイル時に刻む。git が答えられない場所ではパッケージ版に
  フォールバックする。配布物だけでなくあらゆるビルドで版を名乗る。
- `keywords` のハーネス固有 `claude` を `coding-agent` に。次回 publish から有効。
