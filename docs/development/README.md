# 开发参考

这些资料解释实现和调试方法。部署、采集和录制步骤从[文档首页](../README.md)进入；固定提交的调查和实验结果从[历史索引](../validation/history.md)查找。

- [管理服务](management.md)：配置、任务与启停入口。
- [常驻采集服务](capture-service.md)：worker 监督和运行配置。
- [SDK 与 ROS](sdk-and-ros.md)、[SDK ABI](../../crates/orbbec-sys/README.md)、[原生 YUYV 适配](native-yuyv.md)、[SDK 扩展评估](sdk-extensions.md)。
- [检查点观测](checkpoint-observation.md)、[读取器 CPU 分析](recording-reader-profile.md)。
- [Linux 合成管理测试](linux-management-fixture.md)、[worker 清理契约](../contracts/worker-process-group-cleanup.md)。

修改行为时先同步对应接口或契约，再在用户指南中说明操作变化。实验对照保留源码／镜像身份、方法、成功与失败结果，原始日志及 SHA256 按冻结记录保存。

[旧行为合成参考](../../tests/legacy_reference/README.md)仅供回归测试，不进入管理镜像。
