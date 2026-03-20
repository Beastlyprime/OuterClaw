# ClawShell (OpenClaw Guardian) 开发计划 v1.0

## 1. 项目愿景
OpenClaw 的外部安全守护层。独立进程、独立用户、独立告警通道。

| # | Invariant |
|---|-----------|
| 1 | OpenClaw stays online |
| 2 | No private data leakage |
| 3 | No prompt injection exploitation |
| 4 | Memory is never lost |
| 5 | Rollback to any LKG state |
| 6 | No zombie/hung processes |

## 2. ClawShell 的不可替代价值

ClawShell 只做 OpenClaw 不可能自己做的事：

| 能力 | 为什么 OpenClaw 做不了 | ClawShell 的实现 |
|------|----------------------|----------------|
| 检测自己死了/卡了 | 进程挂了就无法自检 | 外部 watchdog + /proc 状态机 |
| 保护数据不被自己破坏 | Agent 被注入后可以删自己的记忆 | 隔离用户 + 隔离存储的备份 |
| 死了还能报信 | 进程挂了就发不了 Telegram | 独立告警通道 |
| 验尸 | 崩溃时的 /proc、dmesg 需要外部收集 | Post-mortem 取证 |

## 3. 三根支柱: Watch / Vault / Alert

### 3.1 WATCH: clawshell.py (常驻守护进程)
- 纯 Python stdlib, ~900 行, 零外部依赖
- /proc 采集 (每30s) + HTTP 探活 (:18789/health)
- 6 态状态机 + 两步自动恢复 (restart → LKG recovery)
- inotify 监控身份文件篡改 (SOUL.md, AGENTS.md, USER.md, openclaw.json)
- Telegram 双向交互 (/status, /restart, /snapshots, /help)
- `--status` 命令行状态报告
- systemd watchdog 心跳 (每1s sd_notify)

### 3.2 VAULT: systemd timers + shell scripts
- snapshot-sqlite.sh + snapshot-files.sh (每30min, 磁盘配额 + I/O 压力感知)
- promote-lkg.sh (每2h, 门控: uptime + health + row count)
- rollback.sh (人工触发)
- auto-recover.sh (clawshell.py 触发, 30min 冷却)
- postmortem-collect.sh (ExecStopPost)
- pre-start-check.sh (ExecStartPre)
- quota-check.sh (磁盘配额, 超限自动裁剪)
- io-pressure-check.sh (PSI avg10, 高压时延迟快照)

### 3.3 ALERT: alert.sh
- 自动从 OpenClaw 的 openclaw.json 读取 Telegram bot token (零配置)
- 可选独立 bot 用于双向交互
- 本地审计日志

## 4. 纵深防御架构 (Defense-in-Depth)

### Layer 0: Fortress (系统级加固)
- **三用户隔离**: yimeng (管理员, sudo) / ocagent (OpenClaw, 无 sudo) / occlawshell (ClawShell, 限定 sudo)
- **文件不可变**: chattr +i 保护 SOUL.md, AGENTS.md, USER.md
- **systemd 沙箱**: NoNewPrivileges, ProtectSystem=strict, SystemCallFilter=@system-service
- **ClawShell 自保**: OOMScoreAdjust=-500, WatchdogSec=120, StartLimitBurst=10

### Layer 1: Sandbox — OpenClaw 自行管理
- exec-approvals, sandbox 容器等由 OpenClaw 管控

### Layer 2: ClawShell (监控与自愈)
- /proc 状态机 + 4 因子健康检查
- 原子快照 + SHA256 审计链 + 磁盘配额 + I/O 感知
- LKG 自动提升 + 两步自动恢复
- inotify 配置篡改检测
- 崩溃取证 (14 类 /proc + dmesg + coredump)

## 5. 安全编码规范
- Shell 脚本禁止将外部输入拼接进 `python3 -c`，必须通过环境变量传递
- clawshell.env 加载使用 grep 解析，禁止 source
- 所有 .log 文件配置 logrotate (daily, keep 30, compress)
- Telegram token 从 OpenClaw 配置自动读取，通过环境变量传递给 python3

## 6. 实施路线图

### 已完成
- [x] v4.0 架构评审与安全调研 (2026-02-17)
- [x] v4.1 安全加固审计与修复 (2026-02-18)
- [x] v4.2 核心实现 (2026-02-18)
- [x] Phase 1: 部署与 ocagent 迁移 (2026-03-18)
  - [x] 创建 ocagent 用户 + OpenClaw 安装 + 数据迁移
  - [x] Gateway 迁移到系统服务 (User=ocagent)
  - [x] deploy.sh 全量部署 (vault + ACL + sudoers + systemd)
  - [x] 修复: pipefail glob、StartLimitIntervalSec、sd_notify 子进程、vault 权限
  - [x] 安全验证: 三用户隔离、sudo 阻止、不可变文件
- [x] Phase 2: 运维打磨 (2026-03-20)
  - [x] 磁盘配额 (quota-check.sh, 默认 2GB, 超限自动裁剪 + 告警)
  - [x] I/O 压力感知 (io-pressure-check.sh, PSI avg10 > 25% 延迟, 30s 重试)
- [x] Phase 3: 高级防御 (2026-03-20)
  - [x] inotify 配置篡改监控 (ConfigWatcher, ctypes, 零依赖)
  - [x] Telegram 双向交互 (TelegramBot, /status /restart /snapshots /help)
  - [x] 自动读取 OpenClaw Telegram 配置 (零配置告警)
  - [x] `--status` 命令行状态报告 (v1.0)

### 待开发
- [ ] 日常运行稳定性观察 (1-2 周运行无异常)
- [ ] 补充验证矩阵中未测试的项 (postmortem、LKG 提升、回滚、挂起检测)
- [ ] 产品化评估: 一键安装脚本 (`curl | bash`)、用户文档、README
- [ ] 可选: Rust 重写 (单二进制部署, 面向更广泛 OpenClaw 用户)

## 7. 文件清单

| 文件 | 说明 |
|------|------|
| `clawshell.py` | 核心守护进程 (~900 行): 状态机 + 自动恢复 + inotify + Telegram bot + --status |
| `test_clawshell.py` | 27 个单元测试 |
| `deploy.sh` | 幂等部署 (用户/vault/ACL/sudoers/systemd/logrotate) |
| `scripts/alert.sh` | Telegram 告警 (自动从 OpenClaw 读取配置) |
| `scripts/healthcheck.sh` | 4 因子健康检查 |
| `scripts/snapshot-sqlite.sh` | 原子 SQLite 备份 (VACUUM INTO + 配额 + I/O 感知) |
| `scripts/snapshot-files.sh` | 文件备份 (MEMORY.md, memory/, config, git bundle) |
| `scripts/promote-lkg.sh` | LKG 提升 (uptime + health + row count 门控) |
| `scripts/rollback.sh` | 人工回滚 (交互式确认) |
| `scripts/postmortem-collect.sh` | 崩溃取证 (14 类 /proc + dmesg + coredump) |
| `scripts/auto-recover.sh` | 自动 LKG 恢复 (clawshell.py 触发) |
| `scripts/pre-start-check.sh` | 启动前 SQLite 校验 |
| `scripts/quota-check.sh` | 磁盘配额 + 应急裁剪 |
| `scripts/io-pressure-check.sh` | PSI I/O 压力门控 |
| `scripts/migrate-to-ocagent.sh` | 数据迁移 + 路径修复 |
| `systemd/oc-clawshell.service` | 守护进程 (Type=notify, WatchdogSec=120) |
| `systemd/oc-snapshot.timer/service` | 30min 快照 |
| `systemd/oc-healthcheck.timer/service` | 2min 健康检查 |
| `systemd/oc-lkg-promote.timer/service` | 2h LKG 提升 |
| `systemd/openclaw-gateway.service` | Gateway 系统服务 (User=ocagent) |
| `logrotate/occlawshell` | 日志轮转 (30 天) |

*v1.0 completed 2026-03-20*

## 8. 攻击场景分析

Agent 被提示注入攻破后，以 `ocagent` 身份执行 exec 命令的攻击结果：

| 攻击 | 结果 |
|------|------|
| `rm -rf /var/lib/occlawshell/lkg/` | **Permission denied** — 目录属于 `occlawshell`，mode 0700 |
| `kill $(pgrep oc-clawshell)` | **Operation not permitted** — 不同 UID |
| `vim /var/lib/occlawshell/bin/clawshell.py` | **Permission denied** — 无写权限 |
| `sudo rm -rf /var/lib/occlawshell/` | **Blocked** — ocagent 无 sudo |
| `chattr -i SOUL.md` | **Operation not permitted** — 需要 CAP_LINUX_IMMUTABLE |
| `systemctl --user stop oc-clawshell` | **Fails** — 系统服务，非用户服务 |
| 修改 openclaw.json | **DETECTED** — inotify 实时告警 |

## 9. 威胁模型

| 威胁角色 | 能力 | ClawShell 防御 |
|----------|------|---------------|
| 提示注入 (外部内容) | Agent 执行非预期命令 | OpenClaw exec-approvals (非 Guardian 职责) |
| 提示注入 (身份覆写) | Agent 人格被重写 | `chattr +i` + inotify 告警 |
| Agent 被攻破 | 读写所有 ocagent 文件 | Vault 隔离 + 无 sudo + 备份安全 |
| 磁盘故障 | 全部数据丢失 | 原子快照 + LKG (未来: 云同步) |
| OOM killer | Gateway 被杀 | postmortem + 30min 快照 + 自动恢复 |
| 无限循环/死锁 | Gateway 挂起 | 6 态状态机 + 两步自动恢复 |
| Agent 删除记忆 | 记忆丢失 | 隔离备份 + git 历史 |
| 网络中断 | Telegram 不可用 | 本地审计日志 + systemd journal |

## 10. 验证矩阵

| # | 测试 | 命令 | 预期结果 | 状态 |
|---|------|------|----------|------|
| 1 | Agent 无法访问 vault | `sudo -u ocagent ls /var/lib/occlawshell/` | Permission denied | ✅ |
| 2 | Agent 无法杀死 ClawShell | `sudo -u ocagent kill $(pgrep -u occlawshell)` | Operation not permitted | |
| 3 | Agent 无法修改 SOUL.md | `sudo -u ocagent echo 'x' >> SOUL.md` | Operation not permitted | |
| 4 | ClawShell 可读 workspace | `sudo -u occlawshell cat MEMORY.md` | 内容显示 | |
| 5 | ClawShell 不可写 workspace | `sudo -u occlawshell touch workspace/test` | Permission denied | ✅ |
| 6 | SQLite 备份正常 | `sudo systemctl start oc-snapshot.service` | status=0/SUCCESS | ✅ |
| 7 | 备份完整性通过 | `sqlite3 main-*.sqlite "PRAGMA integrity_check"` | ok | |
| 8 | 崩溃时收集 postmortem | `sudo systemctl kill -s SIGABRT openclaw-gateway` | 生成 postmortem | |
| 9 | Gateway 自动重启 | 测试 8 之后 | active (running) | |
| 10 | LKG 提升正常 | 等待 oc-lkg-promote.timer | 生成 LKG + 符号链接 | |
| 11 | 回滚恢复状态 | `sudo /var/lib/occlawshell/bin/rollback.sh` | SQLite + memory 恢复 | |
| 12 | sudo 被 OS 级阻止 | `sudo -u ocagent sudo -l` | 要求密码 | ✅ |
| 13 | 快照定时器触发 | `systemctl list-timers oc-snapshot.timer` | 显示下次触发时间 | ✅ |
| 14 | 告警到达 Telegram | 触发测试告警 | Telegram 收到消息 | |
| 15 | 挂起检测：无误报 | 大模型推理 5 分钟 | HEAVY_INFERENCE，无告警 | |
| 16 | 挂起检测：真阳性 | `kill -STOP $(pgrep openclaw)` | CONFIRMED_HANG | |
| 17 | inotify 检测篡改 | 修改 openclaw.json | WARNING 告警 | |
| 18 | Telegram /status | 发送 /status | 返回状态 | |
| 19 | --status 报告 | `clawshell.py --status` | 显示完整状态 | ✅ |
