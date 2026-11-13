# 固定 checkpoint 8 MiB／6 MiB 对照

此实验使用固定 `61642b8` 的独立源码副本，三个 writer 在同一 CLOCK_MONOTONIC 屏障启动，每组全部使用相同分派目标。两个启动器共用同一实验二进制，只选择固定 8 MiB 或 6 MiB；不混入首次 3／5／7 错峰或 sync_data。8 MiB 保留原有字节阈值触发语义，生产默认和源代码不变。

仍使用 half-credit 的 16 对分派目标、32 对／16 MiB 总未确认额度、16 对 FIFO 和 sync_all。pending 未完成时允许 active 在总额度内继续增长，实际块可能超过分派目标；目标不是新增硬限制。payload／index／directory／checkpoint 屏障、两轮实际读回、逐帧哈希、IMU 校验和恢复协议保持原实现。

## Linux 执行

在检出的工具仓库根目录执行。需要 Git、Rust/Cargo、Python 3、C 编译器。目录必须不存在；旧失败证据和 SHA256 原样保留。启动器依赖旁边的 phase.elf，不要单独移动。

```sh
python3 scripts/validation/fixed_checkpoint_target.py \
  --revision 61642b8 --output /srv/sdk-test/fixed-target-build-01
python3 scripts/validation/fixed_checkpoint_target_run.py \
  --build /srv/sdk-test/fixed-target-build-01 \
  --output /srv/sdk-test/fixed-target-faults-01 --faults
python3 scripts/validation/fixed_checkpoint_target_run.py \
  --build /srv/sdk-test/fixed-target-build-01 \
  --output /srv/sdk-test/fixed-target-600-01 \
  --seconds 600 --repeats 3 \
  --max-output-bytes 824633720832 --max-wall-seconds 24000
```

性能工具交替执行两种目标、三轮重复，共六组三 writer。每组每 writer 至少 600 秒，整套远长于 600 秒。容量预算保守复用已有工具，包含额外余量；768 GiB 是上限，不是空间保证。容量不足应停止，不能删除旧证据或缩短时长后计为验收通过。开发环境可用新目录、`--seconds 1` 或 `5` 验证工具，不能替代原 MiniPC 同盘长测。

任一组失败，保留原始子结果和可用观测，停止后续组，不补记成功。summary 区分 attempted_groups 和 completed_groups；一组只有全部 writer、完整性、共同驱动及观测检查通过才计入 completed_groups。

## 故障与恢复

四项故障用例在两种目标的第二次索引 fsync 分别注入 400 ms 延迟或 EIO。必须有命中标记；延迟后执行真实同步。EIO 必须传播为失败，不能生成 COMPLETE，此前已确认前缀须完成两轮恢复读取、哈希和 IMU 检查，恢复前后文件 SHA256 相同。延迟用例额外要求实际出现全局 credit 等待和非零队龄；同时核对实际目标、首批和有界观测，见 fixed-target-fault-verdict.json。

正常六组全部通过后，工具对两种目标分别执行独立 SIGKILL 恢复，恢复中同样两轮读回并核对原文件不变。若性能组失败，这些恢复未执行，不能判为通过。另用 `--recovery-only` 和新目录可独立做软件恢复回归，不补记失败性能批次通过；该入口结果是 recovery-summary.json，文件哈希在 source_unchanged_sha256 中。

正常组分别验证各自产物哈希、相同输入和 snapshot 哈希，以及两轮逐帧内容。两种目标改变 chunk 边界和目录文件，不能要求所有目录文件哈希跨布局相等。

## 观测与性能解释

记录器在启动屏障前预分配，事件有界、收尾后统一导出，不在采集热路径添加睡眠或逐事件落盘。两种目标采用相同实现，仍有内存、时间戳和短锁开销；6 MiB 可能产生更多事件，因此同实现不代表等量观测开销，更不代表零扰动。

- 每批实际帧数、字节数、分派时间，包含超过软目标的实际大批次。
- recording 时间窗内分派次数／秒；全生命周期实际 fsync 调用数另列，包含启动与收尾，不能混为纯采集同步频率。
- 每次 credit 等待起止、原因、pending／active 帧数与字节快照。准入快照不包含待追加的下一对。
- 全局 credit 等待按间隔不超过 33,333,334 ns 合并成连续 episode，保留跨度、等待总时长和次数。这个阈值只是分析定义，可从原始事件重算，不是额外超时或心跳设置。
- 每次 producer 入队前的队列深度、最老队龄和序号，包括溢出前的满队列；与等待前后最近样本关联。样本不是与 checkpoint 原子获取，也不是连续队龄上界；收尾阶段的实际出队等待另看 queue_wait_p99_ns／queue_wait_max_ns 和原始子结果。

相位事件上限 131072，队列样本上限 108000。丢失计数非零不得计为完整证据。正常组要求所有 pending 等待闭合和 checkpoint 提交完整；失败组保留未闭合等待、失败状态和原始观测，不把缺失尾部当零耗时。

回传 build-manifest、实际补丁、编译器和二进制 SHA256、构建／子日志、manifest、summary、每组 JSON、每组 phase.json、资源采样、四项故障结果与恢复目录、SHA256SUMS，交付包另附 SHA256。比较 CPU、追加 P99／最大值、验证耗时、RSS、吞吐、实际批次分布、同步频率、连续等待、队列深度／队龄、溢出和完成量，不只比较均值或失败时间。

这是无 SDK／USB／ROS、无生产状态持久化背景的合成存储实验，不验证心跳时效、真实相机或物理掉电。生产心跳及采集路径未改。原 MiniPC 复测交由测试者；失败推迟或短测通过都不能宣布关闭 RAW_QUEUE_OVERFLOW。

## 本地验证（2026-11-14）

Linux Docker overlay 上每 writer 5 秒、150 对，交替三轮，共六组通过；8 MiB／6 MiB 的独立 SIGKILL 恢复均通过。两种目标各九个进程结果的追加 P99 中位数为 4.141／2.630 ms，CPU 秒为 0.8068／0.8197，验证耗时为 263.052／217.876 ms；三进程采样 RSS 峰值的轮次中位数为 17.023／17.043 MiB。此轮没有全局 credit 等待，不能证明持续录制长尾得到缓解。

8 MiB 的实际批次为 21 批 7 对加末尾 3 对，6 MiB 为 30 批 5 对。每进程 recording 窗口内分派频率为 4.2／5.8 次每秒，全生命周期 fsync 为 116／156 次。吞吐及完整明细见 [metrics.json](reports/fixed-checkpoint-target-20261114/metrics.json)。

四项最终故障用例通过。第二次 index fsync 单次延迟 400 ms 时，8 MiB／6 MiB 分别实际记录约 174.456／105.271 ms credit 等待、132.424／66.218 ms 入队前最老队龄。6 MiB 用例出现 pending 5 对＋active 9 对，随后封存 9 对块，验证软目标不保证每批固定 5 对。EIO 两例此前前缀均恢复通过且原文件不变。上述注入只证明观测与回归覆盖，不证明实际磁盘长尾具有相同分布。

结果、实际补丁、故障观测、恢复摘要及 SHA256 见[本轮归档](reports/fixed-checkpoint-target-20261114/performance-summary.json)。原始组 JSON 和故障 JSON 以 gzip 保存，另附解压后原始字节 SHA256；压缩不改变证据内容。初步 100 ms 注入结果和全部原始录制另外保留在本地新目录，旧实机证据未修改。生产继续默认 8 MiB，至少 600 秒原盘复测待测试者执行。
