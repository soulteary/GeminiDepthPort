# checkpoint 重叠修正验证

基线 `83c06d878fd74cdb41e65224a607d4bbe7eabccb`，2026-11-07。修改与不变量见[说明](../../checkpoint-overlap.md)。**正式持续录制 RAW_QUEUE_OVERFLOW 尚未关闭，未执行新的 MiniPC 相机验收。** 旧官方包、上下文包再次核对 SHA256 不变，旧 checkpoint-worker 候选未应用。

已复现并修正两处软件反压：未耗尽全局额度却在半额度封块目标等待；后台同步／读回 index 时，前台向同一 inode 刷入后续 IMU 索引。第二处改为有界元数据暂存，最多 2 MiB，每 writer 比原 64 KiB 索引缓冲最多增加约 2 MiB 保留内存，不增加原生帧队列。活动尾部不提前确认，恢复仍只读持久化前缀。

同一故障回归在原实现失败、修正后通过。只加封块修正、保留原索引处理的中间实现仍把 gated checkpoint 的索引从 472,447 字节写到 650,605 字节，触发独立断言。pair、byte、较大 IMU 元数据三类共 57 个 checkpoint 故障场景覆盖慢同步、读回、写入、rename、错误、损坏和 SIGKILL；真实队列超限测试仍必须失败，原始原因不被覆盖。新增 regular regression 还核对：真实读回屏障已进入时，后续追加不会进入有延迟的 index write，不依赖脆弱的时间阈值。

## 受控索引争用

相同 Rust 1.90 release / thin LTO 构建选项、相同 benchmark 源码，交替三轮。checkpoint 的真实 readback 暂停；只在测量区间对实际 index write 注入每次 500 ms 延迟，实际 write、所有 fsync、读回和哈希仍执行。每帧 80 个合法单调 IMU 样本，属于 metadata stress batch，不是实时 200 Hz 吞吐模拟。

每轮 15 次追加的耗时中位数：1,507.45 → 2.63 ms；每轮最大追加耗时的中位数：501.47 → 0.368 ms。原版三轮均进入延迟 index write，新版三轮均未进入；47 对图像及 IMU 全部通过 writer 两轮实际读回，另加两个独立 reader。收益仅适用于被该受控争用阻塞的区间，不能换算为普通采集的倍数提升。

## 三 writer 正常与长尾对照

最终 comparison-04：两版从相同环境重新构建，common driver、fixture、状态适配器源码 SHA 一致。三 writer 每路 300 对／10 秒、固定 30 FPS，状态持久化 on/off 各三轮交替，12/12 正常组全部完成，36/36 writer 的额外两轮 reader、逐帧哈希和 IMU 数量均通过。它是共享 VM / virtiofs / APFS 的短合成测试，没有 SDK/ROS，不是 600 秒正式长测。CPU 包括 writer、状态后台和额外两轮 reader；RSS 也是该进程完整生命周期峰值。

以下为各状态／版本九个 writer 测量的中位数，P99 是单 writer 的 P99 再取中位数，不能当作合并分布 P99：

- 状态 off，前 → 后：CPU 1.996 → 2.094 秒；追加 P99 16.35 → 15.28 ms；峰值 RSS 8.14 → 8.11 MiB；writer 校验累计 976.7 → 1,028.6 ms。
- 状态 on，前 → 后：CPU 3.034 → 3.103 秒；追加 P99 27.94 → 37.18 ms；峰值 RSS 10.54 → 9.63 MiB；writer 校验累计 1,231.5 → 1,288.5 ms。

**没有证实通用性能收益。状态 on 的 P99 本轮观测变差，不能略去或称为优化成功。** 共享环境存在明显调度／I/O 波动，不能据此单独归因；保留每 writer 数值和范围。正常 200 Hz/stream 场景索引缓冲峰值约 24 KiB，尚未达到旧 64 KiB 刷写边界，本次 index 修正主要保护较大批次。

每次 payload fsync 连续额外增加 300 ms 或 900 ms 时，两版均 RAW_QUEUE_OVERFLOW。300 ms 下新版各写 43 对，原版各写 50、50、57 对；900 ms 两版均各写 15 对，FIFO 峰值均为 16。均属失败组，不能以截短 CPU/RSS 比较收益，也不能据正常短测宣布持续溢出已修复。长停顿或平均确认速率不足仍超出有限缓冲能力，需要现场完整阶段数据及 600 秒三 writer／三相机验收。

## 归档及验证边界

本地全 workspace clippy（所有 targets，warnings 为错误）、完整 workspace + 外部回归通过；生产 Rust 组合行覆盖率 19,652 / 19,842 = 99.0424%。新增受控 index regression 另行运行并通过；最终远端 CI 以本提交 Actions 为准。

metadata.tar.gz 保留最终与探索性实验的原始 JSON、事件／采样、日志、构建身份、expected-failure 证据；source-snapshots.tar.gz 保留相关源码和复现脚本。早期尝试的权限／错误 harness（运行 0 tests）不计作成功；comparison-03 的较早二进制不用于最终对照。完整载荷、records.jsonl、ELF、共享库及源码 checkout 不包含在 metadata 交付中，排除项见 EXCLUSIONS.json；完整本轮数据仍在 `/private/tmp/gdp-credit-overlap-20261107`。没有修改早期失败输出。

先核对外层 SHA256SUMS，解包后核对 ARCHIVE_SHA256SUMS。归档是元数据子集，不声称含有全部像素或足以脱离原数据重新执行完整录制验证。
