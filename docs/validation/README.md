# 验证入口与最近结论

部署前先看[当前能力与已知限制](../guides/current-status.md)。本页提供正在使用的验收入口；按提交冻结的流程、失败记录和性能实验统一从[历史索引](history.md)查阅。

- [索引 sync_all／sync_data 隔离实验](index-sync-experiment.md)：固定生产 half-credit，候选只存在于独立源码副本；含故障注入、短时对照结果和测试者长测命令。

- [首次 checkpoint 错峰实验](first-checkpoint-phase.md)：同屏障启动、首批 3／5／7、相位观测及故障恢复对照。

- [固定 checkpoint 8 MiB／6 MiB 对照](fixed-checkpoint-target.md)：同屏障同目标、实际批次、同步频率与 credit／队龄观测。

- [宿主 PID filter 身份绑定修正](pid-filter-identity-binding.md)：EINVAL 定点模型、失败 syscall 上下文、非空身份完整性与生命周期回归。

## 原生持续录制的最近结论

截至包含 `dde3e1a` 的主线，三相机原生持续录制的 `RAW_QUEUE_OVERFLOW` 尚未关闭，新的六组正式长测及独立相机恢复验收仍未完成。

- [2026-11-07 checkpoint 重叠修正](reports/checkpoint-overlap-20261107/README.md)：两处反压修正及故障回归通过；受控索引争用改善，普通负载未证实整体性能收益，状态开启时 P99 本轮观测变差。连续慢 fsync 仍溢出。
- [2026-11-07 录制验收与候选评估](reports/recording-readiness-20261107/README.md)：源缺口门禁、容量预检及追踪完整性修正；完整额度批次仍为显式候选，不是默认策略。
- [2026-11-06 三 writer 对照](reports/writer-long-run-20261106/README.md)：按其固定产物和环境读取，未完成组不计作成功。

新软件测试不重写早期实机失败，也不补记未执行的恢复。完成目录、可恢复前缀、源端缺帧和观察器事件缺口分别验收。

## 录制与恢复验收入口

在专用 Linux 主机、固定源码和镜像下执行，每次使用不存在的新输出目录。旧日志、失败文件、partial 和 SHA256 原样保留；容量不足时停止新任务。

1. **三 writer 同盘对照**：入口为 [run_writer_long_test.sh](../../scripts/validation/run_writer_long_test.sh) 和 [writer_long_run.py](../../scripts/validation/writer_long_run.py)，包含真实状态持久化 on/off、有界追踪及恢复。至少每组 600 秒、三轮交替；默认生产策略对照显式选择 `half-credit`。详细参数见[方法说明](writer-long-run.md)。
2. **三相机六组及独立 SIGKILL**：入口为 [run_retest.py](../../experiments/sdk-2.10.6-diagnostics/run_retest.py)，使用实际已验证的 capture/health 配置及 [plan 模板](../../experiments/sdk-2.10.6-diagnostics/three-camera.plan.example.json)。冻结本次真实镜像 revision 和摘要，先完成六组正常验收，再执行独立恢复；失败停止。脚本仍位于 experiments，是独立验收工具，不是生产启动入口。
3. **原生与 DDS／IMU 审计**：使用 [imu_long_test.py](../../scripts/validation/imu_long_test.py)、[审计器](../../crates/offline-depth/examples/audit_imu_long_run.rs)及[实际 ROS 观察器](../../ros/imu_long_run_observer.py)，核对 Global／IMU、原生 counts 和归一化 ROS 深度。历史参数说明见[IMU 长测方法](imu-long-run-45992a4.md)，其中固定旧提交的步骤只用于复现旧版本。

先查看本次检出的工具参数；以下命令只显示帮助，不执行设备测试：

```sh
bash scripts/validation/run_writer_long_test.sh --help
python3 experiments/sdk-2.10.6-diagnostics/run_retest.py --help
python3 scripts/validation/imu_long_test.py --help
```

正常组需达到目标并生成有效 COMPLETE；两轮实际读回、逐帧哈希、时钟／样本检查和独立恢复分别保留结果。详细数据语义见[录制指南](../guides/recording.md)，全程采样的历史说明见[方法记录](c063450-hardware-long-run.md)。

## 其他实机记录

- [305/335L 各 30 分钟消费](reports/rust-service-validation-20261028.md)：含短 ROSbag2 读回、管理流程与 SIGSTOP 恢复；资源／状态窗口分别为 29／28 分钟。
- [四机五分钟并行消费](reports/four-camera-5min-20261029.md)：包含缺路拒绝、USBFS 条件和 D2C 溢出失败，适用其报告中的设备与负载。
- [录制、管理与质量功能验收](reports/capture-completion-functional-20261029.md)：包含 counts、SQLite／MCAP 读回和中断观察。
- [SDK 首次重开修复](reports/sdk-first-reopen-20261029.md)：四台分别通过单次重开；启用需新镜像、重新确认及生成配置，旧独立配置不会自动获得恢复能力。
- [SDK 同网格对齐差异](reports/sdk-grid-differences-20261029.json)：记录覆盖范围与共同有效像素误差。

这些记录均有各自的冻结产物，不构成当前发布版本的整套生产验收。

## 自动检查与其他方法

CI 包含 Rust、Python、前端、Shell、Compose、管理生命周期、固定官方驱动构建和 ROS 合成交付；不打开真实设备。构建和检查见[贡献指南](../../CONTRIBUTING.md#开发检查)。

其他方法：[消费与 ROSbag2 验收](consumer-acceptance.md)、[基础语义](semantics.md)、[性能对照协议](benchmarking.md)、[验证工具](../../tools/validation/README.md)。
