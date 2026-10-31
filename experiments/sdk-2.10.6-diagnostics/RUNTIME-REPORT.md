# 三相机诊断运行镜像验证 — 2026-10-31

本轮补齐独立运行镜像与三相机工具，不修改 SDK 诊断补丁及生产采集、SDK 来源、runtime 门禁和 CI 门禁。操作见 [实机说明](HARDWARE.md)，机器可读结果见 [runtime-image-arm64.json](reports/runtime-image-arm64.json)。本机没有接入三台相机，以下不包含实机帧率、CPU 或持久性验收。

## 实际产物

- GDP 服务源码基线：`f40f316b00e3e9cbf80f1597448a9ce589652ee5`。实验新增脚本由 runtime 文件清单绑定，尚不代表该基线已包含本轮文件。
- 本地镜像：`gdp-sdk-diagnostics-capture:test`，Linux ARM64，ID `sha256:c0adf9ecac1ec98e69f4a45faf5539bbcf6f42fd33804a3e161cbf312d723634`。
- runtime 规范化 JSON 摘要：`e346461be4ed5d8a980b936c8812e3dc90c6678b4f52664b757abaa5d4b8c40b`，绑定 189 个文件路径；不是带换行的原文件字节摘要。
- 实际 SDK 摘要：`107533c9c69b13dbc9f1279fd27614a114c8f99aec459a2edbf2d17665c52229`，GCC `11.4.0-1ubuntu1~22.04.3`，SDK 2.10.6，来源固定提交 `575347ca742d219ef61b1e214e97043f17f48d23`。
- 诊断补丁仍为 `0db54c7c1bcafa9d18cd51bddcd4cf0d0c733c967ff0f9542f7796b770a0757c`。406 个公开 C 导出一致；33 个原始公开头逐文件等于原提交，清单摘要仍为第一阶段的 `b6753f9d936617db43f09e90bcef483ac3d8da74e97d4eb908217f09cbe684a8`。
- CMake 另生成 `libobsensor/h/Export.h`。全部 34 个安装头摘要为 `e086a916c9804ca2b19d17ad40e3df1e8e7de67bb65a980ee30874496aa95511`，也绑定到 SDK 构建记录与 runtime；不把生成头误当成修改过的厂商原始头。

构建只引用公开 Rust/Bullseye、ROS Humble/Jammy 基础及固定公开 SDK Git 提交；不引用 `gdp-coverage-90-env:local` 或其他预先准备的 GDP 镜像。独立微基准开发 Dockerfile 也改为公开 Ubuntu 22.04。最终运行层不复制 SDK 源码、编译缓存或宿主 SDK。首次构建烟测发现未加载 ROS 动态库路径，已修正为先加载 ROS 环境；源码获取遇到一次 TLS 中断，增加有限重试，不降低证书/提交/补丁校验。

构建入口支持 Linux AMD64/ARM64。本轮实际构建与运行验证是 ARM64；AMD64 需在目标主机执行相同构建与烟测。公开基础标签与 apt 包未做字节级冻结，相同 Dockerfile 不保证未来生成相同 SDK/镜像摘要；记录实际清单并固定镜像 ID，跨机器对照使用同一镜像的导出/导入。

## 已完成验证

1. **完整运行清单与拒绝路径**：真实加载 SDK、读取版本 21006 和所需 C 接口、核对 bridge context ABI；真实服务通过三项二进制摘要校验，错误 SDK 摘要被拒绝。分别替换 SDK 字节、补丁字节、manifest 种类，均按预期拒绝；缺少运行配置时默认入口拒绝，未打开设备。无生产 manifest，独立 backend 标签继续不满足生产 `rust-v1` 门禁。
2. **三份完整配置**：使用不存在的合成序列号，分别运行真实镜像中的 Global、IMU、quality、V3 录制配置校验。三份均通过，无 USB/网络挂载，不执行采集。实际生成的 Compose 通过 `docker compose config --quiet`；启用真实硬件仍需要现场已验证的配置与 USB 授权。
3. **Python 回归**：347 项，1 项既有条件跳过，其余通过。本轮新增 18 项测试，覆盖 SDK/头/许可/补丁/服务清单、配置和环境绑定、真实健康流要求、私有目录、三相机隔离、被篡改文件拒绝、部分启动回滚、回滚独立于失败自检、误停防护及正常/恢复审计控制。单元夹具不替代真实文件读回。
4. **Rust 工作区回归**：632 项通过，29 项既有显式性能实验等忽略，覆盖严格 device/global、允许 system 同值、IMU 队列与共享 context、两轮实际读回、帧/索引/载荷哈希、损坏拒绝、提交故障、SIGKILL、检查点前缀恢复及生产保护门禁。生产 Rust 源码完全未改。
5. **最终镜像真实 DDS 合成回归**：原生 YUYV/深度字节、Global metadata、camera_info/QoS、辅助消息、IMU rosbag2 写入与两次读回和损坏拒绝、共享/隔离 context 及销毁顺序、三会话观察器、阶段诊断与四路静态 TF 均通过；没有使用相机。

测试容器只读挂载源码，未挂载 USB 或 Docker socket。完整 Python/Rust 权限测试使用其约定的 Linux 二进制、可写临时夹具和必要的 root 文件所有权测试能力；这些能力不进入实验采集 Compose。采集容器继续 `cap_drop: ALL`，不启用 privileged，不授予 Docker socket。

## 数据保障与现场边界

启动对实际 SDK/bridge/service/auditor、SDK 记录、安装头、许可与实验脚本清单做完整复核，随后才允许原有服务启动；全量摘要计算在启动阶段，不移入逐帧/IMU 热路径。健康检查检查轻量配置/身份、实际 ROS 消费与当前 SDK 快照，不逐次重算整镜像清单。原有录制追加、检查点、两轮实际读回和哈希实现不变。

正常审计需要完整目标帧对、IMU 总数匹配及两轮新 reader 验证；崩溃审计只读已确认 checkpoint 前缀，普通完整 reader 必须拒绝 partial，不生成 COMPLETE。SDK 诊断和普通录制错误仍明确保留，回滚不删除证据或改生产配置。两轮 read 不等同于绕过页缓存的两次物理介质读取；SIGKILL 恢复不等同于断电验证。

本轮不沿用旧 GCC 12 SDK 微基准作为新镜像性能数值。旧报告仍见 [REPORT.md](REPORT.md)，现场用同一新镜像 ID 的 on/off 交替试验采集 SDK 配对/队列/锁、GDP checkpoint/追加 P99、CPU、RSS/HWM/cgroup 和 DDS/native 双向证据。尚待执行三相机真实 USB、IMU、Global 时间映射、原生录制完成、正常两轮读回与 SIGKILL 恢复的联合验收；未宣称 66 ms 缺口消失或新的实机性能收益。
