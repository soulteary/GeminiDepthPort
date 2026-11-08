# 录制同步长尾：文件系统与块层归因

`bf7690f` 原盘 half/full 长对照都在首个 status-off 组溢出。half 有串行约 331/327 ms 的同步，full 有连续约 473–490 ms 的额度等待。原追踪只有 syscall／调度事件，不能区分 ext4、jbd2、块层或设备刷新。本工具增加可选的设备追踪，不修改录制实现、队列、心跳、同步屏障、两轮读回、哈希或恢复规则。

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

原有三 writer、30 FPS、200 Hz IMU 模型、FIFO16、32 对／16 MiB 全局额度及心跳保持不变。任一组失败仍停止该实验并保留失败根，后续组／恢复记为未执行，不续跑旧根。

默认新增每组最多 4 GiB 设备事件及 1 MiB 组合元数据预算，纳入总容量预检。上述六组计划从 403,904,790,528 增至 **429,680,885,760 字节**；加 2 GiB 空间保留后至少需 **431,828,369,408 字节普通可用空间**。实际按程序 plan 复核，不靠删除旧数据满足预算。

两个实例各限制 kernel ring 总量 32 MiB，合计不超过 64 MiB；`--trace-buffer-kib` 仍为每 CPU 的数值。CPU 多或 buffer 太大时会拒绝，不能静默扩大内存。输出、单行、线程／事务状态、时长、最终排空和 loss counters 都有界。任何丢失、未知格式或排空错误使设备归因不完整。

设备追踪会增加事件和观察成本。若需先检查采样自身开销，可在**独立新根**做有／无 `--storage-trace` 的短工具检查，保持其他参数相同，比较控制器 CPU、采样 RSS、事件量、最大采样间隔及 kernel ring loss。短检查明确标为 smoke，不替代上面的 600 秒验收；不要从失败截断窗口的平均 CPU 推断录制性能收益。

## 关联 fsync、日志与块事件

每个实际组保留：

- `trace/ready.json`、`trace/summary.json`、`trace/trace-events.jsonl`、`trace/SHA256SUMS`。
- `trace/storage/` 下同名四个文件；ready 包含实际空 PID filter、设备过滤表达式和事件格式哈希。
- 组目录下 `trace-combined.json`：两路状态和 loss 汇总，不改写两份原始 summary。外层 SHA 清单覆盖组合回执。
- 原有 resources、各 writer result、checkpoint-events、失败和清理侧车。

先从 result 和队列样本确定首次溢出附近的单调时间，先取溢出前 2–5 秒，再选一个两路都完整覆盖、最长 30 秒的窗口。单路窗口最多保留 20,000 条／16 MiB；高 I/O 事件率下应缩小窗口，而不是提高到无界。下列时间只是格式示例，必须换成本轮实际纳秒值：

```sh
python3 scripts/validation/analyze_storage_trace.py \
  --trace /srv/sdk-test/new-storage-attribution-01/round-00-status-off/trace \
  --start-ns 10000000000 --end-ns 13000000000 \
  --output /srv/sdk-test/storage-attribution-window-01.json
```

分析器核对两份 SHA 清单、零丢失和完整状态、共同 target 身份／mono 时钟及窗口范围。读取总量、窗口事件数／字节、关联状态、输出和分析时间均有上限；超出时失败，不截断后继续声称完整。不能对旧版仅 syscall 的目录补造设备事件。

文件角色只有在以下证据同时满足时才标为匹配：FD 观测始末均在配对 syscall 内，且同 TID 的嵌套 ext4_sync_file 事件具有相同 dev/inode。FD 读取发生于排空 trace 时，不一定等于 syscall entry；采样太晚、FD 已关闭／复用或 inode 不符时保留未知。syscall 与 ext4 的时长相互重叠，不相加。

jbd2 使用设备／transaction ID 还原提交区间，与 fsync 的关联仍是时间／设备重叠，不是唯一因果 ID。普通 block tracepoint 没有稳定 request ID，扇区可能重复、请求会合并、重排或部分完成，flush 的 sector 常为 0；工具保留这些事件，**不按相同 sector 元组拼造请求延迟，也不把重叠直接归因于 SSD**。

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

分别核对两路 loss counters、原始选定事件流、格式／设备／PID 身份，以及最终资源点是否晚于所有 producer 和两路 trace 结束。trace 完整只证明这次所选事件窗口完整，不替代录制成功、六组长测或独立恢复。若证据仍不足，应明确写未归因，而非用更大的队列、超时、丢帧或弱化同步得到通过。
