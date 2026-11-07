# 三 writer 同盘长时对照与 syscall／调度追踪

本工具接续 `80585ec` 的首次溢出观测，补齐十秒 writer-only 测试没有覆盖的持续写入与状态后台竞争。它生成三路 30 FPS 原生 YUYV 640×480、Depth16 640×400，以及 accel/gyro 各 200 Hz 的确定性输入，运行生产原生 writer/reader。它不访问相机、SDK、USB 或 ROS，不能替代六组实机及独立相机恢复验收。

## 负载与约束

同一 release 二进制分别运行 status-off、status-on，三 writer 共用未来的 `CLOCK_MONOTONIC` 起点。默认三轮、交替顺序，共六组，每组每 writer 完整 600 秒、18,000 对；最后一帧之后仍等到完整单调时钟窗口结束才进入 draining，不能用 599.967 秒代替 600 秒。

on 模式直接复用生产 `ServiceQuality::persistence_batch`、`Persistence`、关键状态确认、单 pending 快照与 durable events 缓存。生产持久化线程与 raw writer 位于同一进程，status/quality 文件和原生录制位于同一文件系统；500 ms 状态后台持续到 writer 结束读回及独立 reader 完成。启动、draining、结束均执行真实关键确认。quality receipt 来自模型化输入计数，不是实机采集证据。

off 模式只关闭实验适配器中的生产状态写入，不修改生产配置。两种模式都有相同的有界 500 ms 观测线程和 checkpoint 观测，以分开“采样本身”和“实际状态写入”的开销。采样线程与独立 reader 的开销均明确纳入统计口径。

FIFO 是合成输入使用的非阻塞有界模型：16 个排队帧对加一个 inflight，满时失败而非反压相机。它不替代生产 `RawRecorder` 的 SDK 采集覆盖。未确认预算仍为 32 对／16 MiB；真实图像受字节预算约束，每半块通常七对。原有五道同步屏障、checkpoint 增量实际读回、结束整段第二轮实际读回、逐帧哈希、严格 device/global 校验及 partial 恢复均不改变。

后续工具增加 `--checkpoint-batching full-credit|half-credit`，默认实验完整额度模式，真实图像通常每块约 14 对；生产仍保留半额度默认，候选需显式启用。两种模式使用同一驱动分别建新目录，详见[部署验收与候选边界](recording-readiness.md)。另持续保存每 writer 最多 16 MiB 的 `checkpoint-events.jsonl`，缺口或尾部丢失使诊断失败；不要将最后 64 条内存历史当作完整时间线。

正常完成后另运行两个独立 reader，比较每帧、IMU 与元数据哈希。off/on 必须使用相同输入，五个关键原生文件的 SHA256 也须一致。之后分别在两个状态模式下进行独立 SIGKILL：确认非空 checkpoint，on 另需状态关键确认，再仅终止本次自有子进程；普通 reader 拒绝 partial、两轮恢复 reader 返回确认前缀、输入树哈希不变、无 COMPLETE。该实验验证进程崩溃，不模拟物理掉电。

该存储隔离适配器没有完整复刻 `RawRecorder` 的持锁提交守卫及 SDK/ROS supervisor。已完成原生提交之后发生的状态错误，可能保留经过读回校验的原生 COMPLETE；整体实验仍必须失败。不能用原生目录存在 COMPLETE 推断状态后台或整组通过，生产提交守卫保持原样。

## 有界连续追踪

追踪使用本次独占的 tracefs instance，不改变全局 trace 设置、不自动挂载、不安装工具。三 writer 就绪后、共同起点放行前绑定进程出生时间、已有 TID 及同一时间命名空间；通过 `set_event_pid` 和 `event-fork` 跟随后续短生命周期 checkpoint 线程。

默认记录 fsync、fdatasync、writev 的进入／退出和调度、唤醒、线程派生／退出事件。只导出归一化标量和本次自有线程名称，不导出 writev 指针、缓冲内容或无关线程名称。这是选定 syscall 的连续事件流，不是全部文件 I/O；特别是 JSON 的普通 `write()` 不在默认集合。独立追踪入口可用 `--include-read-syscalls` 加入 read/readv/pread64/lseek，需重新预算事件量。

按实际打印精度保留时间戳；微秒文本不会被声称具有纳秒精度。syscall wall、off-CPU 与 wakeup-to-run 区间可关联但不能相加。fd 可复用，TID/阶段相关也不等于唯一文件或 SSD 根因；没有块请求追踪时不能给出设备级因果链。

默认每 CPU ring buffer 256 KiB，并限制总内核缓冲内存；不能把每 CPU 大小当作总量。每组事件文件默认上限 4 GiB，可显式设置为 1 MiB–8 GiB，另保留有界元数据空间；解析状态、线程数、单行、读取批量和结束排空均有上限。真实内核短测约 3.8 秒已产生 3.9 MB 事件，600 秒及大规模读回必须额外预算，原先 256 MiB 不足以覆盖这一观测速率。增大磁盘输出预算不会增加内核 ring buffer 或放宽丢失判定。按前后 per-CPU loss counters 及流内 LOST 记录检查丢失；丢失、解析失败、身份变化、超限或清理失败均不属于完整追踪。

能力不足默认在放行负载前拒绝。`--allow-unavailable-trace` 仅允许明确的初始能力缺失以便做独立正确性验证；运行中丢失或错误仍失败，缺失追踪时 `long_comparison_complete=false`。不能将进程退出码 0 或 `summary.ok=true` 单独用作完整长时诊断验收。

tracefs 行为及文本合同参考 [Linux ftrace 文档](https://docs.kernel.org/trace/ftrace.html)、[事件过滤文档](https://docs.kernel.org/trace/events.html) 和 [内核 syscall 文本打印实现](https://github.com/torvalds/linux/blob/master/kernel/trace/trace_syscalls.c)。

## 实机运行

在包含本次变更的干净 Linux checkout 构建，保存提交号、二进制及源码 SHA256。实验驱动位于 `capture-probe` 的测试适配器，不加入生产可执行文件。宿主只需 Python 标准库和构建好的 Linux release ELF；采集内核 tracefs 通常需要 root 及内核对应配置。

先检查追踪能力，不能把权限不足解释为“没有等待”：

```bash
sudo python3 scripts/validation/writer_syscall_trace.py --probe
```

已有 release 驱动时可直接使用启动脚本，不需要让 root 重新构建：

```bash
sudo bash scripts/validation/run_writer_long_test.sh \
  --binary /absolute/path/to/capture_probe-RELEASE-HASH \
  --output /srv/sdk-test/NEW-writer-status-600 \
  --seconds 600 --repeats 3 --disk nvme0n1 \
  --max-output-bytes 549755813888 --max-wall-seconds 14400 \
  --trace-max-bytes 4294967296 --trace-buffer-kib 256
```

没有预构建驱动时，启动脚本会运行 `cargo +1.90.0 test --locked --release -p capture-probe --lib --no-run --message-format=json`，从编译结果选取唯一的 libtest ELF，然后启动同一控制器。以已具备工具链和 tracefs 权限的账户执行；不要为了构建在已保留的数据目录里创建项目。

`--output` 必须不存在，且位于实际录制盘；`--disk` 必须事先核实为整盘设备名。启动前检查三 writer 数据、状态文件及 trace 的整体预算和可用空间。默认六组含最大 trace 预算预计约 404 GB，并非只预留一组约 61 GB；512 GiB 是总输出上限，不是磁盘配额。目录每秒检查一次，意外增长仍可能跨过阈值，实际超量会保留并导致失败。不要在运行中迁移旧数据、drop caches、调文件系统参数或增大队列来制造通过。

组内观测覆盖三 writer 共同起点到进程退出，包含收尾与独立 reader。初始状态目录/bootstrap 与进程启动早于 trace 挂接，不在连续内核追踪窗口内，另保留 ready 和首个关键确认信息。正常采样周期与完整输出大小都有上限；缩短为 `--seconds 3 --repeats 1 --smoke` 仅供快速验证，不能充当 600 秒验收。

## 输出与判断

保留 `manifest.json`、每组 JSON／resources.jsonl、每 writer 的 result.json／status-load-samples.jsonl／原生目录、on 模式 state/quality 文件、trace/ready.json／summary.json／trace-events.jsonl、两个恢复目录及全部 SHA256SUMS。

CPU 同时提供固定采集窗口和包含收尾、独立 reader、状态结束确认的全流程值；三 writer 与父采样／追踪进程分开，RSS 的同时采样合计与各进程 HWM 分开。追加 P99 是每个 writer 自己的样本百分位，不合并三机百分位；另记录最大追加、FIFO 等待、验证时间、producer lateness、队列峰和溢出。各阶段重叠时间不求和。

完整一轮对照至少要求：两组各三 writer 都达到 600 秒，无队列溢出、写入及状态错误；实际心跳状态未跨过原 10 秒期限，最后持久化序号全部确认；两轮原生实际读回及额外双 reader、哈希一致；两种状态模式的 SIGKILL 恢复通过；选定 syscall／调度追踪无丢失、越界或身份错误。三轮都完成才具有重复对照，仍不证明统计显著性或实际相机验收。

现场回传完整元数据和清单，并按需保留原始载荷。新测试目录与旧失败证据分开；先核验最外层 SHA256SUMS，再核验各 trace 子清单。打包元数据子集时必须明确排除项，不能用其哈希声称完整载荷已经交付。

## 本地验证记录

2026-11-06 基于 `80585ec` 完成本轮工具验证，[日志、源码身份与元数据归档](reports/writer-long-run-20261106/README.md)单独保留。Rust workspace、外部录制回归、Rust 1.85 数据平面 lint 与 Python 538 项测试均通过（Python 8 项按环境跳过）；新增控制器／追踪测试在 Linux 为 37/37。生产 Rust 行覆盖率为 19,591 / 19,782，即 99.0345%。

两次实际 600 秒计划均未通过，不能据此给出后台开关的性能收益或长时验收结论：

- APFS 宿主绑定目录中的 off 组三 writer 在约 289.9 秒出现 `RAW_QUEUE_OVERFLOW`，on 及正式恢复未执行。该环境无可用 tracefs，显式记录缺失追踪，未计作完整对照。
- 新建、独立的 Linux ext4 loopback 文件系统中，off 组在约 13.7 秒出现同类溢出，on 及正式恢复未执行。该文件系统仍经过 VM 与宿主 APFS，不能视为物理 SSD 隔离实验。失败组取得 36,513 条内核事件、零报告丢失，2,049 个配对 syscall；writer-2 的 checkpoint 55、`payload_sync` 对应约 581 ms 的 fsync，阶段线程 CPU 约 4 ms。调度记录支持等待关联，不能唯一归因 SSD。

另一次 ext4 三秒 off 短测写完三路各 90 对，取得 15,219 条事件、零报告丢失。随后 on 组心跳过期并留下两项子进程清理超时；环境卸载成功不等于该组通过。独立的新目录中，off/on 各 32 对的真实 SIGKILL、双恢复 reader、哈希与普通 reader 拒绝 partial 检查通过；这是恢复合同回归，不是两次失败长测中未执行的正式恢复。

本轮实测还修正了适配器报告自锁、关闭通道遮蔽原始错误，以及已确认退出的 writer 被其他 writer 收尾时间误判心跳过期的问题。最终汇总要求所有计划组都有完整追踪；保留的早期短测汇总中，`trace_complete=true` 只覆盖已返回的 off 组，不能证明失败 on 组完整，原始输出不改写，补充回归防止复发。

完整的 600 秒三 writer 开关对照、三轮重复结果及正式独立恢复仍待在目标录制盘的新目录执行。没有通过增大队列、放宽心跳或修改生产同步策略制造通过；旧失败证据的 2,376 个已提取文件重新核对大小及 SHA256，一致。
