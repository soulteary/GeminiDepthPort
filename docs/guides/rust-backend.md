<a id="rust-采集后端接入管理流程"></a>

# Rust 采集后端

管理器默认使用官方 ROS 驱动。可在 Web 中[切换后端](web.md#切换型号或后端)，也可在 schema v2 inventory 中
显式设置 `capture_backend:"rust"` 并填写 `rust_capture`。首次配置可从[Rust 单机示例](../../examples/rust-single-camera/README.md)开始。

<a id="配置边界"></a>

## 准备配置

下面是服务策略示例，不包含设备绑定和采集档位：

```json
{
  "capture_backend": "rust",
  "rust_capture": {
    "depth_unit_policy": "normalize-mm",
    "pairing": {"clock": "device", "max_delta_us": 20000},
    "poll_ms": 100,
    "capacity": 4,
    "startup_discovery_seconds": 2,
    "service": {
      "idle_timeout_ms": 2000,
      "startup_timeout_ms": 15000,
      "heartbeat_ms": 500,
      "heartbeat_timeout_ms": 10000,
      "max_restarts": 3,
      "restart_backoff_ms": 1000,
      "shutdown_timeout_ms": 5000,
      "ready_min_pairs": 5
    }
  }
}
```

该兼容策略对应相机 `timestamp_source:"host"`。需要保留有效单流 ROS 图像时添加
`acquisition:{"preset":"complete"}`，并将时间来源设为 `global`；原生完整帧对录制改用
`strict-rgbd`。System／Global 选择和覆盖规则见[采集预设](configuration.md#rust-采集预设)。

每台相机填写独立序列号、命名空间、光学帧前缀和实机支持的两路档位。当前常驻服务要求
彩色、深度同时启用，格式为 YUYV／原始未配准 Y16，深度通过指定策略检查或归一化为毫米。
Gemini 305 是 Rust 专用档案，不能选择官方后端；335L／Gemini 2 按实际设备和档位确认。

相机 `parameters` 允许两路启用标志、宽高帧率、YUYV／Y16、`depth_registration:false`
和四项 `DEFAULT` QoS，以及下述六项曝光／增益控制。滤波、翻转、旋转等参数暂不接受，包括填写默认值。
SDK 同步、聚合和消息时钟通过 `rust_capture.acquisition` 配置。服务预算字段见
[常驻服务参考](../development/capture-service.md)；管理流程的 `shutdown_timeout_ms` 最多为 5000 ms。

<a id="构建与不可变生成版本"></a>
<a id="管理能力"></a>

## 构建与启动

保存配置后，通过 Web 的“构建镜像”或已准备的[宿主 CLI](deployment.md#cli-入口)
执行 `geminidepthport build`。管理器选择 `capture.Dockerfile`，从固定公开提交
`c153462518ad674650bafa4464fda72c27ab797a` 获取 Orbbec SDK 2.10.6，并构建采集服务与 ROS bridge。
SDK 许可和 NOTICE 随镜像保留；APT 和基础镜像未完全锁定，不能据此声称整个镜像可重现。

生成器绑定实际 image ID、runtime 清单和运行二进制摘要，生成私有 capture 配置。
用户无需手写这些摘要或修改 generation。重建镜像后即使标签不变，也需要重新预览确认。
清单字段和校验实现见[后端适配器](../../src/capture_backend.rs)。

按[Web 流程](web.md)逐台预览、确认，停止全部预览，再启动相机组。健康检查同时检查服务
会话／心跳和实际 ROS 图像；消费方仍按[消费指南](consumer-integration.md)核对接收到的数据。
标定导出读取已发布的 CameraInfo 和静态 TF，不额外打开 SDK。CLI 发现设备使用 `scan`；
Rust 镜像没有官方 `list_devices_node`，`devices` 会提示改用 `scan`。

<a id="humble-跨容器通信约束"></a>

## 同机跨容器通信

服务、预览与消费者使用 Humble／Fast DDS、相同 domain、`ROS_LOCALHOST_ONLY=0`，
并加载镜像的 `/app/fastdds.xml`：关闭 builtin transport，使用 UDPv4，仅允许 `127.0.0.1`。
这些设置需要一起使用，以避免 private IPC 容器间 DDS 图可见却收不到图像。XML 已纳入
runtime 摘要，修改后需重建并重新确认。具体消费命令见[通信设置](consumer-integration.md)。

<a id="验证与诊断"></a>

## 维护与回退

结束录制、释放保护并停止相机和预览后，按[配置指南](configuration.md#准备部署配置)维护。
回退官方后端时，按[官方示例](../../examples/official-three-camera/README.md)重新核对型号及参数，
移除 Rust 策略，关闭仅 Rust 支持的 IMU 配置，构建官方镜像并重新确认档位。

运行失败先查[故障排查](troubleshooting.md)；长期录制、性能和硬件验收的当前结论见
[能力与限制](current-status.md)。源码构建和自动检查见[贡献指南](../../CONTRIBUTING.md)。

## 设备能力与曝光／增益

Rust 后端在实际档位启动后、第一帧与 IMU 采集前，读取设备名称、固件／硬件版本和 USB VID／PID，
并查询六项属性的读写支持、范围、步长、默认值和当前值。范围来自当前设备与档位，不能按型号写死。
报告仅覆盖以下六项，并不代表固件全部功能，也不执行固件升级。

在每台相机的 `parameters` 中配置：

```json
{
  "enable_color_auto_exposure": false,
  "color_exposure": 1000,
  "color_gain": 16,
  "enable_ir_auto_exposure": false,
  "ir_exposure": 1000,
  "ir_gain": 16
}
```

这些数值只是语法示例，必须先核对实机范围。曝光与增益均按 SDK 属性整数原样传递，不做微秒或 dB 换算；物理单位须按具体型号的 SDK／固件说明核对。
省略字段沿用设备设置；曝光／增益 `-1` 也表示不写入。手动值大于等于零时，必须显式关闭对应的自动曝光。
Web 提供“沿用／开启／关闭”，其中“沿用”会省略自动曝光字段。

服务先检查全部请求的权限与范围，再依次设置自动曝光和手动值，逐项读回，并在全部设置完成后再次核对。
设置或读回失败时，在本次启动内尝试恢复写入前的值并验证，错误中的 `rollback_verified` 表示恢复是否被确认。
SDK 属性写入不是跨属性事务，恢复失败时不能声称设备值已恢复；服务拒绝进入采集就绪状态。

预览中的“设备能力”显示启动快照；它不是自动曝光的持续遥测。保存更改后必须停止旧预览、重新预览并确认档位。
运行相机、预览占用、录制保护或配置版本过期时，仍禁止保存相关配置，没有绕过保护的实时写属性接口。

原始报告位于采集服务状态 `device_capabilities`，也进入不可变录制快照的
`configuration.device_capabilities`，参与既有快照摘要。预览只读取与当前生成配置 SHA256 匹配的报告，
不额外打开相机。旧 SDK 缺少属性接口且未请求修改时报告不可用；显式请求设置则启动失败。
属性探测仅在启动执行，不增加逐帧 SDK 查询；两轮实际读回、逐帧哈希、同步屏障和崩溃恢复保持既有流程。

本地与 CI 使用合成 SDK／ROS 接口验证，实际设备的属性权限、范围、读回和画面效果由实机测试者复测。
复测请保留固件版本、档位、六项属性报告、失败及恢复结果，使用新输出目录，不覆盖旧失败证据。
