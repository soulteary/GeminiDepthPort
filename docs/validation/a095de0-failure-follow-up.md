# a095de0 失败后的状态等待与 IMU 修复

本次修复针对 a095de0 实机失败暴露的软件问题。原批次仍为接受 0/6、独立 SIGKILL 恢复未执行、清理成功。原报告、官方包、上下文包及 MiniPC 上的录制不作为本次测试输出目录，不补写通过结果。

## 证据与修复范围

官方证据包 SHA256 为 `0f44bdc6b24f650e074da483b353365c530198dab1c862ad570f1e57cf65e1a0`；上下文包为 `eb3d369babac1c40c93b24e8e7f31d88dca50224391589c4fd8515778289e5f5`。只读核验官方包 110 项、上下文包 758 项内部 SHA256 清单，独立重跑长尾关联得到相同的默认 C 锚点结果、119 个检查点分组。三个 lifecycle 已发布事件前缀 A 1–6961、B 1–6961、C 1–6972 连续，共 20894 个事件。

三机先发生 RAW_QUEUE_OVERFLOW；首个 raw failed 关键状态提交分别等待 A 324.453 ms、B 709.759 ms、C 474.746 ms。三个 IMU 首错推算时间都落在各自等待区间内，距上次 drain 约 317–330 ms。两路各 200 Hz、容量 128，对应约 320 ms 的积压量级。IMU 时间由启动时的 Unix 时间与单调 elapsed 相加，含墙钟映射误差，不能当作独立校准的 UTC 时间。

本次解除这条状态等待对 IMU 排空的阻塞，保留 critical 批次的可靠落盘确认。状态提交和等待分开，在等待期间由原采集线程继续执行 IMU 排空、时间戳/数据校验和 ROS 发布；SDK/ROS 句柄不迁移到新线程。持久化锁不覆盖 IMU pump。关键批次未确认前不能被普通快照替换，磁盘失败或 IMU 校验失败仍传播为错误。

raw 已 failed/draining 时，样本按照原有 outside-recording 计数排除，不写入已失败或封口的录制。raw 仍在 recording 时，继续保留原 pending IMU 容量约束，不能通过丢样本或扩大缓冲来假装录制完整。图像采集在持久化 fence 期间仍可能暂停；本修复不是无限长落盘停顿的吞吐保障。

c063450 的失败状态提前可靠发布、后停止 SDK/等待录制器的顺序保留。持久化确认之前不声称状态已经 durable，不通过刷新虚假心跳延长原 supervisor 的心跳和退出期限。录制队列上限、提交守卫、两轮实际读回、逐帧哈希及 partial 恢复规则不变。

## 控制器和采样

控制器将已绑定状态的归属校验与实时进程存活校验分开。已 pin 的 raw/IMU 失败保留完整原因，worker 退出或 /proc 文件消失作为附加诊断。首次绑定、健康采集、成功验收仍要求完整容器和 supervisor/worker 身份链，核对 PID/start_ticks、namespace、配置、session/instance 及容器启动身份。无 pin 或身份变化不能借失败字段绕过校验，也不能重建 pin 来接受新进程。

生命周期采样保存读取开始和结束的阶段。读取在 sample/wait-complete 中开始、跨入 cleanup 才报错时仍使采样失败；不会因为异常处理时已进入 cleanup 而免责。按阶段保留读取错误统计和汇总口径。启动前、清理后目标不存在可以单独记录，但不能把这些缺测补成零 CPU/RSS。

控制器结构化失败保存到 `result.json.capture_failure.cameras[]`，包含 classifications、raw_reason、imu_error 和 liveness 附加诊断。生命周期新增 `read_errors` 的起止阶段/transition/operation 统计、successful/failed_reads、`all_target_reads_succeeded` 及 `sampling_ok_scope`。线程 CPU 子项读取失败仍保留此前已从状态提取的检查点事件，活跃阶段的资源读取错误仍拒绝成功。

sampling_ok 仅描述观察器采样检查，既不证明像素完整，也不证明 600 秒、六组长测或恢复通过。事件无缺号还需核对最终 latest_event_id；尝试读取次数不等于成功次数。

## 原始 writer 长尾尚需实机定位

A/C 检查点 verify_wait 约 665 ms，thread CPU 仅约 0.012/0.019 ms，同期有数百毫秒 payload/checkpoint 同步阶段。重叠阶段不能相加。整盘 diskstats 计数包含其他活动，无法单独区分 SSD、文件系统、锁或调度等待。

本次增加独立只读 writer 观测入口，采集实际 worker 线程的 stack/wchan/schedstat、磁盘计数与 dirty/writeback，核对同一运行身份，并要求新输出目录。缺失权限、线程退出和采样超时均显式保存。该工具不打开相机、不修改生产状态、不执行恢复，也不将观测成功等同于硬件验收。

后续单机/三机对照应在独立诊断批次保持镜像、SDK、每机配置、磁盘和其他负载一致，再比较检查点等待与队列积压。仅选择观察一个目标不等于只启动一台相机。标准三相机六组及独立恢复仍单独执行，诊断批次不能代替验收。

归档的 `checkpoint-worker-candidate.patch` 和此前 A/B 结果继续只归档，本次不应用该候选。

## 实机 writer 观测命令

先按原入口启动新的复测批次。等控制器已创建该组 `capture/run.json` 后，在另一终端运行：

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/observe_writer_tails.py \
  --run-dir /srv/sdk-test/imu-fence-retest-01/ab/on-01/capture \
  --output /srv/sdk-test/imu-fence-retest-01/ab/on-01/writer-tails \
  --disk nvme0n1 --seconds 3600 --interval 0.2 \
  --wait-for-bindings 300 --tail-seconds 5
```

按现场替换根目录和磁盘名；后续组使用各自新的 writer-tails 目录。不要在控制器检查 FRESH_ROOT 前提前创建测试根。如果需要提前启动观察器，其输出应放在测试根外另一个新诊断目录，回传时单独打包。

默认不启用 perf。默认读取主线程、native-depth/recording-checkpoint/status-persistence 线程的有界 kernel stack、wchan、schedstat，以及队列、关键状态等待、IMU 和 SDK 的紧凑状态、整盘 diskstats、meminfo Dirty/Writeback 等字段。已相同的线程栈用 SHA256 引用前一条原文；不会重复保存完整直方图和 64 条检查点历史。

默认总预算 256 MiB，最多 64 个线程、单栈 8192 字节、100000 个样本。选中 worker 全部退出后继续 5 秒磁盘尾部；到达时长/容量边界、stop-file 或信号均明确记录。`ok`、`workflow_completed`、`diagnostic_data_complete` 分开，权限不足、线程选择截断、磁盘计数倒退及缺失数据不可伪装为完整观测。默认 3600 秒覆盖正常单组录制与收尾，实际长于该期限时应在开始前显式增加，最大 86400 秒。

`--camera a` 只选择观察目标。`--all-threads` 可扩大到有界的全部线程；可选 `--perf-seconds 30 --perf-frequency 49 --perf-max-mib 64` 只使用已安装的 perf，启动前后核对目标身份，限制输出并仅管理自有 perf 进程组，不安装工具或更改内核设置。此类额外 profile 应用于独立诊断批次并标记开销，不能混入正式 SDK on/off 性能结论。

输出包括 `samples.jsonl`、`summary.json` 和 `SHA256SUMS`。位于组目录内的 JSON/JSONL 会随 `pack_results.py` 收集；perf.data 仍被排除，若显式启用应单独保留与回传。先等待控制器和观察器均结束再打包。

worker 仍存活而状态 JSON 缺失、损坏或字段不全时，记录 `status-unavailable` 并继续读取已 pin 线程，等待实际进程退出；proc 批次缺测单列为 `proc-batch-unavailable`。这些缺口分别使 `status_evidence_complete`/`process_batches_complete` 和最终 `ok` 为 false，不能借 worker 之后正常退出抹去。真实身份变化仍拒绝跟随，原 pin 不变。

## 合成故障验证与观察器开销

Linux SDK/ROS C ABI 故障回归在真实 fsync 屏障期间验证发布推进和 durable 确认。正常两路各 200 Hz、原容量 128：draining 关键等待 910.524 ms，期间发布推进 299、pump 88 次、最大 drain 间隔 35.059 ms、队列峰 14；raw overflow 后关键等待 848.755 ms，期间发布推进 297、pump 81 次、最大 drain 间隔 34.451 ms、峰 14。屏障释放前不提前确认，正常结束核对两轮实际读取字节数和 IMU 数量；永久阻塞仍按原期限失败并恢复两遍前缀。逐次数据见 [故障屏障结果](reports/imu-critical-fence-20261104.json)。这些是合成后端的正确性验证，不是实机吞吐或 CPU 优化结论。

坏时间戳、真实 burst 超限、持久化错误/panic、失败状态早于被阻塞的最终 reader 落盘等拒绝路径仍覆盖。另保留 Recording 态原 pending IMU 上限的拒绝测试，防止只排空 SDK 队列后在原生缓冲中悄悄丢弃数据。

只读观察器另做 4.213 秒真实 Linux procfs 短跑：3 个自有 fixture worker、每个 4 线程，22 行、211433 字节，observer CPU 109.424 ms（约 0.026 核）、RSS 20.82 MiB、最大间隔 201.362 ms，无 iteration overrun。Docker 身份 API 为测试注入，实际执行严格 procfs 身份链；没有真实相机。kernel stack 权限被拒绝 241 次，因此明确 `diagnostic_data_complete=false`，wchan/schedstat/meminfo 使用真实读取。完整结果见 [观察器开销](reports/writer-tail-observer-20261104.json)。

使用实际约 50 KB/相机状态形状压缩到约 1.87 KB，每机 4 个紧凑线程、三机 5 Hz 的一小时日志投影约 184.18 MiB，低于默认 255.75 MiB 样本预算。它不是长期实测保证；栈变化、更多线程或启用 perf 可增加开销，达到硬上限会拒绝完整观测，不静默截断。

## 新实机复测要求

必须从包含本修复的干净提交重建 runtime 镜像，保存源码 revision、镜像 ID 和 runtime/SDK 摘要。只更新宿主 Python 工具、继续使用 c063450/a095de0 旧镜像，不能验证 Rust 修复。

为本次所有运行选择不存在的新输出根；不得复用 `/srv/sdk-test/a095de0-full-lifecycle-01` 或 `/srv/sdk-test/c063450-full-lifecycle-01`。使用同一不可变新镜像完成 on-01/off-01/on-02/off-02/on-03/off-03，任一失败停止后续组；仅六组全部成功后执行独立 SIGKILL 恢复。两轮实际读回、原生 counts/归一化 ROS 深度哈希、IMU/Global/DDS 匹配及恢复验收沿用原门槛。

除原证据包外，回传新 observer 输出及 SHA256、raw FIFO/IMU 队列峰值、关键状态 pending/completed sequence、critical wall、IMU drain 最大间隔、心跳状态年龄和退出原因。观测包含录制及收尾；未知或未执行项目保留为未知/未执行，不补写通过。

## 本地验证记录（2026-11-04）

最终 Linux 完整 Python 回归 464 项通过、1 项条件跳过；macOS 首轮完整回归 459 项通过、4 项条件跳过，后续新增项已在最终 Linux 全套覆盖。SDK 终态/身份与阶段错误专项、writer 观察器 13 项回归、证据打包中的 writer JSON/JSONL 保留及 perf 排除均通过。

提交后的终审另补观察器缺状态/坏 JSON 与真实进程退出的两项边界回归，Linux 观察器专项共 15 项通过。上述 464 项为 bcbceb7 软件修复全套结果；新增异常分支单独验证，覆盖率及开销报告保留当次测量的源码 SHA256，不改写历史实验结果。

完整 Linux Rust workspace、Rust 二进制支持的外部验收测试及 combined 覆盖率脚本通过；生产行覆盖为 19239/19428（99.0272%），没有修改覆盖率过滤或 99% 门槛。最终额外的 Recording 800 ms fence 容量回归通过，达到 pending IMU 128 时保留 RAW_IMU_QUEUE_OVERFLOW 首因、无 COMPLETE，实际恢复两遍检查点前缀；未扩大队列来避免真实错误。capture-probe all-targets Clippy、Rust 1.85 格式检查、Python 编译及差异检查通过。

运行命令：

```bash
python3 -m unittest discover -s tests
RUST_TEST_THREADS=2 bash scripts/testing/rust_coverage.sh combined
cargo +1.90.0 test --locked -p capture-probe --test service_supervision \
  recording_critical_fence_retains -- --nocapture
cargo +1.90.0 clippy --locked -p capture-probe --all-targets -- -D warnings
cargo +1.85.0 fmt -p capture-probe --check
```

Linux 测试在隔离容器中使用 Linux 二进制和独立临时目录；未访问相机。当前两包、当前报告及上轮 c063450 两包共五个输入，在修复前后逐字节 SHA256 和大小核对一致。归档候选补丁及旧实验结果未变，SDK/ROS/录制格式实现未变。本次软件验证不改变 0/6 的旧实机结论；原始 writer 长尾的设备根因和新镜像的完整实机验收仍需现场复测。
