# Rust 单机配置意图

[inventory.json](inventory.json) 显式选择 `rust`，提供一个未绑定、禁用、未确认的
RGB-D 槽位与有界服务策略，显式选择 `complete` 预设和 `global` 消息时间戳。
640×480@30 仅为候选；健康策略的发现及采样窗口各 8 秒、新鲜度 1 秒、
最低 FPS 比例 0.8，以及设备时钟配对上限 20 ms、服务启动预算 30 秒都需审查。
它供接入参考，不是已经验证的相机档位。
参数解释与限制见[Rust 后端指南](../../docs/guides/rust-backend.md)。

## 接入流程

1. 按[示例总览](../README.md)离线校验，在专用 Linux 部署中选择模板。
2. 扫描并绑定实际受支持设备。两路都必须启用；填写实机支持的宽高/FPS、YUYV
   彩色和未配准 Y16 深度，确认设备支持 global 时间，使用 `global` 时间来源与
   显式毫米归一化策略。
3. 通过 `geminidepthport build` 或 Web 构建操作构建 Rust 采集镜像。镜像标签只是
   选择入口，正式身份由实际 image ID 及运行清单绑定。
4. 保存配置，逐台预览并实际确认档位；停止预览后执行正式启动。正式生成器将
   SDK、bridge、service 和 USB 恢复 helper 的清单、摘要及路径写入私有 generation。
   用户不用手写 `capture.json`，也不能从别的镜像复制 SHA256。
5. 由实际健康检查与消费订阅核对出帧、帧率、时间、布局及会话，再开始录制。

`rust_capture` 中的配对与服务预算是明确的示例策略，需按实际接入需求审查。
遇到失败先保留诊断，不为通过检查而自动放宽时间差、延长启动预算或增加重试。
当前 Rust 后端拒绝未实现的曝光、滤波、旋转等官方参数，不能从官方参数
整段复制后假定已经生效。SDK 软件同步与聚合仅通过受控 `acquisition` 策略配置。

## 选择采集策略

本例的 `complete` 在开始采集前关闭 SDK 软件彩深同步，使用 `any` 聚合输出，
让有效的彩色和深度图像分别发布；未形成 RGB-D 对不会使已有图像被整组排除。
消息 header 使用映射到主机 Unix 时钟的 global 时间，仍保留 device 和 system
时间供诊断。它不承诺设备、SDK 队列或下游录制器绝不丢帧，需核对各阶段的计数。

设备不支持 global 时，明确修改策略并将相机 `timestamp_source` 改为 `host`：

```json
{
  "preset": "complete",
  "advanced": { "message_timestamp": "system" }
}
```

这段替换 `rust_capture.acquisition`，不会因单帧 global 无效而自动回退。
global 映射不表示曝光同步。硬件同步预设 `hardware-sync` 当前不可用，选择后会被拒绝。

`pairing.clock` 与 `max_delta_us` 仍只检查同一个 SDK frameset，不实现跨 frameset
或跨设备缓冲配对。必须获得完整 RGB-D 对的消费者可改用 `strict-rgbd`；该预设
开启 SDK 软件同步并仅发布通过配对检查的图像，配对输出数量可能少于收到的图像。
任何策略或时间来源变化都需要重新生成配置并确认档位。

## 快照、质量与录制

在已启动并验证的受控部署根目录，可查询：

```sh
geminidepthport check
geminidepthport capture-snapshot
geminidepthport quality-current camera_a
geminidepthport quality-events camera_a --limit 20
geminidepthport recording-context --require-strong
```

命令返回的上下文及事件保存在私有环境，不公开上传。上下文查询不等于已经取得保护；
录制调用方需按[质量接口](../../docs/interfaces/quality.md)和
[录制保护契约](../../docs/contracts/recording-protection-v2.md)取得保护后重新核对
绑定的 generation、session 和 snapshot，并在结束后释放凭证。

`complete` 的图像保留通过 ROS 单流输出实现，需要下游录制器分别订阅并记录两路。
内置 native V2 录制只接受完整配对，不能与本例的 `complete` 策略同时启用；
需要 native V2 时应明确切换 `strict-rgbd` 并重新确认。不能把仅完整配对的录制
描述成全部原始图像都已保存。

元数据应按各自流时间戳与实际会话关联，不把两个时间戳覆盖为相同值，也不将
SDK frameset 关联标为已认证硬件同步。消费端使用与服务一致的 Humble/Fast DDS、
通信域和 loopback UDP XML，见[消费指南](../../docs/guides/consumer-integration.md)。

## SDK 异常退出与离线输出

新镜像生成的配置包含受指纹保护的 USB 启动前恢复 helper。它在允许启动时、加载
SDK 前工作；录制保护仍阻止恢复和重新打开设备。旧独立配置缺少 `usb_recovery`
时不会自动获得这项能力。启用需重建镜像、重新确认档位并生成新 generation，见
[恢复契约](../../docs/contracts/usb-stream-recovery-v1.md)。

首次重开测试属于独立故障注入，见[硬件测试示例](../hardware-reopen/README.md)，
不要对正在录制的正式设备运行。已有四台分别通过的记录不是多机规模或长稳验收。

历史录制先通过[录制适配器](../../tools/recording/README.md)保留真实快照、像素和
配对证据，再按[offline-depth](../../crates/offline-depth/README.md)执行输入校验、
可选离线 D2C 和无损 PNG/TAR 输出。该工具不直接读取裸 MCAP，不负责选择时间对；
不要构造假 snapshot 或 ID 来凑出可以运行的输入。第一版不发布在线 aligned-depth。
