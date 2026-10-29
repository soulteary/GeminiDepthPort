# GeminiDepthPort 文档

初次使用从[部署](guides/deployment.md)开始。接入采集数据从[消费方指南](guides/consumer-integration.md)
开始；已知硬件结果和限制集中在[验证索引](validation/README.md)。

## 使用指南

[公开接入示例](../examples/README.md)提供未绑定配置、受管接入步骤和独立硬件测试入口。

- [部署与开机管理](guides/deployment.md)
- [Web 配置、预览与确认](guides/web.md)
- [接收长尾与分阶段诊断](guides/pipeline-diagnostics.md)
- [相机配置](guides/configuration.md)与[旧版迁移](guides/migration-v1.md)
- [生命周期与录制保护](guides/lifecycle.md)
- [可选 Rust 采集后端](guides/rust-backend.md)
- [消费方接入与通信设置](guides/consumer-integration.md)
- [USBFS 预检与管理员操作](guides/usbfs.md)
- [故障排查](guides/troubleshooting.md)

## 接口与契约

- [相机消息、时间和坐标语义](interfaces/camera.md)
- [标定快照](interfaces/calibration.md)
- [质量查询与录制上下文](interfaces/quality.md)
- [官方后端只读观察器](interfaces/official-observer.md)
- [快照与帧上下文 v1](contracts/capture-contract-v1.md)
- [配对证据 v1](contracts/pair-evidence-v1.md)
- [质量核心 v1](contracts/quality-core-v1.md)与[常驻服务适配 v1](contracts/service-quality-v1.md)
- [ROS 观测关联 v1](contracts/observed-association-v1.md)与[中断行为 v1](contracts/observer-interruption-v1.md)
- [会话绑定录制保护 v2](contracts/recording-protection-v2.md)
- [原生 SDK 启动前 USB 恢复 v1](contracts/usb-stream-recovery-v1.md)
- [原生 SDK 录制 v1](contracts/native-sdk-recording-v1.md)
- [深度输出 v1](contracts/depth-output-v1.md)

录制与离线工具入口：[录制适配](../tools/recording/README.md)、
[离线深度校验与导出](../crates/offline-depth/README.md)。

## 验证与开发

[硬件结果与已知限制](validation/README.md)、[消费与 ROSbag2 验收](validation/consumer-acceptance.md)、
[基础语义验收](validation/semantics.md)、[性能对照协议](validation/benchmarking.md)。

开发者参考：[常驻采集服务](development/capture-service.md)、[管理服务](development/management.md)、[SDK 与 ROS](development/sdk-and-ros.md)、
[官方 YUYV 适配](development/native-yuyv.md)、[SDK 扩展评估](development/sdk-extensions.md)、[Linux 合成管理测试](development/linux-management-fixture.md)、
[worker 进程组清理](contracts/worker-process-group-cleanup.md)。

构建、检查及文档贡献规则见[贡献指南](../CONTRIBUTING.md)。
