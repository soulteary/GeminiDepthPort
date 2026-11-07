# GeminiDepthPort 文档

首次部署按[部署指南](guides/deployment.md)安装，再通过 [Web](guides/web.md)绑定设备、预览确认并启动。开始长期任务前查看[当前能力与已知限制](guides/current-status.md)。

## 按任务查找

- 安装、开机和日常启停：[部署](guides/deployment.md)。
- 准备型号、后端和参数：[相机配置](guides/configuration.md)、[公开示例](../examples/README.md)。
- 预览和确认档位：[Web 操作](guides/web.md)。
- 订阅图像及 IMU：[消费接入](guides/consumer-integration.md)、[IMU 配置](guides/configuration.md#rust-相机-imu)。
- 保存数据、校验和恢复：[录制指南](guides/recording.md)。
- 排查失败：[故障排查](guides/troubleshooting.md)、[接收长尾诊断](guides/pipeline-diagnostics.md)、[USBFS](guides/usbfs.md)。
- 维护配置或切换后端：[生命周期与保护](guides/lifecycle.md)、[Rust 后端](guides/rust-backend.md)、[旧配置迁移](guides/migration-v1.md)。

## 接口与契约

接口说明供消费方接入，契约定义字段、状态和失败规则。版本名称见[录制版本说明](guides/recording.md#版本名称)。

- [相机消息、时间与坐标](interfaces/camera.md)、[标定快照](interfaces/calibration.md)、[质量与录制上下文](interfaces/quality.md)、[官方只读观察器](interfaces/official-observer.md)。
- [快照与帧上下文 v1](contracts/capture-contract-v1.md)、[配对证据 v1](contracts/pair-evidence-v1.md)、[深度输出 v1](contracts/depth-output-v1.md)。
- [质量核心 v1](contracts/quality-core-v1.md)、[常驻质量适配 v1](contracts/service-quality-v1.md)、[ROS 观测关联 v1](contracts/observed-association-v1.md)、[中断行为 v1](contracts/observer-interruption-v1.md)。
- [录制保护 v2](contracts/recording-protection-v2.md)、[原生服务录制 v1](contracts/native-sdk-recording-v1.md)、[分块目录 V3](contracts/native-recording-v3.md)、[相机 IMU v1](contracts/camera-imu-v1.md)。
- [能力协商 v1](contracts/provider-capabilities-v1.md)、[SDK 启动前 USB 恢复 v1](contracts/usb-stream-recovery-v1.md)、[worker 进程组清理](contracts/worker-process-group-cleanup.md)。

## 验证与开发

[验证入口](validation/README.md)列出当前验收方法与最近结论。[历史索引](validation/history.md)保留按提交和日期冻结的复测说明、失败记录及性能实验，复现旧结果时再查阅。

实现与调试资料见[开发参考](development/README.md)。构建、测试和文档维护规则见[贡献指南](../CONTRIBUTING.md)。
