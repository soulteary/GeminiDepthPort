# 8c279f9 后续：状态同步减量与 writer 对照

本次修复针对 8c279f9 三相机失败暴露的同步等待风险与观测缺口。原实机批次仍为接受 0/6、独立恢复未执行、清理成功；不改写原报告、包内清单或 MiniPC 上的原始录制。归档的常驻 checkpoint worker/FD 候选及此前实验结果不应用到生产。

## 依据与约束

官方证据包 SHA256 为 `1b854ad574f041fd9535a45c60c305feb716fe7341b5a545f85c5fdd4a611598`，补充包为 `6a5944e580b573fd1fc7be500b6bd7c80a2a40e0e0b36d89ad03fca5bc84aa5b`。内部 109/774 项清单已核验。B/C 长尾分析分别重跑得到 133/134 个分组，三个派生 JSON 与交付结果逐字节一致。

B/C 的 raw failed 关键确认期间执行了 28/25 次 IMU pump，终态 IMU 无错误。此次 280/245 ms 等待没有覆盖上轮约 710 ms 的实机等待；长 fence 正确性仍依赖已有 800 ms 以上故障回归及后续实机验收。

故障前有效等待证据包括 18:58:29.786 的 B checkpoint jbd2/fsync，以及 29.987 的 B/C native writer writev/journal metadata 等待。30.185 的 native writer fsync 已晚于两机 raw failed 提交，可能属于 abort/Drop 写 INTERRUPTED，不能当作起始原因。快照中的线程状态、栈和 wchan 不是原子读取，出现等待函数不等于测到了连续阻塞时间。

队列容量、心跳/退出期限、严格时间戳、增量检查点读回与结束时整段第二轮读回、逐帧哈希、提交守卫和 partial 恢复规则不变。raw 每检查点的 payload/index/pre-directory/checkpoint-file/post-directory 五个同步步骤不减少；前后目录同步跨越 checkpoint 发布，承担不同的恢复顺序保障。

## 状态与质量文件持久化

原来 query/current/events/status 四个文件分别执行文件 fsync、rename 和目录 fsync，共八次 fsync。新的后台 writer 对同一目录的连续质量文件完成逐文件 fsync/rename 后，共同同步质量目录一次，再发布并同步 status 及其目录，共六次。关键确认仍在整批同步成功后返回，质量文件可靠落盘先于 status 发布。

稳定 events 内容进一步去重：只有后台 writer 成功完成整个批次之后才建立 durable cache，键包含 events 路径、session、instance 和完整内容 SHA256，缓存容量为一项。未变化时还要核对实际文件与目录身份；文件被删除、替换或修改不能沿用缓存。新事件、新会话、身份变化或校验失败必须重写或报错。普通快照被合并、磁盘失败和关键批次尚未确认都不能推进缓存。稳定期每批为五次 fsync，status 心跳仍按原周期实际落盘。

这些计数描述同步调用次数，不能直接换算为 NVMe 请求数或承诺延迟下降百分比。需要同时比较 CPU、追加 P99、状态年龄、队列峰值、验证耗时和内存；原始 writer 的设备级根因仍需同盘受控实验。

## writer 观测边界

观察器补充每目标/线程读取的起止时间、线程前后状态和 host/namespace TID，分别保存状态、栈、wchan 的证据来源。等待证据只能解释为采样点之间的一致性，不能冒充完整调度或 I/O 时间线。

仅在验证 worker ELF ABI 且观察到 fsync/fdatasync 时采集 fd；输出限于本次绑定的 state/recordings 文件，前后复核 syscall、路径和 inode。未知 ABI、权限不足、fd 变化或进程/线程退出都明确记录，其他系统调用参数和无关 fd 路径不导出。前后值相同仍无法排除 ABA，不声称测到了同一次系统调用的完整起止。没有块请求级追踪时，能力字段必须明确不可用，整盘 diskstats 均值不是设备延迟分布。

严重采样间隔和 overrun 纳入缺口及成功判定；真实退出流程完成与诊断数据完整继续分开。精简状态保留 protected_sequence、pump_errors，使关键确认与 IMU 排空可在同一观察流关联。原来的身份拒绝、状态缺失继续观测、栈摘要引用、输出上限和只管理自有 perf 进程组等保障保留。

## 单 writer 与三 writer 对照

新增独立的同盘对照入口，不访问相机或 SDK。它使用生产原生 writer/reader，按 30 FPS 生成有限图像和 200 Hz IMU，保留有界队列、溢出报错和未确认预算。单/三 writer 使用相同数据负载、同步起点和新目录，交替运行；状态后台负载是否存在必须在 manifest 中声明。

结果保存每进程 CPU/RSS、append P99、队列峰值/overflow、验证时间和哈希，并区分同时采样的 RSS 合计与各进程 HWM。检查点前缀恢复、正常两轮实际读回及逐帧哈希均需通过，失败不能生成 COMPLETE 或写成通过。工具只清理自行创建的子进程/进程组；默认保留新实验文件，不接受旧失败目录作为可写目标。

本机容器文件系统上的合成实验只验证工具和机制，不代表 MiniPC 的 ext4/NVMe 性能，也不能单独验证状态优化收益。现场应先在同一磁盘、同样文件系统和背景负载的新目录运行单/三 writer 对照，确认同步竞争，再决定是否研究 checkpoint 首块相位错开。不得通过 sleep writer、延后检查点、扩大预算或关闭同步来制造通过。

## 实机验收

需从包含本次 Rust 修复的干净提交重建镜像，保存源码/SDK/runtime/镜像摘要；只更新宿主脚本不能验证状态同步优化。使用不存在的新输出根，保留所有旧失败目录和清单。正式三相机仍依次完成六组长测，任一失败即停止；六组全部接受之后才执行独立 SIGKILL 恢复。

采集覆盖录制及收尾，回传完整 lifecycle/writer 数据、同步计数、关键 submitted/completed/protected sequence、IMU drain/峰值/错误、raw 队列/P99、CPU/RSS、读回和恢复结果及 SHA256。未执行和权限不足如实保留。只有全部正式验收完成，才能宣称解决本机 RAW_QUEUE_OVERFLOW。

## 可复现命令与输出预算

先在目标 Linux 主机、最终提交上构建 release 公共 driver（前后对照必须使用同一份 driver，不能分别使用各版本自己的负载）：

```bash
cargo +1.90.0 test --locked --release -p offline-depth --test writer_contention_benchmark --no-run
python3 scripts/validation/writer_contention_ab.py \
  --after /absolute/path/to/writer_contention_benchmark-EXECUTABLE \
  --output /srv/sdk-test/NEW-storage-contention \
  --seconds 10 --repeats 3 --max-output-bytes 8589934592 --max-wall-seconds 900
```

可选 `--before /absolute/path/to/SAME-driver-baseline`，工具会交替单/三 writer 与 before/after；十秒、三轮的双版本对照需同时把 `--max-output-bytes` 提至 `17179869184`（16 GiB）并预留相应空间，默认 8 GiB 只覆盖此处的单版本计划。`--output` 必须不存在，且位于实际待测录制磁盘；先预留足够空间。总量计划包含全部轮次和恢复；单文件有限额，日志写前受限，总目录每秒检查，但这不是文件系统级硬配额。任一失败保留新目录并停止，不覆写旧失败证据。默认十秒只作定位对照；正式三相机六组长测与恢复仍按 RETEST 执行。

在主长测开始前并行启动观察器，等待 bindings，整个录制及收尾都不提前停止。示例的一小时上限包含等待窗口，需要按正式批次时长调整；`RUN_DIR` 是该组 capture 根（例如 `$NEW_BATCH_ROOT/ab/on-01/capture`），`WRITER_OUT` 必须在该根之外且不存在（例如 `$NEW_BATCH_ROOT/ab/on-01/writer-tails`）：

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/observe_writer_tails.py \
  --run-dir "$RUN_DIR" --output "$WRITER_OUT" \
  --disk "$RECORDING_DISK" \
  --seconds 3600 --interval .2 --tail-seconds 5 --max-output-mib 512
sudo python3 experiments/sdk-2.10.6-diagnostics/pack_results.py \
  --root "$NEW_BATCH_ROOT" --output "$NEW_EXPORT" \
  --max-file-mib 512 --max-total-mib 4096
```

`RECORDING_DISK` 必须事先核实为录制盘的整盘设备名（例如现场确认后的 `nvme0n1`），默认不传 `--disk` 不会采集磁盘计数。宿主观察及打包沿用正式复测的同一 root UID/GID。观察目录必须位于 `RUN_DIR` 之外；要与主批次一起打包，可放在已创建的批次根下、各组 capture 根的旁边。控制器通过 FRESH_ROOT 检查之前不要提前创建批次根；若观察器提前启动，先使用批次根外另一个新诊断目录，结束后单独打包。

批次根外的观察目录不能直接作为 `pack_results.py --root`，该工具要求完整 retest 元数据；可在观察完全结束后用 `tar -C /absolute/parent -czf /absolute/NEW-writer.tar.gz writer-directory`，再对新包生成 SHA256，并在解包后的 writer 目录核验原始清单。默认等待已验证 bindings，固定选中的 worker 全部终止后继续观察尾部 5 秒；不要在录制刚停止时手动结束。六个正式组分别使用六个新观察目录，主 lifecycle 观察仍覆盖整个清理和审计。

增强后的 12 线程样本一小时估算约 434 MB，512 MiB 不是任意线程数/时长保证；较长批次须事先计算全部六组的总预算。pack 的单文件默认 256 MiB 可能跳过大 trace，不能只调观察器预算。检查打包 warnings，无 oversized/skipped 证据后，解包分别核验外层清单与每个 writer-tails 内原始 `sha256sum -c SHA256SUMS`；仅包 SHA256 一致不能证明样本齐全。

## 本机验证结果（2026-11-05）

数据卷实验结果保存在 `reports/storage-follow-up-20261105/`；独立单/三 writer 对照的完整载荷保留在 Docker 数据卷的新实验目录，Git 中只归档其指标、资源样本、日志和完整运行清单。状态 A/B 的临时录制在独立读回后由测试夹具清理，归档保留完整状态指标和文件摘要，不声称仍保存这些载荷。`writer-contention/FULL-RUN-SHA256SUMS.txt` 是含载荷的原清单，不应对这个元数据子集执行全量核验；本次归档自己的 `SHA256SUMS` 另行覆盖全部已提交文件。原日志采用无损 `.log.gz` 归档，解压后仍为原始字节，保留终端输出的空白和失败信息。

状态 A/B 使用同一 release 配置、原始 8c279f9 baseline 和当前候选，C ABI SDK/ROS 替身、逐调用日志与实际 fsync 拦截计数均启用；每次 90 对 640×480 彩色/640×400 深度，名义 30 FPS、双路 200 Hz IMU。该计时包含替身和观察开销，不能等同于真实 SDK/ROS 速率。

两批各三轮交替 A/B 共 12 次完整运行，每次原生增量/结束读回、独立 reader 90 对哈希、新事件发布和稳定事件期间心跳前进均通过，IMU 无错误。首批 quality 目录 fsync 为 31/34/37→14/14/13；第二批为 31/31/31→13/13/13，包含启动和收尾，不能用总数直接充当逐心跳成本。逐批可靠性测试确认稳态批次由 8 次减到 6 次、events 稳定时 5 次。

首批 CPU 中位 0.438→0.612 核，追加 P99 上界的轮次中位 25.139→149.502 ms，RSS 峰中位 10136→11084 KiB；第二批 CPU 0.311→0.250 核，追加 P99 7.064→8.866 ms，RSS 10576→10192 KiB。只有 90 个追加样本且使用有界直方图，共享宿主的长尾很不稳定；这些结果不支持整体 CPU/P99 已改善。完整验证耗时与各次原值保存在 comparison-summary.json 和未删减 JSON 中。

更早一次 overlay 文件系统试验因 `IMU_STREAM_STALE` 失败（排空间隔约 527 ms、ready 关键确认约 1.187 s），保留在 `gdp-storage-release-experiments.log.gz`，不计为通过，不通过扩大队列或延后心跳掩盖。改到数据卷重跑改变了文件系统，两个环境不能混合计算收益。

独立存储对照为 6 组（单/三 writer 各三轮，after-only、每进程 30 对），全部双轮读回及额外 reader/哈希通过，无队列溢出，峰值为 1。每进程 CPU 中位约 0.310/0.327 核，追加 P99 中位 12.021/15.182 ms，同刻 group RSS 峰中位 4.61/13.84 MiB。30 个样本时 P99 等于最大值，只证明此负载下流程和工具通过，不是稳定尾延迟结论；没有 status 背景负载，不能用于归因状态优化。独立 SIGKILL 在已确认检查点后恢复 32 对，两次恢复 reader 校验通过、普通 reader 拒绝 partial、前后源树 SHA256 不变。

观察开销的短时 ABBA 结果另存 `writer-tail-observer-point-evidence-20261105.json`：12 线程 CPU 中位 0.1035→0.1182 核、RSS 21600→21790 KiB，每行约 10.56→24.14 KB。共享宿主并行编译与权限不足均在原结果中披露；新增严重缺口判定按预期拒绝了一轮，不能宣称无采样影响或数据全部完整。

完整 Python 回归在 macOS 为 489 项、11 项平台条件跳过，在挂载 Linux ELF 产物的隔离容器为 489 项、1 项条件跳过，均通过。最初误用系统 Python 和宿主二进制的运行保留原日志并作废，不计作成功。Linux 全 workspace/all-targets Clippy（Rust 1.90、`-D warnings`）与 rustfmt 通过。Rust 首轮完整执行暴露两处旧测试夹具竞态：逐属性实时启用故障会在同一帧中途改变 Global 数据；首样本后人为暂停会合法触发 500 ms stale。现按帧锁存故障、恢复连续 200 Hz 源，并核算发布/提交/排除及真实超限；精确错误码、队列上限、两轮恢复检查不变。修正后两条隔离回归通过，性能使用的生产和 benchmark 代码未改；归档补充了两处 test-only 差异及前后源码摘要。清空 profiles 后的完整重跑通过：生产 Rust 行覆盖率 19374/19561（99.0440%），高于 99% 门槛；工作区及 doctest 654 项通过，32 项具名性能/子进程入口按定义忽略，四组外部真实 Rust 契约共 63 项通过且没有跳过。覆盖率过滤器 18 项通过。覆盖率摘要、逐套结果、最终日志和 SHA256 均已归档；本地未启用 real Compose，完整远端 CI 另行验证。
