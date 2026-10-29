# 相机配置

默认配置为 schema v2，提供三个未绑定、禁用的 RGB-D 槽位。支持一至三台启用设备；
旧版双机 schema v1 仅为兼容入口。部署见[部署指南](deployment.md)，运行状态见[生命周期](lifecycle.md)。

## 配置字段

`driver_profiles.json` 是依据公开上游 launch 参数建立的型号目录。型号选择与逻辑位置分离，同一型号可以绑定多个逻辑相机，但不能自行指定 launch 可执行文件。

每台相机分别声明：

- `name`：逻辑标识，仅用于管理。
- `profile`：受支持的公开驱动配置标识。
- `namespace`：显式绝对 ROS 命名空间，不从逻辑名称推导。
- `frame_prefix`：显式坐标系前缀；通过组件启动适配器与命名空间分别绑定。
- `serial_number`：受控设备绑定；公开模板为空。
- `enabled`：是否参与采集，启用设备必须已绑定。
- `parameters`：型号目录允许的驱动参数；身份、数据源和路径类受管理字段禁止注入。
- `timestamp_source`、`calibration_revision`：受控的时间与标定声明。
- `depth_unit`：公开接口要求的毫米单位。
- `health`：受控检查策略。
- `profiles_verified`、`verification`：确认结果及其配置摘要，仅用于绑定实际检查结果。

启用数量可以是一至三台。没有启用设备时允许草稿编辑，但严格校验返回 `NO_ACTIVE_CAMERAS`，不会报告采集就绪。彩色必须请求 YUYV，深度必须请求原始未对齐的数据格式。图像真实字节、深度尺度和实际硬件支持仍需后续运行时检查。

`inventory.v2.example.json` 是未绑定模板，不提供档位或标定，不能直接启动设备。选择可选 Rust 后端时，还需满足[后端配置限制](rust-backend.md)。

完整接入参考见[公开示例](../../examples/README.md)：官方三机和 Rust 单机均保持未绑定、禁用与未确认。

## Rust 采集预设

显式选择 Rust 后端后，可在 `rust_capture.acquisition` 中选择采集目标。
新配置推荐 `{"preset":"complete"}`；旧配置省略该字段时保留原有 SDK 默认同步与
system 消息时间戳，不会在升级后自动切换行为。

- `complete`：关闭 SDK 软件彩深同步，采用 `any` 聚合输出，保留有效单流图像；
  消息时间戳默认使用 global。适合优先保存收到的图像。它不承诺设备、USB 或队列绝不丢帧。
- `strict-rgbd`：开启 SDK 软件彩深同步，采用 `any` 聚合以观察缺流，
  仅发布通过现有完整 RGB-D 配对检查的图像。消息时间戳默认使用 global；
  配对输出可能少于 SDK 收到的图像数量。
- `hardware-sync`：保留的硬件同步预设。目前未实现硬件主从配置及验证，明确拒绝启用；
  不会把 global 时钟映射或 SDK 软件配对显示成硬件同步。

例如，为不支持 global 的设备明确选用 system 时间，而不是逐帧静默回退：

```json
{
  "acquisition": {
    "preset": "complete",
    "advanced": { "message_timestamp": "system" }
  }
}
```

上例字段应放入完整的 `rust_capture` 策略中。global 对应启用相机的
`timestamp_source:"global"`；system 对应 `timestamp_source:"host"`，不一致会被拒绝。
global 必须由设备支持、成功启用且帧值有效；启动期限内允许等待拟合产生非零值，
就绪后失效则明确失败，不会混用 system。适配层在开始采集前显式设置并读回 SDK
主机时钟为 Unix 实时时钟，活跃 pipeline 期间禁止切换，确保可用于 ROS `header.stamp`。

高级选项还有 `sdk_frame_sync`（布尔）、`aggregate`（`any`、`all`、`disabled`）及
`unpaired`（`preserve`、`strict`）。覆盖不能破坏预设语义：`complete` 不接受 `all` 或
`strict`，`strict-rgbd` 必须开启软件同步并采用 `strict`，且不接受会逐帧独立输出的
`disabled` 聚合模式。`all` 会在 SDK 内过滤不完整帧，
使应用无法统计那些帧；通常保留 `any`。SDK 同步与聚合选项均在开始采集前下发。

现有 `pairing.clock` 与 `max_delta_us` 仍只检查同一 SDK frameset，
尚不提供跨 frameset 的缓冲配对或跨设备配对。global 用于消息时间表达，不会改变
SDK 内部选择 device/system 配对时钟的逻辑，也不会使曝光时刻自动对齐。

原始单流保留是 ROS 图像输出能力；下游录制器需要订阅并记录两路。
内置 native V2 录制格式仍要求完整、有效配对，缺流时失败，不会伪造配对或宣布录制完整。
每次修改预设或高级参数都会改变配置绑定，需要重新生成并确认档位。

## 离线校验

在已安装项目依赖的环境执行：

```sh
geminidepthport validate --file inventory.v2.example.json --draft
```

输出只包含校验状态、schema 版本、启用数量和硬件验证标记。`--draft` 允许未确认的配置，不代表设备健康。省略 `--draft` 时，启用相机必须携带与当前配置相符的检查证据。

确认摘要涵盖设备绑定、命名空间、坐标系前缀、型号、档位、时间、标定和共享运行字段。通过保存入口修改配置会清除受影响的确认；共享运行字段变化会清除全部确认。仅改另一台相机不会使本台确认失效。

直接编辑受控文件或伪造证据不属于可信检查流程。摘要用于检测漂移，不是数字签名或身份认证。schema v2 使用显式命名空间生成预览绑定，以实际消息和 QoS 检查确认档位；确认不代替标定精度、深度尺度和消费方时间验收。

## 兼容迁移

已有双机 schema v1 配置按[旧版迁移指南](migration-v1.md)执行预览、备份与应用。
新部署无需执行迁移。

## 运行资料

真实 inventory、绑定映射、迁移备份、标定及运行快照均属于私有运行资料。
档位确认只绑定当时实际消息检查，不认证物理精度或硬件同步；验收见[验证索引](../validation/README.md)。
