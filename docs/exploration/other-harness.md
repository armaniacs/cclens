# feature-otherharness 探索メモ (opencode / kilocode / DSH 対応)

- ブランチ: `feature-otherharness` (このファイルが探索作業の成果物)
- 目的: cclens を Claude Code 専用から、opencode / kilocode / DSH でも使える構成にする
- 方針 (AGENTS.md 準拠): spec-first / TDD / privacy厳守のため、本メモに実データは貼らない。以下はすべて手元の実ディレクトリを read-only で読んだ shape の要約で、ID・パス・プロンプト文は合成例に置き換えている。
- 日付: 2026-10-25 JST

## 1. 現状の Claude Code 前提 (どこを剥がすか)

| 箇所 | Claude 前提の内容 |
| --- | --- |
| `src/adapter/transcript.rs` | JSONL の `type` (`assistant`/`user`/`system`)、`message.usage.*`、`message.content[]` の `tool_use`、`Skill`/`Agent` の特別扱い、`<command-name>`、 `toolDenialKind`、`subagents/` + `*.meta.json` 連携、`cwd` 先頭採用、`promptId` |
| `src/adapter/config.rs` | `~/.claude` (`$CLAUDE_CONFIG_DIR`) 配下の `skills/*/SKILL.md`、`rules/**/*.md`、`agents/*.md`、`settings.json`、`mcp.json`、プロジェクト側 `.claude/` + `CLAUDE.md`/`AGENTS.md` (+ `@import` 追跡) |
| `src/cli.rs` (`run_analyze` 周り) | `--projects` 既定 = `$CLAUDE_CONFIG_DIR/projects`、config root 既定 = `~/.claude`、project root 列挙 = sessions の `cwd` |
| `src/store.rs` | `sessions` に harness 列なし。`events.kind` は Claude 由来の語彙 (`skill_invocation`, `agent_spawn`, …)。`ANALYZER_VERSION = "3"` (抽出意味変更時は bump 必須) |
| `docs/specs/*` | `session-format.md` / `config-format.md` が Claude 形式を直接カタログ。`format-isolation.md` の「adapter のみが upstream 形式を知る」境界はそのまま活かせる |

## 2. 実測: 各ハーネスの入力 shape (read-only 調査)

### 2.1 opencode — 一番近い。SQLite が正本

- セッション正本: `~/.local/share/opencode/opencode.db` (SQLite, drizzle schema)
  - `session(id, project_id, directory, title, agent, model, tokens_input/output, cost, time_created/updated)`
  - `message(id, session_id, time_created, data JSON)` — `data.role` / `data.tokens{input,output,reasoning,cache{read,write}}` / `data.cost` / `data.modelID,providerID,agent,mode` / `data.path{cwd,root}` / `data.error{...}`。`parentID` でスレッド連結
  - `part(id, message_id, session_id, data JSON)` — `data.type` 分布 (実測約5000件): `tool` / `step-start` / `step-finish` / `text` / `reasoning` / `compaction` / `file` / `subtask`
  - `step-finish` が `tokens{total,input,output,cache}` + `cost` を持つ。compaction 検出は `type=compaction` が直接使える (Claude の `compact_boundary` 相当)
- ファイルミラー: `project/global/storage/session/{info, message}/...` に同内容の JSON があるが、DB を読む方が join が楽。`subagents/` 相当の分離はない (エージェント実行は `part.tool=task` / `part.type=subtask`)
- 使用シグナルの対応付け (案):
  - skill 呼び出し → `part.tool=skill` (`input.name` が skill 名) + `skill_use`。Claude の slash/tool 2経路に相当する区別は opencode にはないので `source=tool` 一本でよい
  - subagent spawn → `part.tool=task` / `part.type=subtask` (`agent`, `command`, `prompt` を保持)
  - tool 使用 → `part.tool=<name>` (`read`/`bash`/`edit`/`write`/`glob`/`grep`/`webfetch` …)。`callID` が `tool_use_id` 相当の join key
  - tool 失敗 → `part.state.status=error` + `state.error` (例: `File not found: /tmp/example/foo.md`)。Claude の `is_error`/`tool_use_error` ラッパーと同位置づけ。`input.filePath`/`command` が target になる
  - permission 拒否 → `opencode.json` の `permission` (allow/ask/deny) が静的根拠。実行時シグナルとしての denial marker は未確認 (要追加調査)
  - user prompt → `message.role=user` + `part.type=text`。`session.title` は要約文なので使わない
- コンフィグ面:
  - グローバル: `~/.config/opencode/opencode.json` (`model`, `small_model`, `permission`, `providers`, …) + `plugins/`。`command/`・`agent/` ディレクトリはこの環境では未存在 (プロジェクト規約 `.opencode/command/*.md` を使う流儀と合わせて要確認)
  - プロジェクト: カレントでは `.opencode/` / `opencode.json` を持つ repo は未確認。`AGENTS.md` は opencode も読む共有規約なので、既存の `claude_md` surface (symlink dedup 付き) を流用できる
  - MCP: `mcp` キー (opencode.json 内 or 別ファイル) を要確認。本環境の `opencode.json` トップレベルキーに `mcp` はなし

### 2.2 kilocode (Roo-code 系) — VSCode globalStorage 配下。tokens が最大の欠落

- セッション: `<VSCode globalStorage>/kilocode.kilo-code/tasks/<uuid>/`
  - `api_conversation_history.json` — Anthropic 互換: `{role, content:[{type:text|tool_use|tool_result, id/name/input, tool_use_id/content}], ts, reasoning_details?}`。`tool_use.name=execute_command` 等。`tool_use_id` 連結で tool と target が取れる (Claude と同形なので流用しやすい)
  - `ui_messages.json` — `{ts, type:say/ask, say:text/...}` の表示用ログ。人間ターンの区切り (span 終端条件) に使える
  - `task_metadata.json` — `files_in_context[]` (read/edited の痕跡)。never-fires-rule の将来材料
  - トークン量: `api_conversation_history.json` に `usage`/`tokens`/`cost` キーは 0 件 (実測)。`ui_messages` 側か別集計かの追加調査が必要。なければ `out_tokens/ctx_*` は NULL 運用になり、`usage`/`overhead` 系 view が harness 別に劣化表示になる
- コンフィグ面:
  - `settings/mcp_settings.json` (`{mcpServers:{}}` を確認)、`settings/custom_modes.yaml` (mode = 実質 agent/skill 定義)。グローバル skill dir (`~/.kilocode/skills.bak`?) は命名が不安定で要確認
  - プロジェクト側規約 (`.kilocode/rules/` 等) はこの環境で未確認。要追加調査
- パスの環境依存が強い (`~/Library/Application Support/Code/...` は VSCode/Cursor 設置依存) ので、adapter は `KILOCODE_STORAGE` 相当の環境変数 override を持たせたい

### 2.3 DSH (DeepSeek Harness) — 自前イベントソーシング。zstd 付き JSONL

- セッション: `~/.dsh/sessions/<cwd-slug>/<session-id>/session.v4.jsonl.zstd`
  - 1行1イベント `{type, seq, time, data}`。実測の型分布: `tool/call` / `tool/result` / `step/start` / `assistant/message` / `step/end` / `job/registered` / `job/settled` / `user/message` / `agent/inbox/spliced` / `session/title` / `session` / `permission/preset` / `sandbox/mode` / `approval/policy` / `turn/start` / `system/message` / `request/header` / `request/context` (+ `session/title-llm-request`)
  - 起点: `type=session` が `{version:4, id, createdAt, cwd, agentPreset}` を持つ。Claude の「先頭 `cwd` を root に」と同じことが確実に取れる
  - tool 呼び出し: `assistant/message` 内 `content[]` の `{type:tool-call, id, name, arguments}` + 対応する `tool/call{callId,name,arguments}` → `tool/result{content,isError}`。`callId` が join key。実測 tool 名: `bash`, `read`, `web_search` 等 (DSH 自前の動詞)
  - トークン: `usage{inputTokens,outputTokens,totalTokens,cacheReadTokens?}` を確認 (どの event に付くかは要特定。`assistant/message` の `source.replayState` 付近と `step/end` 付近を次に見る)
  - user prompt: `user/message{content[], source{kind:user}}`。Claude の `isMeta`/`tool_result` 除外と同等のガードが必要
  - subagent: `subagent/` 分離はなし。`delegationDepth` と `task` 系 tool (要特定) で spawn を検出する方針
- コンフィグ面: `~/.dsh/profiles/<name>/` (+ ワークスペースの `cordis.yml` 等)。skill/agent/MCP に相当する定義場所は未特定で要追加調査。`AGENTS.md` 共有規約は流用可

### 2.4 横断: 共有できるもの・harness 固有のもの

- 共有可: `AGENTS.md`/`CLAUDE.md` の always-on surface (symlink dedup・`@import` 追跡ごと)、`Scope` (global/project)、`approx_tokens` の static cost、catalog×usage の join・wedges・reporting-honesty の考え方
- harness 固有: セッションの「どこ・何形式」全文、skill 呼び出しの見分け方、subagent の連結 key、token フィールド名、config の置き場所、permission/denial の語彙。→ すべて adapter 配下に閉じ込める (format-isolation の拡張)

## 3. 設計案 (このブランチでは実装しない。合意後に spec 差分として起こす)

- `Harness`: `claude | opencode | kilocode | dsh` (+ `auto`)。`sessions` に `harness TEXT NOT NULL DEFAULT 'claude'` を additive migration で追加 (`store.rs` の宣言スキーマに列追加 = routine migration)。`surfaces` にも同列を持たせ、catalog×usage join を `(harness, kind, id)` + project shadowing に拡張。`ANALYZER_VERSION` を bump (既存行の再 ingest が必要になるため)
- adapter 分割: `adapter::{claude,opencode,kilocode,dsh}` + 共通の domain model (`Record`, `Surface`, `SubagentRun`) は不変。既存 `adapter::{config,transcript}` の中身は `adapter::claude::{config,transcript}` へ移動し、旧パスは re-export のみ残す (境界バグ防止)
- CLI: `analyze`/`doctor`/各 view/`optimize` に `--harness auto|claude|opencode|kilocode|dsh`。`auto` は存在する入力 root から検出。`--projects`/`--config` の既定値は harness 別テーブル化 (`~/.local/share/opencode/opencode.db`、`kilocode tasks dir`、`~/.dsh/sessions` 等)。`meta` に `harness_roots` を記録し、read 系の auto-refresh が同 root を再利用する
- tokens 欠落の扱い (kilocode): `out_tokens/ctx_*` NULL を許し、`usage`/`overhead` は「この harness では利用不可」表示 + `sql` では生行を参照可能に。`sub_tokens_estimated` と同列の reporting-honesty ラベルを追加する (spec `cli.md` 追記)
- `optimize`: 起動コマンドが harness 別 (`claude` / `opencode run` / ほか)。まず `--print` の briefing 統一、launch は Claude 互換のみ残して他は後回し

## 4. 難易度順と TDD の進め方 (案)

1. `sessions.harness` + `Harness` enum + 既存 Claude パスの移動 (リファクタのみ、全テスト green を維持)
2. opencode adapter (DB 読込 → domain records。`step-finish.tokens` で usage 系がそのまま動く = 最初に価値を出す)
3. DSH adapter (zstd JSONL デコード → records。`tool/call|result` join + `usage` 特定が山場)
4. kilocode adapter (records は取れるが tokens 欠落の劣化表示設計が本体)
5. config adapter (harness 別 catalog。`AGENTS.md` 共通部は共通化)
6. doctor/optimize の harness 対応 + `docs/specs/{session-format,config-format,storage,cli}.md` の harness 別追記

## 5. 追加調査が必要な項目 (open)

- [ ] opencode: `permission` 実行時 denial シグナルの有無、`mcp` 定義場所、`.opencode/command|agent` 規約の確定
- [ ] kilocode: トークン量の正本 (UI 側にあるか)、プロジェクト側 rules/modes 置き場所、storage root の決め方 (Cursor 併用時)
- [ ] DSH: `usage` を運ぶ event 型の特定、subagent spawn の見分け方、skill/agent/MCP 定義場所
- [ ] 全般: `--harness` の UX (`auto` の検出順、`--db` を harness 別に分けるか共用か)
