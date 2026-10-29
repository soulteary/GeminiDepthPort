# Rust 采集后端接入管理流程

Rust 数据链是显式可选后端。未填写 `capture_backend` 或填写 `official` 时，继续使用原有官方 ROS 驱动；不会因检测到 Rust 程序而自动切换。

## 配置边界

仅 schema v2 可以选择 `capture_backend: "rust"`。需要在 inventory 根对象同时填写完整 `rust_capture` 策略，例如下面的**策略示例**；它不是实机档位、硬件同步或性能承诺：

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

每台启用的相机需要独立的真实绑定、命名空间、光学帧前缀和实际验证过的彩色/深度宽高帧率。当前常驻 Rust 服务要求同时启用两路；正式彩色为 YUYV，深度为原始未配准 Y16，并通过显式策略验证或归一化为毫米 `16UC1`。上面的兼容策略使用 `timestamp_source:"host"`，对应 SDK system Unix 时间。新配置可显式添加 `acquisition:{"preset":"complete"}`，并将启用相机的 `timestamp_source` 改为 `global`；预设及高级参数详见[配置指南](configuration.md#rust-采集预设)。global 映射不代表硬件曝光同步。

相机 `parameters` 只接受两路启用标志、宽高帧率、YUYV/Y16 格式、`depth_registration:false` 和四项 `DEFAULT` QoS。曝光、增益、滤波、翻转、旋转、SDK 同步等官方驱动参数当前均拒绝，包括填写一个看似默认的值；不会静默忽略或假装已经下发。SDK 软件同步、聚合模式及 global 时间启用只通过 `rust_capture.acquisition` 受控设置。USB 后端、曝光/滤波等仍沿用 SDK 自身行为，未实现的设置不能通过管理入口承诺改变。

`gemini_305` 为 Rust 专用型号档案，精确识别 Gemini 305，不把相近型号混绑。选择官方后端时明确拒绝该档案。原有 Gemini 335L/Gemini 2 档案保持兼容；某型号能通过配置校验不等于完成该设备、固件及档位的真机验收。

`shutdown_timeout_ms` 在管理流程中最多 5000 ms，以容纳现有预览容器的有界停止流程。其他 supervisor 限制见[常驻服务参考](../development/capture-service.md)，超限或相互矛盾的 deadline 会被拒绝。

## 构建与不可变生成版本

运行现有 `geminidepthport build` 或 Web 构建操作时，管理层根据后端选择 `capture.Dockerfile`。该文件从公开 OrbbecSDK_ROS2 固定提交 `c153462518ad674650bafa4464fda72c27ab797a` 获取 SDK，校验预期版本为 2.10.6，再构建本仓库 Rust 常驻服务及薄 ROS bridge。不会复制宿主机任意 SDK 目录。对应源码许可、NOTICE 随 SDK 一起保留并记录摘要；这不是完整镜像许可证审计或完全锁定 APT/base-image 的可重现构建声明。

镜像包含 `/opt/gdp-capture/runtime.json`，记录固定公开来源、SDK 版本、许可证文件摘要、SDK/bridge/service 三个二进制摘要及运行文件清单。生成配置时，管理层通过无网络、无设备挂载的临时容器读取该清单，并绑定不可变 image ID。

每个生成版本为相机写入私有 `capture.json`、`runtime.json`，与原有 `camera.yaml`、`health.json`、`binding.json` 一起纳入摘要清单。常驻服务的 generation、状态文件路径、SDK 和 bridge 路径由管理层生成，不能通过官方参数绕过。启动前验证文件摘要和三个实际运行二进制摘要，再打开设备。

Web 预览使用同一常驻服务，JPEG bridge 仅订阅其实际 ROS 输出。确认档位时保留原有真实出帧探测，并额外绑定 image ID 和 runtime 摘要。即使镜像 tag 不变，只要重建后运行产物变化，旧确认也不能用于正式启动，必须重新预览确认。

## 管理能力

逐台启动、整组复检、停止、录制保护、CLI/Web 共用操作锁和 systemd 流程保持原有入口。Rust 镜像保留 `/app/health.py` 入口，但先后两次检查私有服务 status 的 generation、配置摘要、session、工作/父进程存活、heartbeat 与最近配对时间；两次检查之间执行 `/app/ros_health.py` 实际订阅 Image，检查布局/尺寸、帧率、时间戳推进、源 Unix 时间的新鲜度（含最多 100 ms 未来容差）及 QoS。session/PID 切换或 heartbeat 冻结会失败；`status.json` 中的在线标志不能代替消费侧健康证明。官方镜像保持原有默认时间策略。

标定导出继续读取已经发布的 CameraInfo 和静态 TF，不重新打开 SDK。深度到彩色外参由同一 SDK 会话的实际帧档案提供，不使用型号常量。预览服务的 TERM 会交给常驻 supervisor，由它有界停止工作进程和 SDK。

CLI `scan` 可用于设备发现；Rust 镜像不包含官方 `list_devices_node`，因此 `devices` 命令会明确要求使用 `scan`。

回退时先停止相机和预览，移除 `rust_capture`，将后端改回 `official`，重新构建/选择官方镜像并完成档位确认。切换后端会使旧确认失效；录制保护存在时不能切换或维护。

## Humble 跨容器通信约束

Rust 后端生成的正式服务和预览容器使用 `ROS_LOCALHOST_ONLY=0`，同时必须加载本镜像的 `/app/fastdds.xml`：关闭 builtin transport，显式 UDPv4 且 interface whitelist 仅允许 `127.0.0.1`。因此保持本机 loopback UDP 契约，不把 DDS 开放到外部网卡。消费者也必须使用相同 XML、RMW、domain 和 `ROS_LOCALHOST_ONLY=0`。

该组合用于避免 Humble RMW 在 `ROS_LOCALHOST_ONLY=1` 时隐式添加 SHM 导致 private IPC 容器间“图可见但消息不可达”的问题。不能只改环境变量而遗漏 loopback whitelist。官方默认管理生成行为保留兼容；同口径对照测试需显式配置两侧实际 transport。XML 纳入 runtime 摘要，修改后必须重新构建运行清单及完成档位确认。上游行为见
[Humble RMW participant 初始化实现](https://github.com/ros2/rmw_fastrtps/blob/humble/rmw_fastrtps_shared_cpp/src/participant.cpp#L151-L160)。

## 验证与诊断

构建和无设备检查见[贡献指南](../../CONTRIBUTING.md)，完整管理验收工具见
[验证工具](../../tools/validation/README.md)。硬件结果见[验证索引](../validation/README.md)。

设备忙、USBFS 缓冲、通信和重试问题见[故障排查](troubleshooting.md)；
服务不会自动修改宿主机 USB 参数。真实 SDK 错误只保存在私有诊断文件中。
