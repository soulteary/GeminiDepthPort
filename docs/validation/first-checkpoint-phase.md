# 首次 checkpoint 相位隔离实验

使用固定 `61642b8` 的独立源码副本，比较首批 **7／7／7 对**与 **3／5／7 对**。两组 producer 使用相同 CLOCK_MONOTONIC 启动屏障、相同 30 FPS 输入和 200 Hz accel／gyro。只改变首批封存的帧对数，不延迟 producer，不在采集或 append 路径添加睡眠。

对照组也是显式首批目标 7 对：在追加第 7 对后封存；它不是未经修改的生产二进制。生产按原有字节目标可能到第 8 对追加前才封存第一个 7 对块。此实验比较两组显式首批策略，不能将其结果直接当作生产前后收益。

首次封存以后，两组均恢复原有 half-credit 分派目标（16 对／8 MiB），总未确认额度仍为 32 对／16 MiB，队列仍为 16 对。目标不是硬性块大小：pending 未结束时仍按原逻辑使用剩余额度。payload／index／directory 同步、checkpoint 发布、两轮实际读回、逐帧哈希和前缀恢复均未替换。生产代码没有开关或改动。

## Linux 执行

需要 Git、Rust/Cargo、Python 3 和 C 编译器。在工具所在仓库根目录执行，全部输出目录必须不存在。保留旧数据、日志、partial 和 SHA256；容量不足时停止，不清理旧证据。

```sh
python3 scripts/validation/first_checkpoint_phase.py \
  --revision 61642b8 --output /srv/sdk-test/first-phase-build-01
python3 scripts/validation/first_checkpoint_phase_run.py \
  --build /srv/sdk-test/first-phase-build-01 \
  --output /srv/sdk-test/first-phase-faults-01 --faults
python3 scripts/validation/first_checkpoint_phase_run.py \
  --build /srv/sdk-test/first-phase-build-01 \
  --output /srv/sdk-test/first-phase-600-01 \
  --seconds 600 --repeats 3 \
  --max-output-bytes 824633720832 --max-wall-seconds 24000
```

最后一步交替运行两组策略、三轮重复，共六组三 writer。600 秒是每组每 writer 的采集时长，不是整套总时长。容量预检保守复用含单 writer 余量的预算；768 GiB 是配置上限，不是保证实际可用空间。支持先用新目录、`--seconds 2` 和 4 GiB 上限验证工具，但不计为正式长测。

相位对照只在 `writer-0/1/2` 中选择 3／5／7；恢复使用单独固定 first3／first5／first7 启动器。启动器依赖构建目录中的 phase.elf，不能单独搬移。构建清单记录源码提交、补丁、编译器、ELF 和启动器 SHA256。

## 故障与完整性

故障工具复用真实 Rust writer，对首批 3、5、7 三种策略的第二次索引 fsync 分别注入 100 ms 延迟及 EIO，共六项。注入标记必须存在。延迟后执行真实同步；EIO 必须返回失败，不能发布 COMPLETE。已确认前缀使用恢复读取器执行两轮实际读回和哈希／IMU 检查，恢复前后所有原始文件 SHA256 相同。额外核对实际首批数及观测无丢失，结果见 phase-fault-verdict.json。

正常组逐项校验有效文件哈希、相同输入哈希和 snapshot 哈希，并通过两轮逐帧内容验证。首批不同会改变 chunk 边界、索引、checkpoint 等文件，不能要求两个布局的文件 SHA256 相等。正常组后，对 first3／first5／first7 分别执行独立 SIGKILL 已确认前缀恢复；未执行不得计为通过。

## 相位与性能

两个策略使用同一实验二进制和同一有界内存记录器，记录封存开始 dispatch、worker payload_start、index_start／end 和 commit_end。dispatch 是封存入口时间，不是线程真正开始时间。容量 131072 行，超出会计数并拒绝判为完整相位结果。时间戳为同主机 CLOCK_MONOTONIC，逐组核对共同启动屏障、首批数量和正常 checkpoint 完整提交。

记录器会带来内存、分配和短锁开销，两组一致不等于零开销。事件在收尾后导出；遭 SIGKILL 时不承诺保留该进程的相位历史，独立恢复验收只针对原始录制前缀。当前工具不包含 capture-service 状态持久化背景、SDK／USB／ROS 或心跳验收，也不证明物理掉电安全。

回传各组原始事件（包含后续全部 dispatch 时间）、phase.json 中的相对屏障分派时间和 index_sync 并发重叠时长，检查错峰是否持续、是否重新汇合。应用 span 不是块设备耗时，不可据此判定 SSD／jbd2 根因。

同时回传 CPU、追加 P99／最大值、验证耗时、RSS、队列峰值、credit 峰值、溢出与实际完成量，以及 manifest、summary、每组 JSON、子日志、资源采样、故障结果及恢复目录、SHA256SUMS。交付包另附 SHA256。只有启动错开、均值改善或失败推迟，均不能视为关闭持续录制溢出。

独立软件恢复回归可用 `--recovery-only` 和新的输出目录单独运行。它不继续失败的性能批次，也不把失败批次或未执行组计为通过；真实验收的停止规则仍应遵守。该入口输出 recovery-summary.json，文件哈希见各策略恢复结果中的 source_unchanged_sha256。

## 本地软件验证（2026-11-14）

六项延迟／EIO 故障注入及恢复均通过，独立 first3／first5／first7 SIGKILL 恢复通过。容器 overlay 上 1 秒／writer 的六组烟雾流程通过，首批数量、共同屏障及相位完整性检查通过。该长度不能代表持续录制性能。

另两次更重的本地运行失败，均保留：Docker 共享目录 2 秒对照在首个同相位组三 writer 溢出；overlay 上 5 秒对照的同相位组完成，但随后错峰组 B 溢出，其他组停止，性能批次内的恢复未执行。其原始 summary 区分尝试组和通过组。共享目录失败同时暴露旧汇总器对 null 读回结果报错，已修复并增加回归；原失败目录不补写 summary。

5 秒错峰组三路首批封存约为屏障后 79.94／144.35／212.40 ms，证实首次分派错开，但不足以消除该环境积压。失败进程提前结束，不能直接比较整组 CPU／吞吐或宣称性能改善。摘要、原始组结果、相位事件、实际补丁和恢复结果见[本轮归档](reports/first-checkpoint-phase-20261114/overlay-five-second/summary.json)，各文件 SHA256 一并保留。原 MiniPC 的至少 600 秒复测仍待测试者执行；不改变生产默认。
