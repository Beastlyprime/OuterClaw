# ClawShell (OpenClaw 安全卫士) 开发计划 v4.2

## 1. 项目愿景
构建一个 OpenClaw 安全卫士，确保 1. OpenClaw 系统的稳定性和安全性 2. 本地系统文件、用户隐私信息的安全

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

不做的事（交给 OpenClaw 或 systemd）：
- exec 策略/审计 → OpenClaw 内建 exec-approvals
- 进程重启 → systemd Restart=always
- 定时快照 → systemd timer
- 提示注入防御 → OpenClaw SOUL.md + AI 层

## 3. 三根支柱: Watch / Vault / Alert

### 3.1 WATCH: clawshell.py (常驻守护进程)
- 纯 Python stdlib, ~460 行, 零外部依赖
- /proc 采集 (每30s)
- HTTP :18789/health 探活
- 6 态状态机: UNKNOWN → HEALTHY → HEAVY_INFERENCE → POSSIBLE_HANG → CONFIRMED_HANG / ZOMBIE / DOWN
- 状态变化 → alert.sh 通知
- 每 30s 写 `gateway-proc-latest.json` 供 postmortem 使用
- systemd watchdog 心跳 (每1s sd_notify)

### 3.2 VAULT: systemd timers + shell scripts
- snapshot-sqlite.sh (每30min, oc-snapshot.timer)
- snapshot-files.sh (每30min, oc-snapshot.timer)
- promote-lkg.sh (每2h, oc-lkg-promote.timer, 自带门控)
- rollback.sh (人工触发)
- postmortem-collect.sh (进程死亡时 ExecStopPost)

### 3.3 ALERT: alert.sh
- Telegram (独立 bot, 不依赖 OpenClaw)
- 本地审计日志

## 4. 纵深防御架构 (Defense-in-Depth)

### Layer 0: Fortress (系统级加固)
- **三用户隔离**: yimeng (人类管理员, 有 sudo) / ocagent (运行 OpenClaw, 无 sudo) / occlawshell (运行 ClawShell, 限定 sudo: 仅 restart gateway + auto-recover)
- **文件不可变**: chattr +i 保护身份文件 SOUL.md, AGENTS.md, USER.md（运行时配置 openclaw.json 不锁定）
- **systemd 沙箱**: NoNewPrivileges, ProtectSystem=strict, SystemCallFilter=@system-service
- **ClawShell 自保**: OOMScoreAdjust=-500, WatchdogSec=120, StartLimitBurst=10

### Layer 1: Sandbox (行为管控) — OpenClaw 自行管理
- exec-approvals.json 由 OpenClaw 自身管理，不属于 Guardian 职责范围
- Guardian 不部署、不锁定 exec-approvals.json，避免影响 OpenClaw 正常使用和更新

### Layer 2: ClawShell (监控与自愈)
- 4 因子健康检查 (healthcheck.sh, 每2min)
- /proc 状态机 (clawshell.py, 每30s)
- 原子快照 + SHA256 审计链
- LKG 自动提升 (≥30min 健康 + 完整性校验)
- 崩溃取证 (14 类 /proc + dmesg + coredump 数据)

## 5. 安全编码规范
- Shell 脚本禁止将外部输入拼接进 `python3 -c`，必须通过环境变量传递
- clawshell.env 加载使用 grep 解析，禁止 source
- 所有 .log 文件配置 logrotate (daily, keep 30, compress)

## 6. 实施路线图

### 已完成
- [x] v4.0 架构评审与安全调研 (2026-02-17)
- [x] v4.1 安全加固审计与修复 (2026-02-18)
  - [x] ~~exec-approvals.json 重构~~ (已移出 Guardian，由 OpenClaw 自行管理)
  - [x] healthcheck.sh 注入漏洞修复
  - [x] oc-clawshell.service 自保加固 (Watchdog/OOM/Syscall)
  - [x] deploy.sh 扩展 chattr +i 覆盖范围
  - [x] alert.sh source 注入修复
  - [x] promote-lkg.sh 运行时长验证
- [x] v4.2 核心实现 (2026-02-18)
  - [x] ~~exec-approvals.json 重写~~ (已移出 Guardian，由 OpenClaw 自行管理)
  - [x] clawshell.py 实现 (外部 watchdog, /proc 状态机, 6 态分类)
  - [x] oc-lkg-promote.timer/service 创建 (每2h 自动 LKG 提升)
  - [x] deploy.sh 更新 (clawshell.py 部署 + logrotate 部署)
  - [x] `logrotate/occlawshell` 配置
  - [x] oc-clawshell.service StartLimit 防无限重启
  - [x] .gitignore 扩展追踪所有项目文件

### Phase 1: 部署与 ocagent 迁移 (2026-03-18)
- [x] 创建 ocagent 用户，安装 OpenClaw
- [x] 迁移数据：记忆、身份、credentials、config（migrate-to-ocagent.sh）
- [x] 修复 openclaw.json 路径（/home/yimeng/ → /home/ocagent/）
- [x] 安装全局 Node.js（NodeSource v24），解决 ocagent 无 nvm 问题
- [x] 执行 `deploy.sh` 完成全量部署（创建 occlawshell 用户 + vault + ACL + sudoers）
- [x] 手动创建 `start-gateway.sh`（deploy.sh 未检测到 ocagent 的 openclaw 路径，已修复）
- [x] 迁移 gateway 到系统服务（User=ocagent，替代 yimeng 用户服务）
- [x] 修复 vault 目录权限（0711 → 0700）和 ACL（occlawshell 读取 ocagent 数据）
- [x] 修复 oc-clawshell.service（StartLimitIntervalSec 移至 [Unit] 段）
- [x] 修复 snapshot-sqlite.sh pipefail 问题（ls glob 空匹配 + set -e 导致脚本退出）
- [x] 验证 oc-clawshell.service 运行正常
- [x] 验证 oc-snapshot/oc-healthcheck/oc-lkg-promote 定时器正常
- [x] 验证 gateway 系统服务运行正常（ocagent doctor 通过，Telegram 连接正常）
- [x] 修复 clawshell.py sd_notify 子进程继承警告（pop NOTIFY_SOCKET from env）

### Phase 1b: 待验证安全项
- [x] 验证隔离: `sudo -u ocagent ls /var/lib/occlawshell/` → Permission denied
- [x] 验证无 sudo: `sudo -u ocagent sudo -l` → 要求输入密码（ocagent 无密码，无法提权）
- [x] 验证 ClawShell 不可写: `sudo -u occlawshell touch /home/ocagent/.openclaw/workspace/test` → Permission denied

### Phase 2: 运维打磨 (2026-03-20)
- [x] /var/lib/occlawshell 磁盘配额限制（quota-check.sh，默认 2GB，超限时自动裁剪 + 告警）
- [x] I/O 压力感知快照调度（io-pressure-check.sh，PSI avg10 > 25% 时延迟快照，30s 重试）

### Phase 3: 高级防御 (2026-03-20)
- [x] inotify 监控核心配置变更并自动告警（ConfigWatcher，ctypes 实现，零依赖，实时检测）
- [x] Telegram bot 双向交互（TelegramBot，getUpdates 长轮询，支持 /status /restart /snapshots /help）

## 7. 文件清单

| 文件 | 状态 | 说明 |
|------|------|------|
| `clawshell.py` | 更新 | 外部 watchdog, ~460 行, 自动恢复 + sd_notify 修复 |
| `deploy.sh` | 更新 | ocagent 路径检测 + home 目录 ACL |
| `scripts/snapshot-sqlite.sh` | 修复 | pipefail glob 空匹配问题 |
| `scripts/snapshot-files.sh` | 修复 | 同上 |
| `scripts/promote-lkg.sh` | 修复 | 同上 |
| `scripts/migrate-to-ocagent.sh` | 新 | 数据迁移 + 路径自动修复 |
| `scripts/auto-recover.sh` | 不变 | 自动 LKG 恢复 |
| `scripts/pre-start-check.sh` | 不变 | 启动前 SQLite 校验 |
| `scripts/alert.sh` | 不变 | 独立告警通道 |
| `scripts/healthcheck.sh` | 不变 | 4 因子健康检查 |
| `scripts/rollback.sh` | 不变 | 人工回滚 |
| `scripts/postmortem-collect.sh` | 不变 | 崩溃取证 |
| `logrotate/occlawshell` | 不变 | 审计日志轮转 |
| `systemd/oc-clawshell.service` | 修复 | StartLimit 移至 [Unit] 段 |
| `systemd/oc-lkg-promote.*` | 不变 | 每2h LKG 提升 |
| `systemd/oc-snapshot.*` | 不变 | 快照调度 |
| `systemd/oc-healthcheck.*` | 不变 | 健康检查调度 |
| `systemd/openclaw-gateway.service` | 不变 | Gateway 系统服务 (User=ocagent) |

*v4.2 implementation completed 2026-02-18*
*v4.2.1 deployment + ocagent migration completed 2026-03-18*

## 8. 攻击场景分析

Agent 被提示注入攻破后，以 `ocagent` 身份执行 exec 命令的攻击结果：

| 攻击 | 结果 |
|------|------|
| `rm -rf /var/lib/occlawshell/lkg/` | **Permission denied** — 目录属于 `occlawshell`，mode 0700 |
| `kill $(pgrep oc-clawshell)` | **Operation not permitted** — 不同 UID，ocagent 无法向 occlawshell 进程发信号 |
| `vim /var/lib/occlawshell/bin/clawshell.py` | **Permission denied** — 无写权限 |
| `sudo rm -rf /var/lib/occlawshell/` | **Blocked** — ocagent 没有 sudo 权限（OS 级阻止，无需依赖 exec-approvals） |
| `chattr -i SOUL.md` | **Operation not permitted** — 需要 CAP_LINUX_IMMUTABLE (仅 root) |
| `systemctl --user stop oc-clawshell` | **Fails** — oc-clawshell 是系统服务，非用户服务 |

关键改进：`sudo` 被 OS 级阻止（ocagent 不在 sudoers 中），无需依赖应用级管控。

## 9. 威胁模型

| 威胁角色 | 能力 | ClawShell 防御 |
|----------|------|---------------|
| 提示注入 (外部内容) | Agent 执行非预期命令 | OpenClaw 自身 exec-approvals 管控 (非 Guardian 职责) |
| 提示注入 (身份覆写) | Agent 人格被重写 | `chattr +i` 保护身份文件 |
| Agent 被攻破 (完整 ocagent 权限) | 读写所有 ocagent 文件 | Vault 隔离 (occlawshell 用户)，备份安全；ocagent 无 sudo，无法提权 |
| Agent 被攻破 + sudo 尝试 | 无法提权 | **OS 级阻止** — ocagent 不在 sudoers 中 |
| 磁盘故障 | 全部数据丢失 | 离线备份 (Phase 3: 云同步) |
| OOM killer | Gateway 被杀，内存损坏 | 优雅 OOM 策略，postmortem，30min 快照 |
| 无限循环 / 死锁 | Gateway 挂起 | 挂起分类器，健康检查定时器，人工介入 |
| Agent 删除自己的记忆 | 记忆丢失 | ClawShell 拥有的备份，git 历史 |
| Agent 修改 ClawShell 代码 | ClawShell 被攻破 | 不同用户，0700 权限，系统服务 |
| 网络中断 | Telegram 告警不可用 | 本地审计日志，systemd journal 持久化 |

## 10. 验证矩阵

部署后执行以下测试：

| # | 测试 | 命令 | 预期结果 |
|---|------|------|----------|
| 1 | Agent 无法访问 vault | `sudo -u ocagent ls /var/lib/occlawshell/` | Permission denied |
| 2 | Agent 无法杀死 ClawShell | `sudo -u ocagent kill $(pgrep -u occlawshell)` | Operation not permitted |
| 3 | Agent 无法修改 SOUL.md | `sudo -u ocagent echo 'x' >> /home/ocagent/.openclaw/workspace/SOUL.md` | Operation not permitted |
| 4 | ClawShell 可读 workspace | `sudo -u occlawshell cat /home/ocagent/.openclaw/workspace/MEMORY.md \| head -1` | 内容显示 |
| 5 | ClawShell 不可写 workspace | `sudo -u occlawshell touch /home/ocagent/.openclaw/workspace/test` | Permission denied |
| 6 | SQLite 备份正常 | `sudo -u occlawshell /var/lib/occlawshell/bin/snapshot-sqlite.sh` | snapshots/ 中生成新 .sqlite |
| 7 | 备份完整性通过 | `sqlite3 /var/lib/occlawshell/snapshots/main-*.sqlite "PRAGMA integrity_check"` | ok |
| 8 | 崩溃时收集 postmortem | `sudo systemctl kill -s SIGABRT openclaw-gateway` | 生成 postmortem 目录 |
| 9 | Gateway 自动重启 | (测试 8 之后) `systemctl status openclaw-gateway` | active (running) |
| 10 | LKG 提升正常 | `sudo -u occlawshell /var/lib/occlawshell/bin/promote-lkg.sh` | 生成 LKG 目录 + 符号链接 |
| 11 | 回滚恢复状态 | `sudo /var/lib/occlawshell/bin/rollback.sh` | SQLite + memory 恢复 |
| 12 | sudo 被 OS 级阻止 | `sudo -u ocagent sudo ls` | ocagent 不在 sudoers，OS 直接拒绝 |
| 13 | 快照定时器触发 | `systemctl list-timers oc-snapshot.timer` | 显示下次触发时间 |
| 14 | 告警到达 Telegram | 触发测试告警 | Telegram 收到消息 |
| 15 | 挂起检测：无误报 | 启动大模型推理，等待 5 分钟 | 状态 = HEAVY_INFERENCE，无告警 |
| 16 | 挂起检测：真阳性 | `kill -STOP $(pgrep openclaw)` (暂停进程) | 5 分钟后状态 = CONFIRMED_HANG |
