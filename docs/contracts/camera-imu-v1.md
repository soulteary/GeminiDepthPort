# Camera IMU v1

Rust 采集后端可显式开启相机加速度计和陀螺仪。省略 `imu` 保持原有行为：不解析
IMU SDK 符号、不创建 IMU 回调或发布器，也不向录制索引添加空字段。官方后端不接受
本节的原生 IMU 配置；已有官方驱动参数不属于这个新增接口。

## 配置与能力

Inventory 配在单台 `cameras[].imu`，生成的 capture-service/rgbd-probe 配置使用顶层
`imu`。它受配置摘要、generation 与真实运行时快照约束；修改后需重新确认配置。
模板见 [inventory.imu.example.json](../../examples/rust-single-camera/inventory.imu.example.json)，
仍保持相机未绑定、禁用与未确认。

```json
{
  "enabled": true,
  "accel_rate_hz": 200,
  "gyro_rate_hz": 200,
  "accel_range_g": 4,
  "gyro_range_dps": 1000,
  "queue_capacity": 128,
  "stale_timeout_ms": 500,
  "accel_frame_id": "camera_a_accel_frame",
  "gyro_frame_id": "camera_a_gyro_frame"
}
```

保留配置但关闭功能可将 `enabled` 设为 false。所有列出的字段仍须完整、合法，未知字段
会被拒绝。启用时需要显式 acquisition 策略以设置 SDK realtime host clock，并确定
system/global 消息映射。IMU 使用同一消息时钟策略；global 不可用时不会回退到 system。

采样率可请求 25/50/100/200/400/500/800/1000 Hz；加速度量程为
2/3/4/6/8/12/16/24 g，陀螺仪量程为 16/31/62/125/250/400/500/800/1000/2000 dps。
这是接口允许的请求集合，不表示每台设备都支持。实际通过同一个已打开设备查询传感器，
按精确量程和采样率选择 SDK profile；设备、符号、档位或任一路启动失败都终止请求。
不根据型号名称伪造能力，不选择默认/最近档位。默认配置和已有相机不会自动开启 IMU。

队列容量为两路合计 16–128 个样本，stale timeout 为 100–5000 ms。启用配置要求
`(accel_rate_hz + gyro_rate_hz) * poll_ms * 2 <= queue_capacity * 1000`，至少保留两轮
轮询的容量。两个 frame_id 必须独立、合法，并与全部相机的图像和 IMU frame_id 区分。

## SDK 与发布

视频仍通过原有 Pipeline 轮询。IMU 使用同一设备的两个 sensor 回调，不加入 RGB-D
frameset，不等待颜色/深度配对，不插值、不融合、不改变图像配对或硬件同步设置。
每个回调复制有限的 XYZ、序号与设备/system/global 时间戳后释放 SDK frame 引用。
同类样本的序号及各个可用时钟必须严格前进，数值必须有限；SDK 原始序号保留用于核对。
这不证明设备在回调前没有丢样，也不承诺跨传感器同时曝光。

SDK 回调只操作有界内存队列。采集线程每次 SDK wait 前后排空它，ROS 发布发生在
原有线程，避免跨线程共享 SDK 或 rclcpp 句柄。队列溢出、非法/倒退样本、单路停流、
发布失败均为终止故障；启用 IMU 后需要两路均收到样本才可报告会话 ready。
发现阶段样本不发布、不进入原生录制，并有单独排除计数。
`received` 表示通过基础校验并进入队列的样本，`published` 是发布调用成功数；
故障锁定后的计数不能用于声称回调数据完整，发布成功也不等于下游已收到。

发布两个 `sensor_msgs/msg/Imu` 话题：

- `<namespace>/accel/sample`：linear_acceleration，单位 m/s²。
- `<namespace>/gyro/sample`：angular_velocity，单位 rad/s。

SDK 的 XYZ 已是 SI 单位；配置中的 g/dps 仅用于请求满量程，不再次换算 XYZ。
保持各自传感器坐标轴，header.frame_id 使用显式配置。没有输出姿态或未经验证的
相机到 IMU 外参/TF。orientation_covariance[0] 与未测量向量的 covariance[0] 为 -1；
已测量向量 covariance 全零表示未知协方差。不要把占位 quaternion 当成姿态估计。

桥接在创建时检查实际 QoS：reliable、volatile、keep-last(100)。DDS graph 不一定暴露
history depth，因此测试用桥接本地 actual QoS 检查深度，并在接收端验证可观察的
可靠性/持久性。新增符号使用可选扩展，不改变旧图像 ABI v1 的结构布局。

SDK stop 可能阻塞，进程监督仍执行原有退出期限。SDK 会先复制回调列表再分发；
只有两路全部停止才释放 userdata。任一路 stop 失败时，保留整个有界 IMU 会话到
进程退出，避免迟到回调访问已释放内存或已卸载库。回调不允许 panic 穿过 C ABI。

## 原生录制与恢复

内置录制仍记录有效的完整 RGB-D 对。每条 RecordingRecord 可带 `imu` 数组，包含前一
成功提交到录制队列后收到的原始 IMU 样本；它只表示接收批次，不是曝光配对或同步证据。
空数组不序列化，旧 V2/V3 目录仍可读取。旧版严格读取器会拒绝含新字段的目录，避免
静默丢弃 IMU。当前导出器把样本保留在 `recording-record-*.json`。

每批最多 128 个样本，整条 JSONL 索引仍受 64 KiB 限制；样本和配置绑定，不能把没有
IMU 声明的快照用于 IMU 录制。序号、时钟、frame_id、有限数值、global 映射及跨记录进度
都在写入和读回时验证；声明启用但两路样本缺失不能完成。索引 SHA256 覆盖样本，
`max_bytes` 计入其序列化内容。原生队列同时拥有图像和 IMU 批次，后台写盘不借用 SDK 内存。

V3 原有 payload/index/fsync/检查点顺序、两轮实际读回、最终完整验证和发布 guard 保留。
流水线检查点还保留 IMU 的验证进度。恢复只读取已提交索引前缀，因此 IMU 与所在图像
记录一起恢复；后缀不能伪装完整。`validation_imu_samples` 记录实际验证数量，正常完成
应等于录制样本数的两倍。队列待处理样本和录制结束后排除样本有独立计数。

该录制方式不是纯 IMU 录制器。原生录制不能和图像 `complete`/preserve 策略混用；
使用原生录制时选择 `strict-rgbd`。ROSbag2 可以独立记录图像和两个 IMU 话题，IMU 不依赖
完整图像对。实际丢样判断仍需核对保留的 SDK 序号、下游接收/落盘计数和设备证据。

## ROSbag2 验证

在 ROS Humble 环境中可执行：

```sh
python3 ros/imu_recording.py --bag /absolute/capture --namespace /camera_a
```

只读校验 SQLite3 ROSbag2 的两路类型、样本非空、逐流严格递增时间戳、稳定 frame_id、
有限数值及未测量/未知协方差标记。默认报告内容摘要，不能单凭摘要证明数据未被篡改。
提供 `--expected /absolute/imu-evidence.json` 时，逐条比较独立采集证据中的
`topic`、`stamp_ns`、原始 CDR `sha256`，并要求样本集合完整匹配。

`ros/tests/smoke_imu.py` 使用实际 DDS 接收、ROSbag2 写入/读回和逐条 CDR 哈希证据，
还会修改 SQLite 中一条消息，确认验证拒绝损坏。它是无硬件合成验证，不是实机验收。

## 配置缓存与计时

SDK 回调和 ROS 发布器在创建时分别构建不可变的已验证 IMU 配置绑定。原生录制
写入器及每次新开的读回器，都从各自经过内容哈希验证的快照独立解析并校验绑定；
该缓存不随外部配置变化，也不复用另一份快照的绑定。启用的 IMU 配置或 acquisition
无效时，在创建写入器/读回器时失败。禁用 IMU 时不要求 acquisition。

缓存仅省去固定配置的重复校验和反序列化。每条样本仍检查有限数值、绑定 frame_id、
时钟范围、序号和各来源时钟严格递增；选择 global 时仍要求每条样本具有 global 时间戳。
两轮实际读回、索引/图像哈希、检查点与崩溃恢复规则不变。

- `pipeline_timing.sdk_wait_call`：仅 `pipeline.wait` 调用，包括超时和错误；不含 IMU
  处理或录制缓冲。有限采集按调用开始时所处的预热阶段归属该次等待。
- `imu.drain_wall`：每次 pump 的计数读取和 SDK 队列排空，包括锁等待及失败尝试。
- `imu.ros_publish_wall`：每个非空、允许发布的批次内所有同步 ROS 发布调用，包括失败
  的调用；未配置 ROS、预热排除或空批次不会增加其样本数。该分位数单位是批次，不是单条 IMU。
- `imu.pump_wall`：完整 pump 耗时，包含排空、发布和统计开销，不含原生录制器缓冲。

这些计时使用单调时钟，均未声明 DDS 下游送达。旧版本 `sdk_wait_call` 含前后两次 IMU
pump，跨版本比较须使用整条路径指标，不能把拆分后的等待缩短解释为 SDK 性能提升。

## 来源与验证边界

ABI 依据 [SDK v2.10.6 C headers](https://github.com/orbbec/OrbbecSDK_v2/tree/v2.10.6/include/libobsensor/h)，
回调生命周期依据同版本 Sensor/ImuStreamer 实现；单位依据
[官方 IMU 读取示例](https://doc.orbbec.com/documentation/Orbbec%20Gemini%20330%20Series%20Documentation/C%2B%2B%20Sample%20IMU%20Reader)。
部署 SDK 仍来自现有固定 OrbbecSDK_ROS2 提交，未替换厂商二进制。

配置缓存优化与计时拆分的前后对照见 [配置缓存性能报告](../validation/reports/imu-config-cache-ab-20261030.md)。

性能影响需区分合成回调/发布边界开销与真实 USB、SDK 调度和 DDS 传输成本。
本轮数据见 [IMU 性能报告](../validation/reports/camera-imu-ab-20261030.md)。本机没有连接
实际相机，物理采样率、轴向、噪声、硬件时间一致性与长时稳定性仍须实机验证。
