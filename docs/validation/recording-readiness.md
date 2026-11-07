# 录制可靠性修复与部署验收

基线：`8697b4fe1111ad0c99da2a14c961d1e0e39570eb`。本轮正式实机结果仍为 0/6，独立相机恢复未执行。软件测试或 CI 通过不能改写这一结果。

## 本轮改动与边界

原半额度流水线保持默认。增加显式 `raw_recording.checkpoint_batching="full-credit"` 候选模式，必须同时设置 `storage_format="chunked-v3"` 和 `checkpoint_pipeline=true`；不允许无效配置静默生效。设置为 `half-credit` 可回到原策略。不指定新字段时生产行为保持不变。

完整额度模式把封块目标从半额度改为既有完整额度；640×480 YUYV 与 640×400 Depth16 通常由每块约 7 对变为约 14 对。全局未确认预算仍为 32 对／16 MiB（保留原有单对不可分割超大帧例外），FIFO 仍最多 16 对，pending 仍最多一个。它减少同步事务次数，但降低流水线重叠，可能增加追加尾延迟和确认间隔；不保证能容忍任意长的 fsync，不默认推广到生产。

payload sync、index sync、directory sync、checkpoint 文件 sync、rename 后 directory sync 的顺序和错误检查不变。结束全段第二轮实际读回、逐帧哈希、IMU/图像 device/global 校验、持锁最终提交守卫、心跳期限和崩溃恢复均沿用现有实现。v3 文件格式未改变，原 reader 可读取新批次布局。

合成实验新增有界 `checkpoint-events.jsonl`，持续保存完成的 checkpoint 阶段，而不是只依赖最后 64 条内存历史。每 writer 上限 16 MiB，单行上限 64 KiB；事件 ID 必须从 1 连续，重复快照不重复写，历史缺口、尾部缺失、写入失败或超限使诊断失败。原 64 MiB 状态采样上限独立保留，总预算已增加对应预留。该线程只在实验适配器中；生产采集热路径没有新增文件写入。

`writer_long_run.py --checkpoint-batching full-credit|half-credit` 在每个新目录记录请求策略，并核对实际策略、阶段文件大小和连续 ID。驱动、来源、队列、状态和同步合同仍需逐项通过。它没有 SDK/USB/ROS，不能作为相机验收替代。

## 源端缺帧与存储门禁

正式 `run_retest.py` 默认 `--source-gap-policy reject`，逐组传递给 A/B 和单组执行器。真实读回与 DDS 匹配通过之后，仍须 SDK 源序号无缺口；布尔字段必须是真正的 JSON boolean，字符串不能当作通过。`report` 仅用于明确接受缺口的诊断；即使全部流程成功，`strict_600s_acceptance_passed` 仍为 false。该门禁不能修复 SDK 未输出的深度帧，也没有补帧或伪造连续序号。

冻结配置后、启动相机前生成 `storage-preflight.json`：按实际 profile、目标帧对、全部组数，预留原生数据、恢复运行、25% 余量及 1 GiB 固定余量。DDS 数据库仅保存哈希／元数据，按观察器现有 4096 MiB 上限及等额 DELETE rollback journal 预留，不按像素副本或 WAL 错算。空间不足保留证明并拒绝启动，不删除旧数据。它是保守估计，不是配额，不能保证其他进程或 VM backing 文件不会额外占用空间。VM loopback 还必须核对宿主盘空间；不能只看 ext4 镜像内部的可用容量。

合成控制器在启动和运行中额外保留 2 GiB 可用空间；有可见宿主目录时用 `--backing-path /visible/host/directory` 同时检查 backing 文件系统。周期性检查不是预留配额，无法覆盖检查间隔内突发占用。避免在共享宿主盘上创建超出实际预算的大型可写 loopback 镜像；本轮出现的 backing 空间异常不能用作 writer 性能结论。

trace summary v2 使用 256 KiB 有界输出缓冲，仍保留原有内核缓冲上限、输出上限和两秒最终排空期限。解析／排空错误与读取 per-CPU 丢失计数器分开处理；`loss_counters_complete=false` 时 `lost_events=null`，不能转成 0。原始错误保留，新增 `final_drain_error` 独立记录收尾错误。`state=complete` 仍需完整计数器、无丢失、有效配对与完整尾部；旧失败输出不重新计算或改写。

## 现场执行

使用包含本次改动的新镜像、固定 revision、干净源码和全新输出目录。先执行[三 writer 工具](writer-long-run.md)，两种批次策略分别建新 root；每组不少于 600 秒，失败立即停止本计划，保留失败原因和原始数据，不续跑旧 root。完整实验必须包含状态 on/off、连续无丢失追踪及两个独立合成 SIGKILL 恢复。需要完整额度候选时显式指定：

```bash
sudo bash scripts/validation/run_writer_long_test.sh \
  --binary /absolute/path/to/NEW-release-driver \
  --output /srv/sdk-test/NEW-full-credit-writer-600 \
  --checkpoint-batching full-credit --seconds 600 --repeats 3 --disk nvme0n1 \
  --max-output-bytes 549755813888 --max-wall-seconds 14400
```

候选只有在正常负载、重复对照、原生读回及故障恢复均通过后才进入实机试用。正式相机流程保持三轮、每组 600 秒、源缺口拒绝及独立恢复；例如：

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/run_retest.py run \
  --plan /srv/sdk-test/three-camera-plan.json --image NEW-IMMUTABLE-IMAGE \
  --root /srv/sdk-test/NEW-three-camera-full-credit \
  --checkpoint-batching full-credit --source-gap-policy reject \
  --rounds 3 --first on --seconds 600 --camera b --disk nvme0n1 \
  --max-bytes 137438953472 --ros-domain-id 78 --execute
```

`--max-bytes` 是录制器预算，不能据此推断六组所有数据只需 128 GiB。以实际 `storage-preflight.json` 为准，准备足够容量的目标盘。

只有六组全部通过、正式独立恢复通过、源序号无缺口、Global/IMU/共享 ROS context、原生 counts 和 ROS 归一化深度、双轮读回及哈希均完成，且清理成功，才能得到 `strict_600s_acceptance_passed=true`。这仍不是比 600 秒更长任务的耐久保证；正式任务最长时长及设备断连、空间不足、重启和回滚仍需按部署要求验证。

该字段的缺口拒绝范围是六组正常采集；独立恢复沿用已确认前缀、双读回、哈希、时间戳及 IMU/存活检查，不新增“恢复试验的 SDK 源序号绝无缺口”承诺。恢复试验的源序号连续性仍需单独核对，不能把该字段作为整个系统无缺帧的证明。

## 本地结果

本轮验证记录见 `reports/recording-readiness-20261107/`。明确区分受控延迟回归、VM 正常负载、环境失败和正式硬件验收；任何未完成计划均不计作通过。旧 `8697b4f` 及更早失败证据保持原样。

后续针对提前反压与索引 inode 争用的修正、内存代价及回归范围见[checkpoint 重叠说明](checkpoint-overlap.md)；原始本轮失败结论及归档不改写。
