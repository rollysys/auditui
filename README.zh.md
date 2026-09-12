# auditui

[English](README.md) · [中文](README.zh.md)


**用 TUI 浏览 Claude Code / Codex / Qwen 编码 agent 的 session 日志。**

只读 transcript，无 hook、无 daemon。直接解析 agent 自己写的日志，提供单二进制 TUI。审计命令默认离线；只有显式配置并运行 `audit explain --execute` 才会发送脱敏后的分析载荷。普通 TUI 的更新检查可关闭。

```
┌ Sessions (235 groups · 4534 / 4534) ──────────┐┌ 1ca3c5bd · claude-opus-4-7 · ~/auditit · [3of8 [ ]] ──┐
│ ▶ CLA 04-19 14:03 ~/auditit   [8 sessions]    ││ 2026-04-19 14:03:29  USER                             │
│ ▼ CLA 04-18 19:40 ~/argus     [21 sessions]   ││ > 再加一个功能: Sessions 列表按会话组折叠展示          │
│   └ CLA 04-18 19:40  fix bar chart …          ││                                                        │
│   └ CLA 04-18 18:12  cost by sessions …       ││ 2026-04-19 14:03:32  ASSIS                            │
│   ...                                          ││ 让我先探查一下 transcript 里是否有线索 …             │
│ ▶ COD 04-17 09:22 ~/ArgusV4   [1105 sessions] ││                                                        │
└─────────────────────────────────────────────────┘└────────────────────────────────────────────────────────┘
 ↑/↓ 移动 · Enter 打开/折叠 · Space 展开 · [ ] 组内导航 · / 搜索 · Tab 切焦 · D dashboard · r 刷新 · q 退出
```

## 为什么做这个

编码 agent 会产生大量 transcript 文件 — `~/.claude/projects/<cwd>/<session>.jsonl`、`~/.codex/sessions/...`、`~/.qwen/tmp/<cwd>/logs/chats/...`。用几周后你会有几千个,横跨几十个 repo,而你没有办法:

- 找到**那一次**你解决了某个棘手问题的 session
- 看自己这周到底花了多少 token,按项目拆分
- 比较自己用 Claude / Codex / Qwen 的占比
- 不 `cat` 原始 JSONL 就读懂一段历史会话

`auditui` 解决这些。它**故意**做成 TUI 而不是 web app:

- **没有 server、没有端口、没有秘密泄漏到局域网**
- **没有 hook** — agent 自己写文件,`auditui` 只读
- **单一二进制** — 能跑在 SSH 远端、无显示主机、tmux 里,任何地方
- **够快** — 并行索引 + 落盘缓存,首次扫描后秒级刷新

## 功能

### Sessions 视图
- 按 `cwd + agent + 时间间隔 < 24 小时` 自动分组(自然的"我在做 X"工作单元)
- 可展开/折叠分组,单 session 组直接一行显示
- 全文搜索 transcripts(`/`)
- 实时 transcript 预览(user / assistant / tool_use / tool_result / thinking / system)
- 组内上一个/下一个(`[` / `]`)

### Dashboard
- 时间范围: 1h / 4h / 1d / 7d / 30d / all(范围内的成本,不是 lifetime)
- 单位切换(`u`): 美元 vs `calls/hr` — 本地大模型用 USD 没意义时就切到调用率
- 按 agent / model / session-group 聚合
- 按成本/调用率排名 top-20 组的横向柱状图(`v`)
- 时间轴折线(成本或调用次数)

### Memory / Skills 浏览
- 浏览所有 `CLAUDE.md`、`AGENTS.md`、`SKILL.md` 和 auto-memory 文件
- 按项目分组(最近修改在前)
- Markdown 渲染(颜色、加粗、列表、code block、表格)

### 不做的事
- 不改 transcript。不会写入 `~/.claude/`、`~/.codex/`、`~/.omp/`、`~/.qwen/` 等 agent 日志目录。
- 不提供多用户分享或网页服务器。
- 不自动上传分析。审计留在本机，除非显式执行已配置的模型解释。干预与任务结果写入独立的审计状态目录。

## 安装

### 一行命令安装(推荐)

```bash
curl -fsSL https://github.com/rollysys/auditui/releases/latest/download/install.sh | bash
```

自动检测平台 → 从 GitHub 下载最新 release tarball → SHA-256 校验 → 装到 `~/.local/bin/auditui`。环境变量可覆盖:`PREFIX=/usr/local/bin`(改安装位置)、`TAG=v0.1.0`(锁版本)。

> 安装脚本只访问 `github.com`(以及 release asset URL 会 302 跳转的 `objects.githubusercontent.com` CDN)。**不**访问 `raw.githubusercontent.com` 和 `api.github.com` — 这两个域在很多公司网络里被封。

### 预编译二进制(手动下载)

去 [GitHub Releases](https://github.com/rollysys/auditui/releases) 页面下载。提供的目标平台:

- `aarch64-apple-darwin` — macOS Apple Silicon (M1/M2/M3/M4)
- `x86_64-unknown-linux-musl` — Linux x86_64(静态链接;任意 glibc 版本均可运行,含老发行版)

> **Intel Mac (x86_64-apple-darwin)**: 请用下面的源码编译方式 — GitHub Actions 的 Intel runner 已 deprecated,不再发布预编译产物。

### 从源码编译

```bash
git clone https://github.com/rollysys/auditui
cd auditui
cargo build --release
./target/release/auditui
```

单一约 5 MB 静态二进制,可以拷到任何位置:

```bash
cp target/release/auditui ~/.local/bin/
```

## 用法

```bash
auditui                  # 启动 TUI

auditui --dry-run        # 显示 session 数量(健全检查)
auditui --bench          # 对所有时间范围跑一次 dashboard 计算并计时
auditui --memory-dump    # 列出找到的 memory + skills 文件
auditui --group-dump     # 显示 session 分组直方图
auditui --check-update   # 去 GitHub 查有没有新版,打印结果
```

### 审计账本与改进闭环

`auditui audit` 是独立于普通 Sessions/Dashboard 的入口，旧 `--audit` 已移除。
新账本按 **LLM 请求和 usage 观测**计成本，不把工具调用次数当成美元：

```bash
auditui audit costs --since 7d
auditui audit candidates --root ./transcripts --since all --json
auditui audit costs --since 2026-09-01T00:00:00Z --until 2026-09-08T00:00:00Z
auditui audit explain --root ./transcripts --since all           # 离线预览
auditui audit explain --root ./transcripts --config explain.json # 预览 + 预算报价
auditui audit explain --root ./transcripts --config explain.json --execute
auditui audit record --file intervention.json
auditui audit outcome --file outcome.json
auditui audit compare --intervention reduce-recovery
auditui audit browse --root ./transcripts --since all
make audit ARGS="--since 7d --json"
```

参数严格校验：未知或重复参数、缺值、未知 agent、倒置或空时间范围都会报错，
不会悄悄扩大扫描范围。costs/candidates/explain/browse 支持 `--root`、
`--since 7d|30d|all|RFC3339`（默认 `30d`）、`--until RFC3339`、
`--project PROJECT`（精确匹配）及 `--agent claude,codex,omp`。
起点包含、终点排除，按 **usage 观测时间**而非会话文件修改时间筛选。
缺时间戳的观测在有限窗口内排除并告警，`all` 则保留。
非交互命令支持 `--json`；browse 必须在真实终端运行。
compare 支持 root/project/agent 筛选，但**拒绝 `--since`/`--until`**：
比较必须包含完整任务及失败尝试，不能截断成本窗口后充当完整任务。

**如何读结果。** 供应商实际报告金额、价格表估算金额、成本未知观测数分栏，
不能把前两栏相加后叫“实际账单”，未知也不等于零。Coverage 会披露缺 usage、
缺时间戳、解析失败、未支持格式和归属不明等局限；没有诊断不保证账单完整。
候选将已观察到的恢复链、上下文增长、跨会话多操作序列与假设分开。
候选关联请求成本**不是可避免成本，更不是预计节省**；一个工具批次也不是一次 LLM 请求。

**支持路径与扫描方式。** 审计支持 `~/.claude/projects` 下 Claude JSONL、
`~/.codex/sessions` 下 Codex JSONL、`~/.omp/agent/sessions` 下 oh-my-pi JSONL，
包含嵌套子会话。Qwen/Hermes 可用普通浏览器查看，但不属于审计账本支持源。
`--root` 可指定本地源文件或目录树，替代默认 home 路径。目录里的 `manifest.json`
可显式列源、项目、父子关系及逻辑任务元数据：

```json
{
  "sources": [
    {"path": "before/session.jsonl", "provider": "omp", "project": "demo", "task_id": "task-before", "work_type": "bugfix"},
    {"path": "before/child.jsonl", "provider": "omp", "project": "demo", "parent": "before/session.jsonl", "task_id": "task-before", "work_type": "bugfix"},
    {"path": "after/session.jsonl", "provider": "omp", "project": "demo", "task_id": "task-after", "work_type": "bugfix"}
  ]
}
```

Manifest 路径相对 root，越界会拒绝。每次审计命令和 browse 刷新都会**读取完整源内容**，
以 SHA-256 校验独立的解析账本缓存后才复用；不复用普通 timeline cache，也不是
按字节偏移追加的增量索引。证据引用携带整个源文件 SHA-256 版本；文件改过，
即使大小不变，也必须重新扫描才能打开证据。

**本机证据与隐私。** JSON 默认隐藏本机路径并脱敏凭证，不导出工具参数正文或原始代码。
需要可回放的本机 SourceRef 时，显式导出敏感路径：

```bash
auditui audit candidates --root ./transcripts --since all --json --include-paths > local-candidates.json
jq '.candidates[0].evidence[0]' local-candidates.json > source-ref.json
auditui audit evidence --file source-ref.json          # 脱敏摘要，验证版本
auditui audit evidence --file source-ref.json --raw    # 显式本机原始记录
```

请选实际存在的非空候选/证据。`--include-paths` 仅支持 costs/candidates/compare，
会暴露敏感文件系统路径，这类导出应只保存在本机；不开放正文，也不改变模型解释载荷。
`--raw` 只用于本机证据查看。不要分享原始证据，也不要把启发式脱敏当成绝对保密保证。

**可选模型解释。** 不带 `--execute` 时仅显示脱敏载荷，不读 API 凭证、不调用模型。
带可选 `--config PATH` 时，预览还会校验端点、模型与预算。
执行必须提供 ExplainConfig JSON，字段如下：

```json
{
  "endpoint": "http://127.0.0.1:8080/v1/chat/completions",
  "model": "your-local-model",
  "api_key_env": "AUDIT_EXPLAIN_API_KEY",
  "max_cost_usd": 0.05,
  "input_usd_per_million": 1.0,
  "output_usd_per_million": 2.0,
  "max_output_tokens": 1000,
  "allow_remote": false
}
```

示例模型与单价必须换成实际端点配置。密钥放在指定环境变量，不放 JSON。
远程端点还必须 HTTPS、`allow_remote: true`，且命令显式 `--execute`。
模型只接收脱敏候选事实与证据 ID，**不读原始源记录、不能执行工具**。
请求前按输入字节和输出 token 上界保守检查预算。分析本身的 usage/费用及缓存
单独记入状态目录，不掺进被审计工作成本；缺 usage 是未知而非免费。
所有 audit 命令都不会触发普通 TUI 的更新检查。
模型文案是**未经事实验证的解释/假设**，不是账本真值：程序校验结构和证据引用，
不把建议或事实性文字视为已核实。不能拿模型写的数字充当实测成本或因果节省。

**持久化 PDCA。** explain/compare/record/outcome/browse 支持
`--state-dir PATH`，默认 `~/.claude-audit/ledger-state`，与可删的 TUI 缓存分离。
状态路径不能位于源 root 或 agent 数据目录中（包含解析后的别名），请用独立私有目录。
record/outcome 每次导入一个 JSON 对象。干预记录：

```json
{
  "id": "reduce-recovery", "candidate_id": "ID_FROM_CANDIDATES", "project": "demo",
  "work_type": "bugfix", "description": "采用经过检查的输入契约",
  "artifact": "commit 或 skill 引用", "effective_ms": 1788825600000,
  "status": "candidate", "quality_criteria": "验收通过且无质量退化"
}
```

状态为 `candidate`、`confirmed`、`implemented`、`pending_validation`、
`effective`、`ineffective`、`insufficient_evidence`，跟踪器会检查状态迁移。
TaskOutcome 显式标注逻辑任务、**所有会话尝试（包括失败与子会话）**、
外部验收质量和对照元数据：

```json
{
  "task_id": "task-before", "session_ids": ["LEDGER_SESSION_ID"], "passed": true,
  "quality_notes": "验收检查通过", "model": "observed-model",
  "harness_version": "harness-version", "project_version": "commit-id", "cohort": "before"
}
```

将对应 `after` 任务作为另一条结果导入。session_ids 用 cost JSON 中的真实账本 ID，
不是文件名。任务/work_type 由 manifest 显式提供，不猜用户轮次边界，
也不从最后一条 assistant 文案推断成功。比较输出前后样本、分布、失败尝试、
每完成任务对应的全队列成本，以及质量/可比性门槛和混杂因素。
资料缺失或不可比时给 `insufficient_evidence`，不伪造因果节省。
如果 source 筛选漏掉必须的会话，就无法成立完整任务比较。

**审计 browse 快捷键。** `Tab` / `Left` / `Right` 切换 Costs、Candidates、Effects；
`Up`/`Down` 或 `j`/`k` 选行，`Home`/`End` 跳到首尾；`Enter`/`e` 打开版本校验后的
脱敏证据，`[`/`]` 换证据，**大写 `R`** 显式显示本机原始记录，`Esc` 返回。
`PgUp`/`PgDn` 滚详情，证据打开后 `j`/`k` 也滚动；`r` 全量重扫，`q`/`Ctrl-C` 退出。
Effects 使用选中 root/project/agent 的完整任务历史，不受 Costs/Candidates
时间窗口截断。普通 TUI 及下面的快捷键保持不变。

### 自动检查更新

TUI 启动后会在后台线程去拉 GitHub `releases/latest`,每 24h 最多一次,顶栏出现小黄色 `↑ vX.Y.Z` 提示有新版可装。缓存在 `~/.auditui.json`。设环境变量 `AUDITUI_NO_UPDATE_CHECK=1` 可完全关闭(不拉网、不写缓存)。

### 快捷键

| 视图 | 键 | 动作 |
|------|-----|--------|
| 全局 | `S` / `D` / `M` / `K` | Sessions / Dashboard / Memory / Skills |
| 全局 | `f` | 切换 agent 筛选(all / claude / codex / qwen) |
| 全局 | `p` | 切换 scripted-session 筛选(SDK/headless) |
| 全局 | `r` | 重新索引 + 失效缓存 |
| 全局 | `q` / Ctrl-C | 退出 |
| sessions | `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | 移动 |
| sessions | `Enter` | 打开 session(在分组头上则切换展开) |
| sessions | `Space` | 在光标处展开/折叠分组 |
| sessions | `[` / `]` | 同组内上一个/下一个 session |
| sessions | `Tab` | 列表与详情之间切换焦点 |
| sessions | `/` | 全文搜索 transcripts |
| sessions | `z` | 切换折叠模式:smart(默认,长 tool_result / system / thinking 折叠)↔ 全展开 |
| sessions | `x` | smart 模式下,切换光标所在 event 的折叠状态 |
| dashboard | `←` / `→` | 切换时间范围 |
| dashboard | `u` | 切换单位:`$` ↔ `calls/hr` |
| dashboard | `v` | 总览 ↔ 分组柱状图 |

## 数据来源

`auditui` 只读以下位置:

| Agent | Transcripts | Memory | Skills |
|-------|-------------|--------|--------|
| Claude Code | `~/.claude/projects/<encoded-cwd>/<sid>.jsonl` | `~/.claude/CLAUDE.md`、项目 `CLAUDE.md`、`.../memory/*.md` | `~/.claude/skills/<name>/SKILL.md` |
| Codex | `~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-*.jsonl` | `~/.codex/AGENTS.md`、`~/.codex/rules/default.rules` | `~/.codex/skills/<name>/` |
| Qwen | `~/.qwen/tmp/<encoded-cwd>/logs/chats/<session>.json` | `~/.qwen/settings.json`、`~/.qwen/output-language.md` | `~/.qwen/skills/<name>/` |
| oh-my-pi | `~/.omp/agent/sessions/<encoded-cwd>/<ts>_<uuid>.jsonl` | `~/.omp/agent/memories/<encoded-cwd>/*.md` | `~/.omp/agent/memories/<encoded-cwd>/skills/<name>/` |

## 缓存

普通 Sessions/Dashboard 把每个 session 的 timeline 缓存在
`~/.claude-audit/_tui_cache/<agent>/<sid>.bin`，按文件大小作 key。
审计账本则读取完整源内容，以 SHA-256 校验
`~/.claude-audit/_tui_cache/ledger` 下的解析缓存后才复用；同大小修改也会失效，
这不是按字节偏移追加的增量索引。删除可丢弃缓存可强制重解析。
缓存包含本机源身份/路径，是私有本机文件，不是可直接分享的脱敏导出。
干预/结果记录、解释分析费用及缓存位于 `--state-dir`，删除 `_tui_cache` 不会清掉这些记录。

## 状态

Pre-1.0,迭代很快。支持 macOS + Linux (x86_64 + aarch64)。已验证:

- Claude Code(所有最近版本)
- Codex
- Qwen Code

## License

MIT — 详见 `LICENSE`。
