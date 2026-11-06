# 原生录制：首次队列溢出的精确观测

本次针对 `11123c3` 两次 MiniPC 复测仍出现 `RAW_QUEUE_OVERFLOW` 的定位缺口。两轮正式接受仍为 0/6，正式独立相机 SIGKILL 恢复未执行。已有报告、包内清单与失败数据不改写；本次回归不改变这些实机结论。

`raw-failed` 关键状态确认入口发生在检测失败之后。此前用该入口划分的“入口前”样本仍可能属于失败收尾，不能据此断言同步操作导致了队列溢出。新观测直接记录软件 FIFO 首次拒绝新帧对的边界。

## 字段与边界

服务状态增加 `raw_recording.overflow_observation`。没有发生 raw 队列溢出时为 `null`；首次溢出后冻结，后续排空、丢弃、失败收尾及状态持久化均不覆盖它。它是运行时诊断，不写入录制索引或参与内容哈希。

- `version: 1`、`reason: RAW_QUEUE_OVERFLOW`、`clock: CLOCK_MONOTONIC`。
- `detected_monotonic_ns`：队列互斥锁内判满时读取的单调时钟。读取失败明确保留 `null`，不填零或推算时间。
- `rejected_pair`：被拒绝的 `pair_sequence`，以及各自独立的 `color_sequence`、`depth_sequence`。后二者是源序号，不能假定相等；源证据没有序号时保留 `null`。
- `queue_at_detection`：同一队列锁内冻结的容量、排队数/字节、inflight 数/字节及帧对身份、队首/队尾身份、最老队龄、累计出队/丢弃、高水位及关闭标志。队龄以该次判定内的同一个 `Instant` 计算。
- `writer_context_capture_started_monotonic_ns` / `writer_context_capture_finished_monotonic_ns`：队列和采集状态锁释放之后，采集并构造 writer 上下文的外层时间括号。
- `writer_context`：独立 writer 观测锁内复制的有界上下文，带自己的 `capture_started_monotonic_ns` / `capture_finished_monotonic_ns`。最多一个 `active_append`、四个 `active_checkpoint_spans`，超限计数明确保留；不复制历史、直方图、像素或 IMU 数组。

**队列判满快照和 writer 上下文不是跨组件原子快照。** Full 后立即按原有规则设置失败、停止继续入队，再采集上下文；采集时 writer 可能已经推进或开始收尾。以两个时间括号判断关联强度，不能将后采的上下文自动当作判满瞬间的唯一原因。冻结快照存在也不等于其关键状态已经可靠落盘；对外发布仍经原有整批 fsync 确认。

`active_append` 保存 append ID、正在追加的帧对序号、开始单调时间和进程/线程身份。checkpoint span 保存 checkpoint ID、持久前缀目标、阶段、开始单调时间和身份。PID/TID 属于 worker 所在命名空间，关联宿主采样时沿用已有 pinned worker、start_ticks 和 namespace 映射。

细分上下文沿用现有 `checkpoint_observation: true` 开关；无需增加配置项。关闭时仍记录精确队列拒绝信息，但 `writer_context.observation_enabled=false`、活跃细分上下文为空，不能解释为 writer 当时没有工作。没有活跃 span、时钟不可用和观测关闭应分别判断。

## 等待原因

`verify_wait.wait_reason` 描述 append 线程为何回收单个 pending checkpoint，不表示底层设备为何等待：

- `finished_reap`：发现后台任务已完成并回收结果。
- `global_payload_credit` / `global_pair_credit` / `global_payload_and_pair_credit`：继续写入会超过原有全局未确认预算。
- `chunk_rotation`：切换 payload 块前等待前一 checkpoint。
- `explicit_checkpoint`：显式 checkpoint/结束流程等待 pending 任务。

非等待阶段不添加该字段。常规 `disk_stages.checkpoint_observation` 增加 `active_append`，保持原阶段、事件序号和累计计数含义。append、阶段的活跃标记使用 RAII 清理，错误或 panic 不会遗留假活跃标记。

`verify_wait` 包含整个 pending checkpoint 的 join；payload/index/目录同步、读回、checkpoint 文件同步和发布均可能落在其中。它与工作线程阶段重叠，不可相加作为总延迟，也不能仅凭这个标签认定哈希 CPU 或 SSD 为根因。

## 保持的可靠性约束

raw FIFO 容量仍为配置的 1–16，IMU 上限仍为 128；32 对/16 MiB 未确认预算、心跳和退出期限不变。当前图像大小受字节预算约束，每半块通常只有七对，不能按 32 对推算实际缓冲余量。

payload 文件、索引、发布前目录、checkpoint 文件及发布后目录的五道同步屏障不减少。正常录制仍保留 checkpoint 增量首轮实际读回、结束时整段第二轮实际读回、逐帧哈希及严格 device/global 时间戳校验。失败录制无 COMPLETE，确认前缀恢复规则不变。归档 checkpoint worker 候选不应用到生产。

队列锁内仅读取固定数量的标量/身份，不分配 JSON、不复制帧、不获取 writer 或 IMU buffer pool 的锁。writer 上下文序列化在观测锁外执行；冻结对象在采集状态快照间共享，避免在 capture-state 锁内深拷贝，锁外生成 JSON 时仍会遍历并构造输出。

## 采样与复测

现有 checkpoint/lifecycle 采样和 writer 精简状态均保留完整 `overflow_observation`；事件去重不丢弃这个一次性字段。即使实时队列已经归零或当前 append 结束，仍能取到冻结证据。旧版本状态没有此字段时，采样继续兼容，缺失显示为 `null`。

使用包含本修正的干净提交重建 runtime 镜像与宿主工具，使用不存在的新输出目录。实际配置启用 `checkpoint_observation`，沿用现有三相机、IMU、Global/shared ROS context、原生录制的六组长测和失败停止规则。旧失败目录不续跑、不覆盖。

失败时回传以下原始信息：最终状态及完整 overflow 对象、关键状态提交/完成序号与事件、checkpoint/lifecycle/writer 样本及清单、镜像/源码/SDK 身份、配置 SHA256、partial/checkpoint 保全清单。先比较 `detected_monotonic_ns` 与同一 worker 时钟域的 checkpoint span，再看上下文采集括号；宿主时钟域不一致时不得直接比较数值。

本次只补齐定位能力，不宣称实机溢出已消除。正式验收仍要求六组全部接受、之后独立相机 SIGKILL 恢复通过及清理成功；失败批次的元数据前缀扫描或合成恢复不能代替正式双轮像素读回、哈希和 DDS 校验。

## 自动验证

回归覆盖判满时的独立源序号、满队列/队龄/inflight 身份、首次证据冻结、后续排空与收尾、锁外上下文采集、细分观测关闭、活跃标记正常/异常清理和等待原因。服务故障注入分别阻塞实际 checkpoint 读回、payload fsync，再在失败状态 fsync 栅栏期间核对真实 Rust 输出；保留连续 IMU pump、原队列上限和关键状态可靠确认。

2026-11-06 本地验证：Linux 完整工作区及 doctest 663 项通过、32 项具名性能/子进程入口忽略；真实 Rust 外部契约 63 项通过且无跳过，生产行覆盖率 19590/19782（99.0294%）。该全套构建之后新增了故障夹具的异常清理守卫；包含最终守卫的 Rust 1.85 故障专项三项全部通过（12.66 秒），并重新通过全部数据面的 all-targets Clippy（`-D warnings`）及 rustfmt。Linux 最终 Python 回归 501 项通过、8 项条件跳过，其中未在该 target 缓存构建的 Rust CLI 路径由上述外部契约单独验证；OpenCV 仍由 driver 镜像 CI 验证。

最终故障专项中，readback / payload fsync 两场景的失败关键确认分别约 1.199 / 1.809 秒，IMU pump 67 / 92 次、队列峰 20 / 24，未增加 128 上限或原期限。这是受控故障下的观测与可靠性验证，不是实机性能收益。冻结对象从未确认的状态临时文件、可靠发布、排空到最后收尾均一致；两个独立恢复 reader 校验确认前缀及哈希，checkpoint 原始字节不变，无 COMPLETE。

两轮原始补充包的 2376 个已解包文件在本次修改前后大小与 SHA256 相同；使用当前分析器重算两轮各三份派生 JSON，与旧交付结果逐字节相同。WeChat 临时附件的原路径在本轮开始时已不可用，包的原始 SHA256 沿用此前核验记录；本次没有重建、覆盖或声称重新校验缺失的外层附件。

命令示例（Linux、具备仓库锁定依赖的环境）：

```bash
RUST_TEST_THREADS=2 bash scripts/testing/rust_coverage.sh combined
cargo +1.85.0 test --locked -p capture-probe --test service_supervision raw_overflow_ -- --nocapture --test-threads=2
cargo +1.85.0 fmt --all -- --check
cargo +1.85.0 clippy --locked -p capture-contract -p quality-core -p capture-observer -p d2c-core -p depth-output -p offline-depth -p frame-core -p orbbec-sys -p orbbec-safe -p capture-probe -p ros-output --all-targets -- -D warnings
python -m unittest discover -s tests -v
python -m unittest discover -s scripts/testing -p 'test_*.py' -v
```

在容器运行监督测试时保留 `--init`，以正确回收其测试孤儿进程。验证摘要、原始日志及清单位于 [reports/raw-overflow-observation-20261106](reports/raw-overflow-observation-20261106/validation.json)。远端 CI 另以最终提交的全部 check runs 为准，不将本地通过写成远端结果。
