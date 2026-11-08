# 录制、校验与恢复

先根据需要保存的数据选择录制方式，再配置采集预设。开始录制前完成设备预览确认和消费检查；需要阻止维护或重启时按[生命周期指南](lifecycle.md#录制保护与并发)取得保护，结束后由持有方释放。

三相机原生持续录制仍存在未关闭的队列溢出，部署前阅读[当前限制](current-status.md#持续录制尚未完成正式验收)。

## 选择录制方式

**外部 ROSbag2** 保存 ROS 发布的数据。使用 `complete` 时分别订阅彩色、深度及需要的元数据／IMU 话题。ROS 深度是毫米 `16UC1`，不是原生 SDK counts；相关接入及读回步骤见[消费指南](consumer-integration.md)。历史 bag 导入见[录制适配器](../../tools/recording/README.md)。

**原生录制** 由 Rust `capture-service` 的可选 `raw_recording` 配置启用，保存原始 counts、尺度、完整帧对及已启用的 IMU。Web 的 Rust 配置页可按相机显式启用；默认关闭。管理器在正式服务配置中写入录制设置，并将部署目录的 `state/native-recordings/<相机名>` 私有挂载到 `/native-recordings`，不接受浏览器指定任意主机目录。预览始终不录制；关闭配置不删除旧数据。独立服务部署也可使用[原生服务配置契约](../contracts/native-sdk-recording-v1.md)准备配置，或按[验收工具](../validation/README.md#录制与恢复验收入口)在独立测试部署中运行；不要修改运行中的 generation。原生录制拒绝 `complete` 预设，需要完整帧对采集策略。

保护和 `recording-context` 查询不启动录制。它们用于锁定会话及提供调用方上下文；外部 ROSbag2 和原生录制仍需各自启动、停止或达到目标。

## 版本名称

- **契约 v1**：`native-sdk-recording-v1.md` 中定义的服务配置和状态语义，不是文件格式版本。
- **`files-v2`**：原生录制默认目录格式，分别存储每对数据的文件。
- **`chunked-v3`**：显式选择的分块目录格式，提供已确认前缀恢复。仍要求完整帧对。
- **`complete`**：小写时可能指采集预设；录制状态中的 `complete` 则表示事务已完成，两者含义不同。
- **`COMPLETE`**：完整录制的完成标记；`.partial` 或有效 checkpoint 只说明存在未完成任务或可恢复前缀。

本指南用“原生完整帧对录制”指该功能，用 `files-v2`／`chunked-v3` 指目录格式。部分错误提示仍沿用“原生 V2”旧称；实际格式以 `storage_format` 配置为准，V3 同样不支持独立单流录制。同步顺序见 [V3 契约](../contracts/native-recording-v3.md)。

## 正常结束与读回

原生录制达到 `max_pairs` 后停止提交新帧，随后排空队列、校验数据并提交完成目录。相机及 ROS 发布可以继续运行；在目标完成前停止服务会留下未完成录制。

确认最终状态为 `complete`，再使用支持该格式的 reader 校验完成目录。诊断中的 `written_pairs` 是追加成功数，`durable_pairs` 是已确认前缀数；两者都不能单独代替最终完成状态。V3 的 checkpoint 增量读回与结束时整段读回构成两轮实际校验，包含逐帧哈希；V2 没有这套 checkpoint 前缀机制。验收审计另有两个独立 reader。

命令及导出选项见[离线工具](../../crates/offline-depth/README.md)。原生 counts 与 ROS 毫米深度分别按自身表示校验；不要把两者的原始字节哈希直接比较。

## 失败后的前缀恢复

出现队列、SDK、时钟、校验、存储或预算错误时先保存失败状态和日志，按[故障排查](troubleshooting.md#原生录制队列溢出)处理。任务停止后，可对 V3 未完成目录执行只读恢复：

```sh
offline-depth --recover-recording-prefix /absolute/path/capture.partial
```

成功结果包含 `recovered_prefix_valid=true` 和 `complete=false`。恢复不追加帧、不修补原目录，也不把前缀变成完整任务。保留原始 `.partial`、checkpoint、失败文件和校验和；新测试使用新输出目录。
