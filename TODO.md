# auditui TODO

此文件是 auditui 当前的"未做事项"显式清单。做完一条就删,别标 ~~strikethrough~~。

## 新功能 / skill

- [ ] **`dockit` skill**(名字待定): 扫 session → 把"重复教学"的东西分诊到正确载体(Makefile / deploy.sh / SKILL.md / memory / CLAUDE.md 等),而不是一味挖 memory。设计稿见本次会话
- [ ] **`youremember` 改进**: 改成 signal-extractor + adversarial review 两段式;prompt few-shot(现有 memory = 正例、Web-era 候选 = 反例);自动跳过 deprecated 架构候选(现在只是 prepend priming,效果一般)

## Memory 后续整理(dockit 要做的事)

- [ ] `project_youremember_design.md` → 删除,内容并入 `~/.claude/skills/youremember/SKILL.md`
- [ ] `reference_claude_mem_config.md` → 搬到 claude-mem 项目的 memory(不是 auditit 的)
- [ ] `reference_xserver.md` → 做成 `deploy.sh` / `~/.ssh/config` 注释
- [ ] `feedback_rust_release_default.md` → 做成 `Makefile`(`make` 默认跑 `cargo build --release`)
- [ ] `feedback_double_sync.md` → 做成 `deploy-xserver.sh`(rsync + ssh build 一把 all)
- [ ] `feedback_session_id_field_name.md` → 搬到新 skill `claude-session-lookup`(专门讲怎么查 Claude Code session log)

## Release / docs

- [ ] v0.1.0 release 跑完后:在 README 补**截图**(你说过自己要搞一个干净的仓库截图)
- [ ] README 里加 "download prebuilt binaries" 指向 GitHub Release 页
- [ ] 考虑开 CI workflow 做 `cargo check` on push(目前只在 tag 时跑 release)

## 父仓 `/Users/x/auditit/` 待收尾

- [ ] 父仓有 uncommitted 改动(server.py / web/index.html / AGENTS.md / tests/test_codex_rollout.py):收尾 commit 一次,打 tag `web-final` 封存,README 加一行指向 auditui

## Usage / quota 显示(deferred)

- [ ] **Codex weekly usage**(最易做): `~/.codex/sessions/**/*.jsonl` 最新一条 `token_count` event 的 `rate_limits.secondary` 就是 7 天窗口(`window_minutes=10080`),有 `used_percent` + `resets_at` + `plan_type`。Dashboard 或 Sessions 列表可以直接显示
- [ ] **Claude quota**:session transcript **没有** rate_limits 字段;`stats-cache.json` 过时且无 token/cost;需要用 `~/.claude/.credentials.json` OAuth token 调 Anthropic API 才能拿真实 quota
- [ ] **Qwen**:session 里只有 `contextWindowSize`(1M),没有 quota 概念,大概率不做

## 其他观察到的事

- [ ] 全局 `~/.claude/CLAUDE.md` 太长,准备拆分:
  - 核心操守(SMART / fail fast / 不编造)→ 留全局
  - lark-cli 用法 → 已有 `lark-doc` / `lark-shared` skill,全局段可删
  - Claude Code session log 查询步骤 → 变成 `claude-session-lookup` skill
