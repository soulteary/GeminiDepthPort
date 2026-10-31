# 一键三相机流程验证 — 2026-10-31

基于 `ee8c581` 的独立 SDK 诊断镜像与硬件工具增加宿主编排脚本 `run_hardware.py`，操作见 [ONE-CLICK.md](ONE-CLICK.md)。生产 SDK、SDK 诊断补丁、镜像内 runtime/健康校验器、Rust 采集/录制/读回、ROS 发布及 CI 门禁均未修改。

## 验证结果

- Linux 完整 Python 回归：372 项，1 项既有环境条件跳过，其余通过。
- 新增一键流程测试：25 项，Linux 全部通过。涵盖平台/UID/Docker 上下文拒绝、健康等待和采样配额预算、健康/完成条件分离、超时、错误状态与身份绑定、off 模式、真实摘要检查、检查点观测完整性、两轮/哈希验收要求、源缺口单独报告、日志与 SDK 快照保留、取消、部分启动、身份检查回滚和 PID 复用拒绝。
- 真实子进程测试：完整模拟 CLI 启动三个独立采样子进程，执行阶段转换与验收报告检查；另外注入真实 SIGTERM、采样器非零退出和真实睡眠进程超时，确认进程被回收、失败阶段保存、录制证据字节不变且调用实验停止入口。
- Linux 持久化进程回滚测试：检查 PID、启动时钟、UID、独立进程组/session 与私有 token 后清理真实测试子进程；复用 PID 的替身不收到信号；构建阶段尚无采集容器时也可清理自建进程，不依赖镜像自检。
- Python 编译检查、CLI 参数检查、Markdown 相对链接及 `git diff --check` 通过。

测试环境只读挂载源码，没有 USB 和 Docker socket。完整回归使用原有 Linux 开发依赖及已经构建的原主线 Rust 测试程序；本轮没有修改或重新宣称 Rust/SDK/ROS 行为与性能结果。此前镜像构建和原生保障验证保留在 [RUNTIME-REPORT.md](RUNTIME-REPORT.md)。

## 测试边界

`tests/fixtures/sdk_one_click_simulator.py` 明确替换 SDK/Docker 后端和采样遥测；虽然编排 CLI、文件和子进程信号是真实执行，测试中的验收数据仍是合成夹具，不是物理采集，也不证明 SDK 队列、USB 或现场帧率。没有运行真实三相机、硬件 IMU 或物理故障注入。本轮没有新的实机性能收益结论。

实际现场运行必须读取根目录最终 `result.json`：正常录制/读回/DDS 检查通过与 `source_gap_free` 分开；`crash_recovery_verified=false` 保留“本轮未做 SIGKILL 恢复”的事实。两轮实际 reader、哈希和完整/partial 门禁继续调用现有主线审计实现，未改为缓存/模拟校验。SIGKILL 实机恢复仍在另一新目录按手动说明执行。

统一 root UID 是为了让采集、私有诊断、DDS 数据库与验收使用一致的拥有者；脚本拒绝远程/rootless Docker，不自动更改源配置权限或生产 SDK。失败默认清理本轮已验证身份的资源，保留所有录制、partial、状态、采样与失败日志，不自动重试或重用目录。初始权限/参数检查失败时尚未创建运行目录，错误通过 stderr 返回。
