# この PR がすること: プラグイン機構の削除と製品文面の汎用化

改名の後始末です。改名作業で見つかった 2 件を片付けます。

- **Claude Code プラグイン機構は改名せず削除します。**saniwalens はマルチハーネスで、
  Claude Code は主戦場ではありません — Claude 専用ならオリジナルの cclens が担います。
  このリポジトリから Claude 専用プラグインを配るのは誤解を招くため、marketplace
  manifest、`plugins/cclens` パック、README のプラグイン節、不要になった Cargo の
  exclude 指定を削除します。
- **製品文面の Claude 前提を外します。**[AGENTS.md](AGENTS.md)、README の冒頭と対象範囲、
  クレート文書、flake の説明、architecture spec の一文を coding-agent 表現にします。
  Claude 固有の参照はあるべき場所に残します：Claude ハーネスの表示名・パス・
  アダプタ仕様、そして移行コードとテスト内の旧ストア名です。
