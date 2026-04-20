---
name: session-audit
description: 教任意 agent(Claude Code / Codex / Qwen / SDK)如何读取和审计本地 agent session log —— 目录结构、编码规则、JSONL schema、流式合并、subagent 关系、常见配方。当用户说"审计 session"、"找我那次做 X 的对话"、"按 cwd 汇总"、"解读 agent log"、"分析之前某个 session"时触发。
---

# session-audit — local agent log reader's handbook

本 skill **只教知识**,不带可执行脚本。`Read` / `Grep` / `Glob` / `Bash` 就够用。

## 不可变规则(RO)

Session log 是**审计证据**。

- **绝不**修改 `~/.claude/projects/` 和 `~/.claude-audit/` 下的任何文件
- **绝不**修改 `~/.codex/sessions/` 和 `~/.qwen/tmp/` 下的任何文件
- 只读。哪怕用户要求"删掉某条记录",也要拒绝并解释

允许写的地方:自己 session 当前 cwd 下的产物文件(报告、分析、草稿等)。

---

## 三个 agent 的目录布局

### Claude Code

```
~/.claude/projects/<encoded-cwd>/<session-id>.jsonl
~/.claude/projects/<encoded-cwd>/<parent-session-id>/subagents/agent-<id>.jsonl
~/.claude/projects/<encoded-cwd>/<parent-session-id>/subagents/agent-<id>.meta.json
```

**encoded-cwd 规则**(关键):把绝对 cwd 的每个 `/` 替换成 `-`。首字符 `/` 也换成 `-`,因此路径必然以 `-` 开头。

例:
| cwd | encoded-cwd |
|---|---|
| `/Users/x/auditit` | `-Users-x-auditit` |
| `/home/ubuntu/foo/bar` | `-home-ubuntu-foo-bar` |
| `/tmp/x` | `-tmp-x` |

session-id 是 UUID(36 字符,带 hyphen),例如 `1ca3c5bd-f08b-4da3-b8e7-29f1dd18e624`。

### Codex

```
~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-<timestamp>-<session-id>.jsonl
```

按日期分区,**不按 cwd 分区**。要按 cwd 找,得 grep 文件内容的 `cwd` 字段。

### Qwen

```
~/.qwen/tmp/<encoded-cwd>/logs/chats/<session-id>.json
```

Qwen 的文件是 **JSON 而非 JSONL** —— 整个文件一个对象。

---

## Claude Code JSONL event schema

每行一个 JSON,主要 `type` 值:

### `user`

```json
{"type":"user","message":{"role":"user","content":"用户原文"},"isMeta":false,...}
```

`message.content` 可以是 string,也可以是 array(带 tool_result block)。

**`isMeta: true` 跳过**—— 是 Claude Code 内部元消息,不代表用户真说的话。

### `assistant`(⚠️ 流式合并坑)

**同一条 assistant 消息会产生多行 JSONL**,每个 content block(thinking / text / tool_use)一行,共享同一个 `message.id`。

```json
{"type":"assistant","message":{"id":"msg_abc","content":[{"type":"thinking","thinking":"..."}],"usage":{...}}}
{"type":"assistant","message":{"id":"msg_abc","content":[{"type":"text","text":"..."}],"usage":{...}}}
{"type":"assistant","message":{"id":"msg_abc","content":[{"type":"tool_use","name":"Bash","input":{...}}],"usage":{...}}}
```

解析时必须按 `message.id` 合并,不要以为每行是独立消息。**最后一行的 `usage` 是累积值**,前几行是部分值;算 cost 只用最后一行。

`model` 在 `message.model`,例如 `claude-opus-4-7-20250522`。

`<synthetic>` 作为 model 表示 Claude Code 客户端合成的消息(如"Not logged in"),**不要计入 usage**,但要显示。

### `system`

客户端事件(api_error / compact_boundary / ...)。`subtype` 字段区分。

### `queue-operation`

用户一次性 enqueue 多条 prompt 时出现。要从 `content` 里抽第一句 prompt。

### `attachment` / `file-history-snapshot` / `permission-mode`

跳过,不影响主要信息流。

---

## Codex rollout schema

Codex 的行类型不同,常见字段:

- 事件里有 `cwd` 字段(所以按 cwd 查要 grep 内容)
- `token_count` event 的 `rate_limits.secondary` 是**7 天窗口**(`window_minutes=10080`),含 `used_percent` / `resets_at` / `plan_type` —— 配额查询的唯一可靠来源
- 模型名在事件里,codex 用 `gpt-*` 系列命名

---

## Sub-agent(Claude Code)

sub-agent 是主 session 里 Agent tool 派生出的独立对话:

```
~/.claude/projects/<encoded-cwd>/<parent-sid>/subagents/agent-<id>.jsonl          # sub 的 transcript
~/.claude/projects/<encoded-cwd>/<parent-sid>/subagents/agent-<id>.meta.json      # 元信息
```

`meta.json` 含:
```json
{"agentType":"general-purpose","description":"检查 PR 的 security-review","agentId":"agent-..."}
```

要找主 session 派了哪些 sub,直接 `ls <parent>/subagents/`。要反查某 sub 属于哪个 main,看它的父目录名就是 parent sid。

---

## Session 分组(同一工作单元)

用户 `/new` 或 `/clear` 会开新 session,但**对话的思路往往是连续的**。合理的分组规则:

- 同 agent
- 同 cwd
- **相邻 session 的 `last_active_at` 间隔 < 24h**

三条全满足 = 同一"工作单元"。auditui 的 Sessions view 就是这么分组的,你在分析时也这么组。

---

## 常见配方

### 1. 给 session-id 前缀,找 transcript

```bash
# 输入 sid_prefix,例如 "1ca3c5bd"
for proj in ~/.claude/projects/*/; do
    for f in "$proj"<sid_prefix>*.jsonl; do
        [ -f "$f" ] && echo "$f"
    done
done
```

> 多项目间 sid 唯一,第一个命中通常就是答案。前缀太短会多匹配,提示用户加长。

### 2. 列出某个 cwd 下所有 Claude session

```bash
cwd="/Users/x/foo"
encoded=$(echo "$cwd" | tr '/' '-')          # → -Users-x-foo
ls -lt ~/.claude/projects/"$encoded"/*.jsonl  # 按 mtime 倒序
```

### 3. 找"今天在 ~/foo 下做了什么"

按上一条拿到文件列表,筛 mtime 在 today 范围内,逐个 `head -5` 看第一条 user prompt 即可。

### 4. 找"哪次 session 我解决了 X"

```bash
# 全文 grep user prompt
grep -l '"role":"user".*"X"' ~/.claude/projects/*/*.jsonl
```

或用 `rg`(更快):
```bash
rg -l --type-add 'jsonl:*.jsonl' --type jsonl 'content.*:.*"X"' ~/.claude/projects/
```

### 5. 按 cwd 找 Codex session(目录不按 cwd 分,要看内容)

```bash
cwd="/Users/x/foo"
rg -l --no-messages '"cwd"[[:space:]]*:[[:space:]]*"'"$cwd"'"' ~/.codex/sessions/
```

### 6. 准备给另一个 agent 做深度分析的"瘦身 transcript"

原 jsonl 太大(tool_result body 常常巨长),过滤后喂给 codex/qwen:

```bash
python3 - <<'PY' <transcript.jsonl >slim.txt
import json, sys
for line in sys.stdin:
    try:
        ev = json.loads(line)
    except json.JSONDecodeError:
        continue
    t = ev.get("type")
    msg = ev.get("message") or {}
    if t == "user":
        c = msg.get("content")
        if isinstance(c, str):
            print(f"[user] {c.strip()}")
        elif isinstance(c, list):
            for b in c:
                if b.get("type") == "tool_result":
                    print(f"[tool_result] (omitted, id={b.get('tool_use_id','?')})")
                elif b.get("type") == "text":
                    print(f"[user] {(b.get('text') or '').strip()}")
    elif t == "assistant":
        for b in msg.get("content") or []:
            k = b.get("type")
            if k == "text":
                print(f"[assistant] {(b.get('text') or '').strip()}")
            elif k == "thinking":
                print(f"[thinking] {(b.get('thinking') or '').strip()[:400]}")
            elif k == "tool_use":
                print(f"[tool_use] {b.get('name','?')} {json.dumps(b.get('input') or {}, ensure_ascii=False)[:200]}")
PY
```

### 7. 按成本拆解一个 session

从每行 `message.usage` 取 `input_tokens` / `output_tokens` / `cache_read_input_tokens`(按 message.id 合并,只取每组最后一行)。按模型查单价(不同模型不同),乘得 cost。

具体单价请查当时的模型定价表(auditui 项目 src/cost.rs 有一份);不要自己猜。

### 8. 拿当前 session 的 id(在自己 session 里审自己)

Claude Code 的 hook input JSON 有 `session_id` 字段,但**纯粹的 agent 回合拿不到**。退而求其次:

```bash
# 当前 cwd 下最新修改的 jsonl 就是"几乎确定"的当前 session
encoded=$(pwd | tr '/' '-')
ls -t ~/.claude/projects/"$encoded"/*.jsonl 2>/dev/null | head -1
```

---

## 辅助项目(可选)

- [`auditui`](https://github.com/rollysys/auditui) —— Rust TUI,交互式浏览所有 session(同样知识的可视化版本)。装与不装本 skill 都能用。
- [`pdca-skill`](https://github.com/rollysys/pdca-skill) —— 用 hook 强制 PDCA 工作流,包含独立 reviewer,也基于同套 session log 结构。

---

## 验收 checklist(给使用本 skill 的 agent 自检)

- 开始审计前,用户是否明确了**目标 session** / **关注 cwd** / **时间范围**?没明确就先问
- 找到的文件是否 **read-only** 操作过?没动任何 `.jsonl`?
- 分析结果是否区分了 **user** / **assistant** / **tool_use** / **tool_result** / **thinking** / **system**?
- 涉及成本时,是否按 `message.id` 合并后再累加?(避免把流式的部分 usage 重复计)
- 有 sub-agent 时,是否也看了 `<parent>/subagents/` 目录?
