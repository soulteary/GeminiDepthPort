# IMU 错误收尾与关键状态观测

## 背景与范围

2026-11-02 的 e8912cb 实机复测中，on-01 通过；off-01 的 B/C 先记录 IMU_QUEUE_OVERFLOW，后发生 NATIVE_HEARTBEAT_TIMEOUT。官方结果仍为 accepted_groups=1/6，后续四组及独立 SIGKILL 恢复未执行。本修订不修改该批次的附件、录制、日志或汇总，不把检查点计数当成完整验收，也不推断首次 IMU 消费停顿的唯一原因。

## 行为变化

采集循环失败后，先阻止尚未通过提交守卫的原生录制完成，再同步等待失败状态可靠落盘，随后停止 IMU、SDK pipeline 和等待录制线程。IMU 停止失败及外层打开/停止失败也进入该失败状态路径。失败期间 ready=false；监督器仍可按原有心跳期限终止阻塞的工作进程，并在 worker_reason 中保留 SESSION_FAILED，保留已发布的 IMU/录制/收尾指标。

这没有延长心跳或退出期限，没有增加 IMU/图像队列容量，也没有给等待线程额外发送虚假采集心跳。若失败状态自身的存储操作阻塞或失败，无法承诺它已经持久化，监督器原有有界退出仍生效。若原生事务在故障之前已通过提交守卫，之后的采集错误不会将已提交录制伪装为未提交。

正常录制的两轮实际读回、逐帧哈希、严格时间戳验证与崩溃恢复流程不变。错误期间正在执行的读回不会被跳过以提前宣布完成；提交守卫仍拒绝已中止事务。

## 字段与读法

- `status_persistence.queue_wait`：状态批次入队至持久化线程取出的等待；`wall` 是原有批次落盘耗时。
- `status_persistence.critical_wait`：关键提交进入至等待返回的耗时；最近 16 个 `critical_events` 保留提交/完成序号、目标外层状态和原生状态、进入/返回 Unix 毫秒、wall_ns 和结果。普通心跳不增加该历史。
- `status_persistence.active_write`：生成快照时持久化线程正在写的序号和已耗时。字段与其他状态一样是快照，并非持续刷新；阻塞中的当前关键提交可能尚未出现在已落盘的历史中，不能将旧 span 自动延长为磁盘延迟。
- `imu.drain_interval`：相邻排空调用开始时刻的间隔，包含期间 SDK 等待、ROS 发布、状态提交等工作，不等于锁等待。
- `imu.failure_observation`：仅错误路径获取队列快照；记录容量、当前队列长度、首次回调错误时的长度与距上次成功排空时间。`first_error_elapsed_ns` 相对 IMU 启动的单调时钟；`started_unix_ns` 仅作跨记录关联锚点，不用于校验传感器时间戳。非回调类错误可没有首次回调错误时间。
- `cleanup`：最近 8 个已结束阶段与当前阶段的快照，包括 imu_stop、pipeline_stop、failure_record、recorder_join、quality_finalize、complete。时间是 wall time，不能直接等同 CPU 或物理设备 I/O。

观测使用有界内存；IMU 额外队列快照只在错误时获取。关键提交仍可靠落盘，新增字段不会增加逐样本文件写入。

## 回归验证

`slow_critical_draining_status_exposes_imu_overflow_and_persistence_wait` 仅在包含 draining 的真实状态临时文件 fsync 上注入延迟，再启动受控 IMU 回调突发，检查队列容量 128、溢出原因、关键等待耗时与排空间隔。此测试不是实际 200 Hz 性能测量。

`imu_failure_is_durable_before_blocked_final_readback_and_never_commits` 在最终读回 manifest 时设置屏障，再注入 IMU 回调失败。释放屏障前必须已看到 ready=false、SESSION_FAILED、IMU 原因及 recorder_join；之后分别验证正常释放与保持阻塞触发原有心跳期限。两种情况均不得生成 COMPLETE；退出后两次独立恢复读回必须逐帧哈希通过且均为 31 对。

持久化单元测试另覆盖活动写入、等待失败、最近 16 条历史上限和持久化序号；收尾单元测试覆盖阶段历史上限。上述模拟不替代三相机长测或物理断电恢复验收。

## 后续实机验收

使用新版本、新输出根重跑完整三轮 on/off 和独立 SIGKILL 流程，采样持续覆盖 wait-complete/收尾，不在 600 秒性能窗口结束时停止故障观测。关联新的关键提交记录、IMU 排空间隔和首次错误时间，同时保留线程栈、磁盘延迟和 CPU profile。若状态本身冻结，保留冻结前快照与独立线程观测；不能把缺失的当前 span 当作零耗时。
