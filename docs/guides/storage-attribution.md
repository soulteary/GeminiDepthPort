# 录制同步长尾：文件系统与块层归因

`bf7690f` 原盘 half/full 长对照都在首个 status-off 组溢出。half 有串行约 331/327 ms 的同步，full 有连续约 473–490 ms 的额度等待。原追踪只有 syscall／调度事件，不能区分 ext4、jbd2、块层或设备刷新。本工具增加可选的设备追踪，不修改录制实现、队列、心跳、同步屏障、两轮读回、哈希或恢复规则。

`d15ce1c` 后续实机首组达到三路各 18,000 对后进入读回，但 writer/storage 追踪均丢事件，控制器随后中断 writer，三份最终结果缺失。该轮仍为 0/6，独立恢复未执行；未记录到 RAW_QUEUE_OVERFLOW 不能证明问题已解决。原证据、失败日志及 SHA 保持原样。

## 两路追踪与证据边界

启用 `--storage-trace` 后，在 writer ready、共同启动屏障释放前创建两个私有 tracefs instance：

- **writer 实例**：沿用 PID/start_ticks、线程代际、命名空间、单调时钟校验及 syscall／调度追踪。额外尝试读取 fsync/fdatasync 的 FD，保存设备号、inode 和有限文件角色，不输出路径或像素。
- **设备实例**：`set_event_pid` 明确清空，按 dev 字段过滤。ext4/jbd2 过滤录制文件系统设备；block 过滤整盘设备。因此包括 jbd2、kworker、软中断及同一设备上其他程序的 I/O，不限于 writer 的进程后代。

支持可直接对应到整盘／分区的 ext4。设备号由输出文件系统的 `st_dev`、mountinfo 与 sysfs 交叉核对；叠加设备、未知事件格式或能力缺失会拒绝本次设备追踪，不能用 `--allow-unavailable-trace` 绕过。外部 journal 尚未核实，不声称覆盖其他日志设备或获得 SSD 固件身份。

设备事件包括 ext4_sync_file enter/exit、jbd2 commit start/locking/flushing/logging/end，以及 block request insert/issue/complete/requeue。保留 RWBS 中的 preflush、FUA 和 flush 信息；含糊编码保留 null。统计的是带标记的事件数，不是唯一设备 flush 次数。

事件字段以本机 `format` 为准并记录 SHA256。实现参考 Linux 的 [ext4 tracepoints](https://github.com/torvalds/linux/blob/master/include/trace/events/ext4.h)、[jbd2 tracepoints](https://github.com/torvalds/linux/blob/master/include/trace/events/jbd2.h)、[block tracepoints](https://github.com/torvalds/linux/blob/master/include/trace/events/block.h) 和 [RWBS 编码](https://github.com/torvalds/linux/blob/master/kernel/trace/blktrace.c)。不凭版本号假定厂商内核一定兼容。

## 先做只读能力检查

在实际录制主机执行，目录和盘名按现场替换：

```sh
sudo python3 scripts/validation/writer_storage_trace.py --probe \
  --recording-path /srv/sdk-test --disk nvme0n1
```

probe 不创建实例、不挂载 tracefs、不安装组件、不修改全局追踪。需要已经挂载的 tracefs/debugfs 及所需 tracepoint；能力不满足时保留输出并停止，不能把缺失当零事件。

## 实机采样

先用当前提交重新构建 release `capture-probe` libtest 驱动，记录实际源码提交、工作区状态、ELF SHA256。测试代码也参与驱动构建及 source hash，本次不能复用旧驱动。历史 `baseline` 字段不是实际 checkout 提交，仍需单独记录来源。

先只执行一个独立 half-credit 实验，定位原盘长尾，不重复盲测 full。使用新根：

```sh
sudo bash scripts/validation/run_writer_long_test.sh \
  --binary /absolute/new-capture-probe-writer-long-driver \
  --output /srv/sdk-test/new-storage-attribution-01 \
  --seconds 600 --repeats 3 --disk nvme0n1 \
  --max-output-bytes 549755813888 --max-wall-seconds 14400 \
  --trace-max-bytes 4294967296 --trace-buffer-kib 256 \
  --checkpoint-batching half-credit \
  --storage-trace --storage-trace-max-bytes 4294967296
```

原有三 writer、30 FPS、200 Hz IMU 模型、FIFO16、32 对／16 MiB 全局额度及心跳保持不变。追踪失败会立即记录 `trace-failure.json` 并停止采集，当前组不能通过；健康 writer 在原有心跳、容量和总时限内完成当前录制及两轮读回，保留 result 和 COMPLETE 的实际结果，再停止后续组／恢复。writer 自身失败、心跳过期、空间或总时限不足仍立即执行原有有界清理。不要将健康 writer 的成功结果算作整组追踪验收成功，不续跑失败根。

默认新增每组最多 4 GiB 设备事件及 1 MiB 组合元数据预算，纳入总容量预检。上述六组计划从 403,904,790,528 增至 **429,680,885,760 字节**；加 2 GiB 空间保留后至少需 **431,828,369,408 字节普通可用空间**。实际按程序 plan 复核，不靠删除旧数据满足预算。

两个实例各限制 kernel ring 总量 32 MiB，合计不超过 64 MiB；`--trace-buffer-kib` 仍为每 CPU 的数值。CPU 多或 buffer 太大时会拒绝，不能静默扩大内存。输出、单行、线程／事务状态、时长、最终排空和 loss counters 都有界。任何丢失、未知格式或排空错误使设备归因不完整。

每个实例有独立持续排空线程，不依赖控制器 200 ms 轮询；单次读取最多 1 MiB、按 20 ms 预算让出，空闲时最多等待 20 ms。原始片段和解析状态有界，没有无界用户态事件队列；Python 线程仍共用进程和 GIL，不保证所有内核事件速率下无丢失。FD 查询全局每个 writer 实例最多 100 次/秒，跳过项标为未知，不推断文件归属。

每路新增 `loss-observations.jsonl`，约每秒读取每 CPU 的暴露计数，另有最终样本；包含观测始末单调时间、相对上一样本的 per-CPU 增量、读取字节、读处理耗时、排空间隔、预算耗尽次数及 stats 耗时。summary 还包含排空线程 CPU 时间。计数读取并非跨 CPU 原子快照，增量只定位观测括号，不能给出精确丢事件时刻。任何正增量、计数未知或回退仍拒绝完整性。

该文件每实例最多 `min(8 MiB, trace-max-bytes/8)`，预算从原有追踪输出上限中预留，不额外放大总容量；达到上限会失败，不静默截断。收尾等待线程最多 3 秒，不能及时退出会明确报告清理失败，不声称证据完整。事件流仍保留原有字节、时长、内核 ring 和最终排空限制。

设备追踪会增加事件和观察成本。若需先检查采样自身开销，可在**独立新根**做有／无 `--storage-trace` 的短工具检查，保持其他参数相同，比较控制器 CPU、采样 RSS、事件量、最大采样间隔及 kernel ring loss。短检查明确标为 smoke，不替代上面的 600 秒验收；不要从失败截断窗口的平均 CPU 推断录制性能收益。

## 关联 fsync、日志与块事件

每个实际组保留：

- `trace/ready.json`、`trace/summary.json`、`trace/trace-events.jsonl`、`trace/loss-observations.jsonl`、`trace/SHA256SUMS`。
- `trace/storage/` 下同名五个文件；ready 包含实际空 PID filter、设备过滤表达式和事件格式哈希。
- 组目录下 `trace-combined.json`：两路状态和 loss 汇总，不改写两份原始 summary。外层 SHA 清单覆盖组合回执。
- 原有 resources、各 writer result、checkpoint-events、失败和清理侧车；运行时追踪失败另保留 `trace-failure.json` 及 `batch-failure.json`。

先从 result 和队列样本确定首次溢出附近的单调时间，先取溢出前 2–5 秒，再选一个两路都完整覆盖、最长 30 秒的窗口。单路窗口最多保留 20,000 条／16 MiB；高 I/O 事件率下应缩小窗口，而不是提高到无界。下列时间只是格式示例，必须换成本轮实际纳秒值：

```sh
python3 scripts/validation/analyze_storage_trace.py \
  --trace /srv/sdk-test/new-storage-attribution-01/round-00-status-off/trace \
  --start-ns 10000000000 --end-ns 13000000000 \
  --output /srv/sdk-test/storage-attribution-window-01.json
```

分析器核对两份 SHA 清单、零丢失和完整状态、共同 target 身份／mono 时钟及窗口范围。读取总量、窗口事件数／字节、关联状态、输出和分析时间均有上限；超出时失败，不截断后继续声称完整。不能对旧版仅 syscall 的目录补造设备事件。

文件角色只有在以下证据同时满足时才标为匹配：FD 观测始末均在配对 syscall 内，且同 TID 的嵌套 ext4_sync_file 事件具有相同 dev/inode。FD 读取发生于排空 trace 时，不一定等于 syscall entry；采样太晚、FD 已关闭／复用或 inode 不符时保留未知。syscall 与 ext4 的时长相互重叠，不相加。

jbd2 使用设备／transaction ID 还原提交区间，与 fsync 的关联仍是时间／设备重叠，不是唯一因果 ID。普通 block tracepoint 没有稳定 request ID，扇区可能重复、请求会合并、重排或部分完成，flush 的 sector 常为 0；工具保留这些事件，并按 syscall 区间列出各类块事件及 flush/FUA 标记事件的重叠计数。并行 syscall 的计数会重叠，不能相加。**不按相同 sector 元组拼造请求延迟，也不把重叠直接归因于 SSD**。

## 确定性长尾回归

Linux 回归在独立 LD_PRELOAD 子进程注入有界等待，每次延迟后仍调用真实 fsync；不作用于真实相机或生产进程：

```sh
cargo test --locked -p offline-depth --test chunked_recording \
  continuous_sync_tails_keep_credit_limits_real_barriers_hashes_and_crash_prefix
cargo test --locked -p offline-depth --test writer_contention_benchmark \
  sync_tail_replay_reports_real_fifo_overflow_without_relaxing_capture_or_recovery
```

第一项重放五轮 half 的 payload 331 ms／checkpoint-directory 327 ms 串行等待，以及五轮 full 的 payload 458 ms／index 20 ms 等待，检查原有额度、正常两轮读回与哈希，再单独 SIGKILL 并验证可恢复前缀。普通 reader 仍拒绝 partial，不产生伪 COMPLETE。

第二项使用真实 writer 和原有 30 FPS／200 Hz 模型、FIFO16 复现溢出，检查失败输出、原有额度、前缀双读回及逐帧／IMU 哈希。full 场景另加明确标注的 200 ms 预置停顿来建立已有积压；该停顿不是本轮设备测量值。这些测试重放等待及错误边界，不认证原盘 ext4／SSD 的物理根因。

## 回传与判读

回传新源码／ELF／工具 SHA、probe、设备与挂载证据、manifest、全部两路 trace 文件及 SHA、组合回执、资源采样、完整 checkpoint-events、writer result、首次溢出窗口分析和失败／清理记录。不要沿用只收集旧 trace 顶层文件的导出名单而漏掉 `trace/storage/`。载荷留现场时列清其位置及未回传范围，保留旧失败证据。

分别核对两路周期及最终 loss counters、排空指标、原始选定事件流、格式／设备／PID 身份，以及最终资源点是否晚于所有 producer 和两路 trace 结束。trace 完整只证明这次所选事件窗口完整，不替代录制成功、六组长测或独立恢复。若证据仍不足，应明确写未归因，而非用更大的队列、超时、丢帧或弱化同步得到通过。

## 本地回归与下一轮实机

工具回归命令：

```sh
python3 -m unittest discover -s tests -p 'test_writer*trace.py'
python3 -m unittest discover -s tests -p 'test_writer_long_run.py'
```

FIFO 测试覆盖双路突发输入、慢解析／输出、无控制轮询时持续排空、周期 loss、观测字节上限和最终哈希；Linux 控制器回归覆盖追踪失效后保留三份正常收尾结果，同时确认原生溢出或总时限仍停止自有进程。FIFO 背压不是内核 ring 模型，不能用这些测试认证真实机器无丢事件。

本地不执行原盘或三相机复测。由实机测试者使用新提交、新构建和新根执行上文 600 秒命令，回传两路 loss-observations、线程排空指标、最终 writer result 与全部既有证据。若再次失败，分别报告追踪状态、writer 收尾状态及未执行组；旧 `d15ce1c` 和更早失败根均保持不变。
