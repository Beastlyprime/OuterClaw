# OpenClaw Guardian (安全卫士) 开发计划 v4.0 - "数字堡垒 (The Digital Fortress)" 版

## 1. 项目愿景
构建一个**物理隔离、纵深防御、状态感知**的零信任守护系统。通过“三层防御架构”确保 OpenClaw 在极端对抗环境或资源枯竭下的绝对生存。

## 2. 纵深防御架构 (Defense-in-Depth)

### 2.1 Layer 0: Fortress (系统级加固)
- **三用户隔离模型 (The Tri-User Model)**：
  - `yimeng`: 运行 OpenClaw 应用程序（受限权限）。
  - `ocguardian`: 运行守护进程（只读访问主目录，具备管理服务权限）。
  - `ocbackup`: 独立备份账户（仅允许 `ocguardian` 写入，防止 Agent 删库）。
- **文件不可变保护 (Immutable Identity)**：
  - 使用 `chattr +i` 对 `SOUL.md`, `AGENTS.md` 及核心 Skill 配置文件进行写保护。
  - 仅在 `ocguardian` 身份下通过特定逻辑短暂解锁。
- **systemd 深度沙箱**：
  - `NoNewPrivileges=yes`, `ProtectSystem=strict`, `PrivateTmp=yes`。
  - 限制 `SystemCallFilter=@system-service` 屏蔽危险系统调用。

### 2.2 Layer 1: Sandbox (行为管控层)
- **Exec Allowlist (命令白名单)**：
  - 内建 shell 拦截器，仅允许执行 `git`, `gh`, `ls`, `grep` 等白名单工具。
  - 阻断 `rm -rf /`, `sudo`, `chattr` 等具有毁灭性或越权倾向的操作。
- **语义防火墙 (Semantic Firewall)**：
  - (Phase 3) 引入本地 120B 模型对高风险 `exec` 指令进行意图验证。

### 2.3 Layer 2: Guardian (监控与自愈层)
- **精准挂起判定 (4-Factor Analysis)**：
  - `is_progressing = (delta_io > 1MB or delta_cs > 10)`。
  - 结合 **Syscall 频率分析** 与 **逻辑指纹 (Heartbeat Socket)**，区分“大模型沉思”与“真锁死”。
- **复位原因追踪 (Reset Cause Tracking)**：
  - 参考 `watchdogd` 实践，记录 `reset_reason.log`，区分 OOM, Deadlock, I/O Stall 等诱因。

## 3. 技术规范：快照与原子回滚

### 3.1 防御性 I/O 策略
- **压力预检**：监控 `/proc/pressure/io`，若压力过大则延迟快照，防止备份操作拖垮系统。
- **原子快照**：使用 SQLite `VACUUM INTO` 确保数据库备份无损坏。

### 3.2 自动回滚 (LKG Rollback)
- **Commit-on-Change**：所有配置变更通过 `inotify` 自动提交至 Git。
- **稳定标记**：运行满 30 分钟且健康检查通过的状态标记为 `lkg-stable`。

## 4. 实施路线图 (Roadmap)

- [x] v4.0 架构评审与安全调研 (2026-02-17)
- [ ] **Phase 1: Fortress 加固** (Day 1-2)
  - 实施多用户拆分与 ACL 权限配置。
  - 部署 systemd 增强版 service 文件。
- [ ] **Phase 2: Sentry 升级** (Day 3-5)
  - 开发基于 `strace` 的多因子死锁判定逻辑。
  - 实现逻辑心跳 Socket 通讯。
- [ ] **Phase 3: Vault 与回滚** (Week 2)
  - 实现 I/O 压力感知备份。
  - 完善 LKG 自动回滚链路。
- [ ] **Phase 4: 语义防御** (Week 3+)
  - 对接本地模型进行 Exec 指令意图预审。

*Final specification by Senior Systems Architect (2026-02-17) - Romeo 🌹*
