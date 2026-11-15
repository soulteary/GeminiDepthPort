# R02 调度关联与观察器开销复核

## 本次修正的范围

R02 使用 9a95dc3 工具和 61642b8 驱动，首组发生三路 RAW_QUEUE_OVERFLOW，正式验收仍为失败。后续组、完整双读回和独立 SIGKILL 恢复未执行。本次没有调整生产队列、心跳期限、同步屏障或 checkpoint 策略，也没有应用 sync_data、首批错峰或 6 MiB 实验。

开发者精简包的外层 SHA256 为 `2e2f9ecb3acc6739e146d0b251e41fef6093c8685bf837f9e37a21bb7c1765e5`；已核对内层 1,643 项清单，无不匹配。完整 host/storage raw trace 在实机测试者处，本机仅复核交付的失败上下文，不能代替全量重算。旧包、原始日志和 SHA256 不作修改。

失败上下文中，一个跨 CPU wake 时间比对应 switch-out 早 2 μs。旧分析器在失败窗口之前遇到该记录即退出，因此没有分析后续等待链。现在保留原始时间和输入顺序，输出 `association-gaps.jsonl`，不钳位、排序或修补时钟：

- 无法证明的 wake 配对不输出 wakeup-to-run 延迟；独立、顺序有效的 off-CPU 区间仍可保留。
- syscall 名称或时间不匹配保留为缺口，后续独立事件继续解析。只有完整配对且返回整数 0 的同步调用才标记为严格同步。
- 窗口外缺口单独报告，不再阻止所选窗口分析；窗口内缺口仍使该窗口关联不完整。
- `analysis_scan_complete` 只表示扫描完成；`association_complete` 表示所选窗口，`association_pair_history_complete` 表示全扫描配对历史，三者不可互换。原有身份历史检查仍独立生效。
- 缺口全部流式导出，内存示例每类范围最多保留 1,024 条，并报告截断数量。输出达到显式容量上限时停止，不把截断分析判为完整。

R02 的 set_event_pid 绑定通过，不代表全生命周期身份完整。报告中的 21 项代际缺口、132 个未知出生锚点仍需保留，不能由局部窗口成功覆盖。新增事件计数区分已证明 writer 上下文、观察器数字 PID 命中和未绑定上下文；数字命中不是进程代际或子线程身份的证明，也不能据此计算因果开销。

## 在实机证据副本旁重新分析

不需要重新启动相机。使用本次提交的干净源码，设置原始只读证据目录和不存在的新输出目录；以下路径需与实际交付目录对应：

```sh
BASE=/srv/sdk-test/9a95dc3-observer-retest-02
NEW=/srv/sdk-test/r02-reanalysis-01
# NEW 必须不存在；不要覆盖原来的 derived-* 或 trace。
test ! -e "$NEW" && mkdir -m 700 "$NEW"
for camera in a b c; do
  python3 experiments/sdk-2.10.6-diagnostics/analyze_checkpoint_tails.py \
    --run-dir "$BASE/formal-observer-on-01/ab/on-01" \
    --failure-camera "$camera" --before-seconds 1 --after-seconds 0.1 \
    --output "$NEW/app-$camera" || break
  python3 experiments/sdk-2.10.6-diagnostics/host_trace_analysis.py \
    --source "$PWD" --trace "$BASE/observer-on-01/trace" \
    --official-analysis "$NEW/app-$camera" --output "$NEW/kernel-$camera" \
    --max-output-bytes 1073741824 || break
done
```

分别分析三路，避免仅以 C 的窗口判断 A/B。分析器校验输入清单并再次核对输入哈希；回传每路 summary、缺口流、等待链输出、事件计数和新 SHA256SUMS。若缺口与窗口重叠，结论必须保持不完整。原始追踪不完整、身份不成立或容量不足时，保留错误，不能按成功窗口推断设备根因。

## 有界故障复现

新增 Linux `r02_writer_tail_replay` 使用三个同时启动的 producer，在不同 writer 的 payload、index、checkpoint 和目录同步阶段注入连续长尾；B 额外覆盖 index flush 和 rename。注入次数有上限，延迟后仍执行真实 I/O。

这是依据交付上下文构造的确定性压力模型，不是硬盘故障复现或 SSD 根因证明。测试核对 16 对队列上限、实际溢出、未发布 COMPLETE、已确认前缀，以及恢复前后文件哈希不变。前缀执行两轮实际读取，校验图像哈希、深度内容和 IMU 字段。原基准驱动文件保持不变，先前隔离工具的哈希契约不受影响。

```sh
python3.13 -m unittest discover -s tests
cargo test --locked --release -p offline-depth --test r02_writer_tail_replay -- --nocapture
```

本机 Python 620 项通过（30 项按平台等条件跳过）；Linux 上该 Rust 入口 4 项通过、1 项仅供子进程调用而忽略。独立子进程压力也通过。以上不构成三相机正式验收。

## 后续观察器 on/off 对照

`run_host_observation.py` 新增只读工作负载收据：核对官方 run_retest 命令、同盘身份、三相机配置文件哈希及计划字段，仅归一化新输出目录和 plan 路径。前后收据变化或不支持的命令不能用于等价对照。旧 R02 结果没有此收据，不可事后补造。

继续使用现有 wrapper 的显式 `--observer on|off`、容量和时间上限。在独立的新目录运行配对的相同负载，保留正式失败停止规则；失败后不得自动启动后续组来凑齐对照。获得成功且等价的两次结果后运行：

```sh
python3 experiments/sdk-2.10.6-diagnostics/compare_host_observation.py \
  --on-result /srv/sdk-test/new-on/result.json \
  --off-result /srv/sdk-test/new-off/result.json \
  --output /srv/sdk-test/new-observer-comparison
```

比较器只报告 wrapper 自身及观察器线程的 CPU 核数差，排除 driver、worker 和内核线程，不声称总观察器开销或统计显著性。任一 driver 失败、on 追踪不完整、收据不匹配时只报告不可比较原因，不输出开销差。还需结合相同窗口的 writer CPU、队列历史、同步等待和追踪字节量判断影响；R02 单次观察器约 0.957 核的数据不足以确认溢出因果关系。
