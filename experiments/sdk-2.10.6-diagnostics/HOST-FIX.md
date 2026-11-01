# 实机交接中的宿主脚本修正

2026-11-01，合入 2026-10-31 实机交接包 `proposed-fix/host-fix.patch` 的两处修正，并补充进程替换等拒绝路径回归。

## 真实运行身份绑定

准备阶段 `GDP_CAPTURE_INSTANCE_ID` 是 supervisor 的 launch token；生产 supervisor 为 worker 生成另一 instance。不能直接要求 worker status instance 等于 launch token。

宿主首先检查实验标签、run、镜像、禁止自动重启的容器策略；然后通过容器 PID namespace 中的真实 worker 验证 executable inode、父 supervisor、双方环境 token、配置路径及摘要、restart attempt、supervisor PID。读取前后检查进程出生时间与容器身份，拒绝替换及 PID 复用。

验证成功后原子固定到各相机 `runtime-identity.json`（同 UID、0600、fsync、不可覆盖既有绑定）。后续采样、正常审计及 SIGKILL 前缀审计要求同一容器启动、session、worker 和配置身份。容器停止后仅接受之前已验证固定的身份；不会从离线 status 学习新身份。不要复制或手工构造此文件为旧实验补验收。

一键脚本在健康等待阶段通过现有 recording_path 检查建立绑定。独立操作需在容器仍健康时使用 `hardware.py sample` 现有绑定检查入口，或执行 `crash`（在注入前建立绑定）；仅停止后第一次 audit 会拒绝未绑定运行。回滚仍使用原有容器身份校验，不以运行绑定成功为前提。

## Docker 健康命令

Docker healthcheck 不经过镜像 entrypoint。生成命令改为 `/bin/bash -c 'source /opt/ros/humble/setup.bash && exec python3 /opt/gdp-experiment/health.py'`，保留原 validator、超时、重试及启动宽限。既有准备目录不会被修改，请使用新目录重新 prepare/run。

## 验证与边界

回归覆盖不同 launch/worker token 的成功绑定、停止后读取、不可重新绑定、容器替换、PID 复用、状态过期、私有文件和符号链接拒绝、namespace/config/executable 不匹配、僵尸进程、重复 token、错误 supervisor/restart 参数，以及校验期间进程替换；同时保留完整录制与恢复审计中双 reader/hash 拒绝路径。

本次仅修改实验宿主工具和测试，不修改 SDK、生产镜像、Rust 采集录制或门禁。不能据此认为 RAW_QUEUE_OVERFLOW 已修复，也没有新增实机性能结论。两轮实际读回、哈希、完整录制与恢复验收条件保持不变。

验证结果：隔离 Linux 环境运行完整 Python 回归 397 项，1 项既有条件跳过，其余通过；身份专项 10 项全部通过。无物理相机测试，交接包的实机结论不作为本次新增验收。
