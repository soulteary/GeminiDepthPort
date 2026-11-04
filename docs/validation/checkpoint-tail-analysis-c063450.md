# c063450 检查点长尾关联分析

本说明记录的是代码调试和只读证据关联。原批次结论仍为接受 0/6、独立恢复未执行、清理成功；不修改官方汇总、不把派生分析作为录制或恢复验收。

## 可重复运行的工具

在包含本版工具的仓库中执行：

```bash
python3 experiments/sdk-2.10.6-diagnostics/analyze_checkpoint_tails.py \
  --run-dir /srv/sdk-test/unpacked/gdp-retest-evidence/retest/ab/on-01 \
  --output /srv/sdk-test/derived/checkpoint-tail-c063450-01
```

`--run-dir` 指向已解压的单组目录。默认关联首次观察到 raw failed 前 10 秒、后 2 秒，可用 `--before-seconds`、`--after-seconds` 和 `--failure-camera` 调整。故障锚点来自观察器 resource/event 行，不由 controller-status 行推断工作进程身份；首次观察时刻不是实际故障发生瞬间。

输出必须是不存在的新目录，且位于证据目录外；旧目录即使为空也拒绝复用。工具只读 run.json、两套 samples.jsonl 和对应 summary.json，在分析前、分析后和写出后复算输入 SHA256。它不运行 Docker、不访问相机、不读像素载荷、不改写原输入或原接受状态。

派生产物包括 `summary.json`、`checkpoints.json`、`resources.json` 和 `SHA256SUMS`。summary 保留两观察器的原完整性摘要、输入散列、观察器去重计数和已观察前缀的事件缺口；checkpoints 按目标、绑定后的 worker epoch、checkpoint_id 分组，保存每个阶段的 CPU/wall 时序与各观察器的前后资源样本引用；resources 保存被引用的队列、进程、磁盘及窗口上下文。SHA256SUMS 覆盖三个派生 JSON，不包含自身。

观察器 epoch 是本地计数，工具先按 target、host_pid/start_ticks 绑定实际工作进程，再合并 epoch。事件按 target、绑定后的 epoch、event_id 去重；相同事件的观察时间可以不同，实质内容冲突则拒绝分析。新版本的轻量 event-sample 与旧版把 checkpoint_events 放在 resource-sample 中的格式均支持。阶段 wall 与 thread CPU 保留原值；缺失 CPU 不当作零，短事件 CPU 略高于 wall 也不被篡改。

工具为 JSON/JSONL 文件、行长和记录数设定上限；解析错误、观察器 epoch 内身份变化、无绑定事件、重复事件内容冲突、磁盘计数倒退或无可观察故障均拒绝输出成功。重复执行同一输入到不同新目录，三个派生 JSON 及 SHA256SUMS 应逐字节一致。

## 本轮重算事实

以本轮只读复制的 on-01 为输入，10 秒前窗和 2 秒后窗选出 143 个 checkpoint 组。选择规则是至少一个阶段与时间窗重叠，再保留该组所有已观察阶段；这不保证每组阶段齐全。三个相机的组数为 A 52、B 52、C 39。

两个观察器合并后共 17,422 个唯一事件，跨观察器重复 13,733 个，同一观察器重复为 0。已观察前缀分别为 A event_id 1–5863、B 1–5940、C 1–5619，合并后这些前缀内均没有缺号。因此固定窗口记录确实提供了生命周期观察器遗漏的 42 个已观察事件；这不改写原 lifecycle.sampling_ok=false，也不证明最后一个已观察事件之后没有遗漏。原生命周期 A=10/B=32 和错误列表 12 条仍按原值保留，不能将重复错误记录相加为丢失量。

首次 raw failed 由生命周期资源采样记录，时间为 2026-11-03 22:28:45.827（UTC+8），C 原因为 RAW_QUEUE_OVERFLOW，written=3571、submitted=3587。另一观察器在此前约 205 ms 的样本仍记录 recording，构成观察边界，不是精确故障时刻。生命周期前一秒队列已有 11 对排队、1 对处理中，最老约 333 ms；首次失败快照为 15 对排队、1 对处理中、峰值 16，最老约 901 ms。之后队列清理记录丢弃 16 对。这支持 writer 暂时无法消化到达负载并形成积压。

C 的常规满块 checkpoint 每次增加 7 对，约 7.9 MB；在 30 FPS 下对应约 233 ms 的到达间隔。尾部 checkpoint 507、508、509、510 的 verify_wait 分别约为 252.286、245.796、470.492、247.943 ms。checkpoint 509 的 payload_sync 为 228.815 ms、checkpoint_directory_sync 为 226.264 ms；相应 verify_wait 的 thread CPU 仅 0.016401 ms，directory_sync 的 thread CPU 约 0.025438 ms。checkpoint 510 的 directory_sync 也约 224.539 ms。最后 checkpoint 511 仅推进到 3571 对，属于失败后已写前缀的尾块，不能假定所有 checkpoint 都有 7 对。

这些长 wall、低 thread CPU 的阶段把优先排查方向指向等待而非单纯校验计算。相同窗口内，A 的 verify_wait 最大约 472.103 ms，B 的 payload_sync 最大约 459.227 ms，值得继续检查三机并发检查点同步与共享 I/O 长尾。不能据此唯一确定 SSD、SDK、文件系统、同步锁或调度为根因。

`disk_brackets` 使用各观察器最邻近的前后样本包围检查点阶段，明确记录实际包围时长、样本 ID 和整盘累计计数增量。包围区间通常大于实际阶段，计数包含其他相机和进程；不能把某个窗口的 write_ms 或吞吐直接归为某个 checkpoint 的物理磁盘延迟。

## 验证与限制

Python 回归覆盖两观察器去重、worker 身份与本地 epoch 区分、事件内容冲突拒绝、新 event-sample、CPU 缺失、队列/磁盘关联、计数倒退、无故障、输入字节保持不变、新输出目录约束和派生结果可重复。真实证据已运行到新的 `/private/tmp` 派生目录，原附件和原结果未修改。

检查点阶段会重叠：verify_wait 可能覆盖后台 payload/index/checkpoint 同步，不能把它们的 wall 相加作为端到端停顿。资源采样只是离散快照，不能将旧 active span 直接延长为持续 I/O；CPU/wall 的差值也不等于设备等待时间。合并事件前缀无缺号不证明像素哈希、完整录制或恢复通过。本工具没有执行两轮实际读回、像素哈希、DDS 匹配或独立 SIGKILL 恢复，均不得补写为通过。


## 常驻线程与 FD 复用候选的评估

候选把每个检查点创建线程改为每个录制复用一个容量 1 的 worker，同时复用 index clone 和 directory FD。候选没有改变 32 对/16 MiB 未确认预算、同步顺序与次数、两轮实际读回、哈希或提交守卫。补丁仅归档在 [候选补丁](reports/checkpoint-worker-candidate.patch)，未应用到生产 Rust 路径。

使用同一 release 驱动、Rust 1.90、同一 ARM64 Linux 容器文件系统，做 5 轮交替顺序对比：burst/paced 各包含无注入与每次 payload fsync 增加 200 ms 两种情况，共 40 次。每次都检查预算、双读回字节数与完整读回；五份关键元数据/提交文件的 SHA256 在各自 profile 下前后完全一致。生成的测试录制在验证后显式删除，原实机证据不参与写入或清理。完整逐次数据与二进制/补丁摘要见 [性能结果](reports/checkpoint-worker-ab-20261104.json)。

无注入 burst 的 CPU 中位数从 0.48658 秒降至 0.43794 秒（约 -10.0%），吞吐从 284.25 提升至 329.67 MiB/s，但追加 P99 从 10.22 增至 15.59 ms。无注入 paced 的 CPU 从 0.19469 增至 0.24635 秒（约 +26.5%），追加 P99 从 7.49 增至 12.35 ms，验证耗时约 +14.8%。有 200 ms 注入的 paced CPU 也约 +24.3%。内存峰值中位数各组约增加 72–100 KiB。

该共享容器环境存在明显逐次波动；以上不是硬件因果或统计显著性结论。当前数据不足以证明候选在目标 30 FPS 负载有稳定收益，因此撤回生产优化，仅保留补丁和实验记录。更不能把突发吞吐提高解释为已解决 230–470 ms 同步长尾或实机 RAW_QUEUE_OVERFLOW。

复现实验：分别将 tests/checkpoint_sync_benchmark.rs 用同一编译器/profile 编译为基线和应用候选补丁后的 Linux 测试二进制，然后在空输出目录运行：

```bash
python3 scripts/validation/checkpoint_sync_ab.py \
  --before /srv/bench/before-benchmark --after /srv/bench/after-benchmark \
  --output /srv/bench/checkpoint-worker-new --repeats 5 --remove-generated-recordings
```

remove-generated-recordings 只删除本次新输出根中自行生成、已完成验证的 capture 树，保留结果、摘要和日志；省略该参数可保留实验录制。下一步应在新实机目录用修正后的事件采样收集连续检查点、设备/文件系统等待及线程栈，按单机/三机和检查点相位做受控对照，再决定是否改变同步实现。原六组与恢复验收门槛不变。


## 分频采样开销

另用三个合成状态目标、真实 Linux procfs、约 45.6 KB 状态文件、每 500 ms 原子发布，做 3 轮交替、每次 5 秒的采样开销检查。资源均采集 5 次，新事件读取 25 次；所有运行无读取/事件错误。旧采样线程 CPU 中位数约 0.00528 核，新版本约 0.00863 核，增加约 0.00335 个逻辑核；日志约从 179378 增到 219156 字节，约 +22.2%，而非随事件读取频率增加五倍。新版本只在文件版本改变时重新解析，缓存不掩盖原子发布的进程或会话变化。

这是短时合成状态/实际 procfs 的观察器线程开销，不是相机采集 CPU 或实机吞吐，且不包含进程其他线程、Docker 查询子进程开销。逐次记录见 [采样开销](reports/checkpoint-sampler-overhead-20261104.json)，实机仍应回传实际读取间隔、状态年龄和观察器 CPU。

## 本地验证记录（2026-11-04）

完整 Python 回归 441 项通过、4 项条件跳过；Linux 实际 procfs 探针回归 8 项通过，生命周期专项 8 项通过，只读关联工具专项 6 项通过。语法、差异和候选补丁可应用性检查通过。生产 Rust 两文件与原主线逐字节一致；候选仅作为未应用补丁保留。本次没有运行物理相机、没有重写原失败结果，也没有生成新的实机通过结论。
