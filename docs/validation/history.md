# 历史调查与实验索引

这里保存按提交、日期和测试环境形成的调查、复测说明及结果。文件保留原始正文；其中“本次”“当前”“新版”等指当时的产物，不是对当前主线的推荐。复现旧结果时使用文中指定的源码、镜像和配置；开始新验收从[验证入口](README.md)选择本次检出的工具。

原始失败、日志、压缩包、补丁和 SHA256 不做润色或重算。当前行为写入对应指南和契约，后续结果通过新增记录关联，不覆盖历史结论。

## 录制调查与操作记录

先列较新的录制工作，再列早期 IMU 和实机排查，避免沿提交号逐层寻找。

- [checkpoint 与追加重叠修正](checkpoint-overlap.md)。
- [录制可靠性修复与部署验收](recording-readiness.md)。
- [三 writer 同盘长时对照与 syscall／调度追踪](writer-long-run.md)。
- [原生录制：首次队列溢出的精确观测](raw-overflow-observation.md)。
- [8c279f9 后续：状态同步减量与 writer 对照](8c279f9-storage-follow-up.md)。
- [a095de0 失败后的状态等待与 IMU 修复](a095de0-failure-follow-up.md)。
- [c063450 检查点长尾关联分析](checkpoint-tail-analysis-c063450.md)。
- [c063450 三相机全程采样长测](c063450-hardware-long-run.md)。
- [IMU 错误收尾与关键状态观测](imu-error-cleanup-observations.md)。
- [45992a4 三相机与 IMU 实机长测](imu-long-run-45992a4.md)。

## 冻结结果与性能实验

按记录日期从新到旧排列。报告的版本、成功／失败范围及样本口径以原文为准；覆盖率摘要和校验和只用于对应产物。

- [coverage-summary.md](reports/recording-readiness-20261107/coverage-summary.md)。
- [录制验收修复与候选评估](reports/recording-readiness-20261107/README.md)。
- [6c2c25a 覆盖率 CI 跟进](reports/ci-follow-up-6c2c25a-20261107/README.md)。
- [checkpoint 重叠修正验证](reports/checkpoint-overlap-20261107/README.md)。
- [coverage-summary.md](reports/writer-long-run-20261106/coverage-summary.md)。
- [三 writer 长时状态后台／内核追踪验证归档](reports/writer-long-run-20261106/README.md)。
- [Bounded checkpoint synchronization comparison — 2026-11-01](reports/checkpoint-sync-ab-20261101.md)。
- [状态报告锁占用优化 — 2026-10-30](reports/status-report-lock-ab-20261030.md)。
- [校验读取器优化与 CPU 细分 — 2026-10-30](reports/reader-optimization-ab-20261030.md)。
- [IMU ROS 消息复用与共享 context 验证（2026-10-30）](reports/imu-ros-reuse-ab-20261030.md)。
- [IMU 配置缓存与计时拆分验证（2026-10-30）](reports/imu-config-cache-ab-20261030.md)。
- [IMU 有界缓冲区复用验证（2026-10-30）](reports/imu-buffers-ab-20261030.md)。
- [Bounded checkpoint pipeline A/B — 2026-10-30](reports/checkpoint-pipeline-ab-20261030.md)。
- [录制检查点分段观测与采样开销 — 2026-10-30](reports/checkpoint-observation-ab-20261030.md)。
- [相机 IMU 支持与性能验证（2026-10-30）](reports/camera-imu-ab-20261030.md)。
- [Streaming recording validation: before/after release experiment](reports/streaming-verification-ab-20261029.md)。
- [Background status persistence: release A/B comparison](reports/status-persistence-20261029.md)。
- [Native SDK first reopening: functional verification, 2026-10-29](reports/sdk-first-reopen-20261029.md)。
- [Native V3 write batching A/B — 2026-10-29](reports/native-write-ab-20261029.md)。
- [Native V3 chunk recording: release comparison](reports/native-recording-v3-ab-20261029.md)。
- [四机 RGB-D 五分钟复测（2026-10-29）](reports/four-camera-5min-20261029.md)。
- [RGB-D functional validation — 2026-10-29](reports/capture-completion-functional-20261029.md)。
- [2026-10-28 Rust 采集服务验证记录](reports/rust-service-validation-20261028.md)。

## SDK 2.10.6 诊断实验

这些工具和说明为独立测试部署准备，不代替 `deploy.sh` 的生产管理流程。文中旧运行目录只用于定位历史，不用于新测试。

- [SDK on/off 批量流程验证 — 2026-10-31](../../experiments/sdk-2.10.6-diagnostics/AB-REPORT.md)。
- [至少三轮 SDK on/off 自动交替对照](../../experiments/sdk-2.10.6-diagnostics/AB.md)。
- [三相机 SDK 诊断实验镜像](../../experiments/sdk-2.10.6-diagnostics/HARDWARE.md)。
- [实机交接中的宿主脚本修正](../../experiments/sdk-2.10.6-diagnostics/HOST-FIX.md)。
- [一键三相机流程验证 — 2026-10-31](../../experiments/sdk-2.10.6-diagnostics/ONE-CLICK-REPORT.md)。
- [一键三相机实机测试](../../experiments/sdk-2.10.6-diagnostics/ONE-CLICK.md)。
- [一键打包三相机复测证据](../../experiments/sdk-2.10.6-diagnostics/PACK.md)。
- [SDK 2.10.6 第一阶段诊断实验](../../experiments/sdk-2.10.6-diagnostics/README.md)。
- [SDK 第一阶段诊断结果 — 2026-10-31](../../experiments/sdk-2.10.6-diagnostics/REPORT.md)。
- [一键三相机流水线复测与独立恢复验收](../../experiments/sdk-2.10.6-diagnostics/RETEST.md)。
- [三相机诊断运行镜像验证 — 2026-10-31](../../experiments/sdk-2.10.6-diagnostics/RUNTIME-REPORT.md)。
