# 接收长尾的分阶段诊断

本工具用于区分 SDK 等帧、发布前处理、ROS 发布与接收、回调及落盘阻塞。
它不把源帧连续性当作低延迟，也不把发布成功、write 成功当作下游收到或持久化完成。
现有实测约 355 ms 的接收长尾仍需用新观测链路重新测量；合成测试不能替代该验收。

## 采集端观测

升级采集服务和 ROS bridge 后，服务状态增加 `pipeline_timing` 与
`status_persistence`。不需要修改消息的 global/system 来源。

- `sdk_wait_call`：SDK wait 调用单调耗时，包含超时和错误；另计两种结果次数。
- `sdk_success_return_interval`：成功返回 frameset 的间隔；不是曝光间隔。
- `between_sdk_polls`：前次 SDK 返回到下次调用间的处理时间，可发现采集线程被处理或写状态拖住。
- 每路 `sdk_system_to_return`：SDK 接收时刻到返回采集线程的差。仅 SDK realtime 时钟经检查后计算；未知或负差明确计数。
- 每路 `sdk_return_to_publish_start`：SDK 返回后的处理时间；
  `ros_image_publish_call` 包含 Rust/native bridge 的同步发布工作，不能解释成 DDS 已送达。
- `quality_tick_wall` 与 `status_persistence.wall`：质量处理、状态持久化耗时。
  状态文件中的持久化统计仅包含该快照之前已完成的写入，当前写入在后续快照体现。
- `derived_pair_queue`：派生配对队列当前深度、高水位、淘汰及等待分布。
  完整采集图像已独立发布，配对队列不是全部原始图像的排队位置。

Rust 分位数使用固定内存直方图，返回上下界，最大值为实测值；没有样本时是 null。
SDK 和 DDS 内部队列没有可用的深度观测接口，其 current/drop 数保持 null，不能当作零积压。

每帧 ROS metadata 增加 `sdk_return_unix_ns`、`publication_start_unix_ns`、
`image_publish_finished_unix_ns`、`image_publish_wall_ns`、`timing_clock`、
`sdk_system_clock_verified`。前三者用于跨进程时间关联，wall 是单调时钟测得的
Image publish 调用耗时；SDK 返回时刻必须来自 wait 返回处，不能用转包时刻替代。
旧 ABI 保持兼容，带 SDK 返回观测的新路径要求 bridge 提供 observation 扩展。

## ROS 接收观测与诊断记录

在同一 ROS Humble、Fast DDS 配置、Domain 和主机时钟环境中运行：

```sh
python3 -m runtime.pipeline_diagnostics --namespace /camera_a --seconds 120 > timing-observe.json
```

上述模块命令在已加载 ROS 环境的仓库根目录执行；在采集镜像内可改用
`python3 /app/pipeline_diagnostics.py`，参数相同。

工具分别订阅彩色、深度 Image 和 frame_metadata，无需完整帧对或 pair_metadata。
可按三台相机分别运行。记录回调间隔、回调自身耗时、发布开始到接收的差、SDK 返回
到发布的差、SDK 接收到返回的差，以及配对元数据的缺失/歧义计数。
关联依据为 stream/header/frame_id 加发布方报告的 session/sequence；Image 本身没有
独立 session 字段，因此报告不会把这种关联升级为独立验证的 SDK 同帧集证据。

global header 的年龄代表映射后的采集年龄，system header 的年龄代表主机接收后的年龄，
二者分开报告。跨进程差要求同一 Unix 实时时钟；SDK system 年龄还要求
`sdk_system_clock_verified=true`。订阅者可能在 publish 返回前运行，所以
`image_publish_finish_to_subscriber` 可出现负值，负差单列统计，不钳为零。

要观察诊断落盘，可使用一个尚不存在、父目录已存在的新私有目录：

```sh
python3 -m runtime.pipeline_diagnostics --namespace /camera_a --seconds 120 \
  --output-dir /data/gdp-diagnostic-run-a \
  --queue-capacity 32 --queue-bytes 134217728 \
  --sync-every 30 --sync-interval 1 > timing-disk.json
```

回调只复制需要保留的载荷并尝试入队，磁盘写入和 fsync 在后台执行。队列条目数
和 queued+inflight 载荷字节均有上限，另报告最老排队年龄、高水位、溢出、等待、
write/index_write/sync 耗时。溢出使该诊断记录失败，不会静默覆盖图像或宣布完整。
Python 对象、DDS reader 缓冲、待同步文件描述符不是该载荷字节预算的一部分。

输出为独立 `.raw` 图像及 `frames.jsonl` 索引/metadata 事件。只有载荷、索引和目录
最终同步完成才报告 `completed=true`；write 完成数不等于 durable 数。
这是诊断格式，不是 native V2、rosbag 或原 R2 的内部观测，也不证明源数据无丢帧。
默认仅观察模式不会保存图像。

每秒读取一次当前 `/proc` 视图中的 I/O PSI、Dirty/Writeback、回收与 OOM 计数、
观察器自身 I/O。只保留有界近期窗口及最大 stall 窗口；缺失为 unknown，而不是零。
PSI/vmstat 可能是系统级数据，`self/io` 只属于诊断进程，不可直接归因到 R2 或某块磁盘。

## 对照测试与判读

保持相机模式、时间来源、ROS/DDS 配置和采样窗口一致，分别测量：

1. 原 R2 停止，只运行观察器，确定 SDK/发布/接收的基线。
2. 诊断记录写入有容量约束的 tmpfs，检查复制、回调与排队开销。
3. 相同诊断记录参数写入实际磁盘，检查 writer 等待、sync 与 PSI/Dirty 波动。
4. 原 R2 单独运行，再带仅观察器运行，检查额外订阅者的影响。

诊断记录器和 R2 的实现不同。tmpfs/磁盘对照只能定位这一诊断路径的差异，不能直接
证明 R2 的 fsync、缓存回收或线程调度机制。若源时间连续、SDK wait 正常但
between-polls 上升，优先查看采集线程处理和状态写；若 publish wall 上升，检查本地
bridge 与 DDS 背压；若发布正常而接收间隔/年龄上升，检查订阅者调度及中间件；
若回调正常、writer queue/同步耗时上升，再结合 I/O 压力定位存储路径。

本次已减少显式完整采集合法 partial frameset 的重复同步状态写：稳定状态按心跳
合并，首次、状态变化、失败、保护和停止仍走及时写入，逐帧诊断计数保留。
原生 V2 recorder 提交阶段也不再持状态锁跨 fsync。两项改动都有慢 I/O/状态测试，
但不能据此宣称实机 355 ms 长尾已经消失。
