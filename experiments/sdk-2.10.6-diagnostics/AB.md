# 至少三轮 SDK on/off 自动交替对照

`run_ab.py` 是宿主批量入口，复用 [单组一键流程](ONE-CLICK.md) 的 UID、健康/录制等待、两轮实际读回、哈希、DDS 验收及回滚。每一轮包括一组 on 和一组 off，默认三轮共六组，串行执行；某组失败即停止后续组，不重试、不删除数据，不将失败组悄悄排除后继续宣布对照成功。

## 一条命令

先按单组说明在本机 rootful Docker/Linux 上准备三台已验证的配置计划、USB 授权，并通过生产 GDP 正常流程释放相机。建议使用 root 拥有的独立源码副本，避免 `sudo --build` 时 Git 所有权不匹配。

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/run_ab.py run \
  --plan /srv/sdk-test/three-camera.json \
  --image local/gdp-sdk-diagnostics:ab \
  --build --platform linux/amd64 \
  --root /srv/sdk-test/ab-01 \
  --rounds 3 --seconds 600 \
  --max-bytes 137438953472 --ros-domain-id 78 \
  --disk nvme0n1 --execute
```

ARM64 指定 `--platform linux/arm64`。已有镜像时去掉 `--build`。`--rounds` 最小 3、最大 10；默认顺序为 on-01/off-01/on-02/off-02/on-03/off-03，`--first off` 可反向交替。`--cooldown-seconds` 默认 30、范围 0–600，每组验收与停止后才冷却、进入下一组。`--disk` 替换为真实录制盘，可重复或省略。

默认每组采样 600 秒、最长健康等待 300 秒，30 FPS 时自动固定每台 31,680 对。六组仅采集部分约需 106 分钟，此外还有构建、读回审计和冷却时间；原始数据会全部保留，必须事先核对多组数据所需磁盘空间。128 GiB 是**每台每组**的上限示例，不是六组总量或空间预留。脚本不扩容、不删除历史录制，字节限额耗尽会失败。

健康、完成、审计等待与单组相同，可传 `--healthy-timeout`、`--completion-timeout`、`--audit-timeout`，构建限时用 `--build-timeout`。默认不采 perf；额外 `--perf` 会给所有组使用相同 profile 参数，但应将这批结果与无 perf 的开销对照分开。

## 固定什么

开始时复制三份 capture/health 原始字节到私有 `frozen/`，不再读取之后修改的原文件。构建至多一次，解析一次不可变镜像 ID 和 runtime 摘要，并在 `manifest.json` 中保存配置摘要、参数、顺序与 UID/GID。各组都使用同一 ID、冻结计划和一次计算出的帧对/等待限额；每组前以及结束时重新核对冻结摘要。外部修改冻结文件会终止批次。

组后还检查实际容器均已停止。汇总核对每组实际镜像/runtime/UID、源配置摘要、模式，以及生成配置去除唯一 generation 后的摘要。除模式、独立运行身份和输出目录外，配置必须相同。数据完整性和验收继续通过原有单组工具执行，没有减少 reader 次数、哈希、检查点或恢复约束。

## 自动汇总

```bash
sudo python3 -m json.tool /srv/sdk-test/ab-01/result.json
sudo python3 -m json.tool /srv/sdk-test/ab-01/summary.json
sudo cat /srv/sdk-test/ab-01/summary.md
```

只有总退出码 0、`result.json.ok=true`、`summary.json.complete=true` 且 `batch_ok=true` 才是完整批次。中间结果和失败批次保留已验收组，但 `comparison=null`，不输出有效的最终 on/off 比较。

`summary.json` 包含逐组、逐相机原值、三轮描述性中位数及每轮 on-minus-off/相对 off 百分比；off 为零时百分比为 null。统计口径如下：

- **CPU**：按相邻采样时间差积分 worker 的 CPU 核数，再除以实际观察时间；包含 SDK 所在线程，排除 supervisor、健康检查及观察器。另保留观察器 CPU 开销。
- **内存**：每台采样 RSS 峰值、三台同时采样 RSS 总峰值、各 worker 的生命周期 HWM；HWM 相加不是同时发生的峰值。cgroup 峰值单列，包含文件缓存，不可用时为 null。
- **追加 P99**：保留每台整个原生录制生命周期的固定直方图上下界。对照项是“最差相机 P99 上界”的逐轮中位数，不是所有相机/所有帧的合并 P99，也不平均百分位。
- **验证耗时**：整个原生 writer 的 verification 累计 wall ns、checkpoint/final readback 细分，以及外部三台顺序审计命令的 wall ns。外部审计包含 Docker/加载开销；多 worker 验证累计值不等于并行总历时，嵌套阶段不相加。
- **SDK 配对/队列/锁**：每个 PID/instance 的首末有效快照差，保留实际观察时长、计数与每秒速率；配对排除/聚合阈值/输出丢弃分别记录，候选超窗计数不是唯一丢帧数。队列峰值为实例生命周期峰值，不能作差。锁等待与持有分别报告 count、total ns 和 log2 P99 界限；非事务快照的 histogram/count 不一致时明确标注且不给出虚假的 P99。

SDK 关闭组标为不可用，不用零值参加 SDK 指标对比；开启组三轮另有 `sdk_on_rounds` 逐相机计数中位数和逐轮队列/独立锁站点统计。SDK 快照约 5 秒刷新，其首末观察区间不精确等同 600 秒。CPU/RSS 属于采样窗口，P99/验证属于整个录制，不能混称相同时间窗口或用它们推断逐帧因果。

`source_gap_free` 与源缺口逐组保留在原始报告中；通过录制与 DDS 匹配不等于没有源缺帧。统计只有描述意义，三轮不能自动证明显著收益；没有清空页缓存、调整系统参数或控制温度/其他负载，现场仍应记录硬件、固件、USB 拓扑与系统负载。

## 失败与回滚

每组完整目录位于 `on-01/`、`off-01/` 等；父控制器日志/进程身份位于 `controller/`，冻结数据和 manifest 保留。失败会中止后续组，清理本批次自建进程和未完成实验容器；已验收目录不删除、不续写。

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/run_ab.py rollback \
  --root /srv/sdk-test/ab-01
```

回滚先写入中止标记，复核父控制器保存的进程身份，防止正在运行的批次继续启动下一组；再调用单组身份检查回滚。已成功清理且实际停止的组不对其历史 PID 再发信号，避免进程号复用导致误杀。损坏配置/镜像自检不阻断身份检查回滚；UID 或容器身份不匹配仍拒绝发信号并保存错误。

批次不会自动恢复生产或执行 SIGKILL 实机故障注入。确认全部实验容器停止后，按生产 GDP 正常流程恢复；正常采集读回通过不能替代另一次 SIGKILL/断电恢复验收。本轮新增自动化测试使用合成数据与受控后端，没有三相机实机性能结论。

自动验证结果与实机边界见 [AB-REPORT.md](AB-REPORT.md)。
