# auditui

[English](README.md) · [中文](README.zh.md)


**用 TUI 浏览 Claude Code / Codex / Qwen 编码 agent 的 session 日志。**

只读。无 hook、无 daemon、无网络请求。直接解析 agent 自己写的 transcript 文件,并行索引,聚合结果落盘缓存,给你一个单二进制 TUI 来看"到底发生了什么"。

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
- 不编辑。`auditui` 永远不会写 `~/.claude/`、`~/.codex/`、`~/.qwen/`
- 不分享 / 多用户。要别人能看到的 dashboard,这不是它
- 不上传分析数据。任何东西都不离开你的机器

## 安装

### 一行命令安装(推荐)

```bash
curl -fsSL https://raw.githubusercontent.com/rollysys/auditui/main/install.sh | bash
```

自动检测平台 → 从 GitHub 下载最新 release tarball → SHA-256 校验 → 装到 `~/.local/bin/auditui`。环境变量可覆盖:`PREFIX=/usr/local/bin`(改安装位置)、`TAG=v0.1.0`(锁版本)。

### 预编译二进制(手动下载)

去 [GitHub Releases](https://github.com/rollysys/auditui/releases) 页面下载。提供的目标平台:

- `aarch64-apple-darwin` — macOS Apple Silicon (M1/M2/M3/M4)
- `x86_64-unknown-linux-gnu` — Linux x86_64

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
```

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

## 缓存

`auditui` 把每个 session 的 timeline 缓存在 `~/.claude-audit/_tui_cache/<agent>/<sid>.bin`。按文件大小作 key,改动会自动检测。删掉这个目录可强制重建。

## 状态

Pre-1.0,迭代很快。支持 macOS + Linux (x86_64 + aarch64)。已验证:

- Claude Code(所有最近版本)
- Codex
- Qwen Code

## License

MIT — 详见 `LICENSE`。
