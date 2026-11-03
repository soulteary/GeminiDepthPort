# 一键三相机流水线复测与独立恢复验收

`run_retest.py` 提供包含 c063450 错误收尾观测的三相机检查点流水线复测入口。它不会修改生产配置或 SDK；只在私有配置副本中显式设置 `chunked-v3` 和 `checkpoint_pipeline=true`。现有队列容量保留，省略时明确设为16，其他相机/IMU/Global/QoS/quality 配置保留。生产与原入口默认值仍不变。

## 准备与运行

在原生 Linux、rootful Docker、Compose v2 上运行，所有阶段使用同一 root UID/GID。先通过生产 GDP 正常流程停止采集并释放相机。使用 root 拥有的独立 Git 源码副本，以免 sudo 构建触发 Git 所有权错误。源码必须包含 `c063450` 及本次全程采样工具提交；不需要预先构建本地基础镜像。

将 `three-camera.plan.example.json` 复制到自己的位置，填写三台已验证的 capture/health 配置绝对路径。要求 IMU 启用、Global 映射、device 配对、完整帧对录制策略及匹配的 quality 配置、独立 serial/namespace/frame ID。无需手工打开 pipeline；不要在原文件上修改。示例：

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/run_retest.py run \
  --plan /srv/sdk-test/three-camera.json \
  --image local/gdp-sdk-diagnostics:pipeline-retest \
  --build --platform linux/amd64 \
  --root /srv/sdk-test/c063450-full-lifecycle-01 \
  --rounds 3 --first on --seconds 600 \
  --max-bytes 137438953472 --ros-domain-id 78 \
  --disk nvme0n1 --camera b --execute
```

ARM64 改为 `linux/arm64`；磁盘名称按现场替换。镜像构建至多一次，然后对 A/B 和恢复实验始终传递同一不可变镜像 ID。省略 `--build` 可复用镜像，但 runtime 的 GDP 提交必须是 `c063450` 或本地 Git 中可验证的后继提交，否则拒绝开始，避免误用旧的仅异步读回镜像。

输出根目录必须不存在，不覆盖、不续跑、不重试、不删除失败数据。构建/校验失败尚未打开相机。新源配置写入 `frozen/`，原始 capture 摘要、修改后文件摘要、镜像/runtime 摘要及 GDP revision 保存到 `manifest.json`。所有阶段前复核冻结文件；原始文件之后的修改不会进入本次实验。

## 全程采样

正常组自动通过 --observe-lifecycle 开启独立全程采样，从相机启动前持续到录制和停止完成；失败时持续到清理完成。独立 SIGKILL 阶段也覆盖启动、注入、审计和清理。数据存入各组 lifecycle/ 和根 crash-lifecycle/，每秒采集进程 CPU/RSS/cgroup、磁盘累计计数、检查点事件及 c063450 的持久化、IMU 排空和错误收尾字段。

原有 600 秒 checkpoint-samples/、SDK/DDS 窗口不变，全程数据单独分析。每份日志上限 128 MiB、100000 个资源样本，错误摘要最多 256 条；正常运行阶段读取失败、超限均拒绝验收，不覆盖旧证据。完整命令与验收见 [c063450 实机长测](../../docs/validation/c063450-hardware-long-run.md)。

## 执行顺序与验收

1. 准备副本并显式启用流水线，保留健康配置原始字节；计算一次帧对和完成等待限额。
2. 默认依次执行 on-01/off-01/on-02/off-02/on-03/off-03。`--rounds` 为3–10，`--first off` 可反向。正常组健康等待、600秒采样、录满、停止、两轮独立读回/逐帧哈希、IMU/Global/DDS 双向验收均沿用原工具。任一失败停止后续组，并且不注入 SIGKILL。
3. 全部正常组验收通过且清理成功后，在独立 `crash/` 中启动三台。恢复实验开启 SDK 诊断，使用同一镜像、源配置、帧对/字节限额和 ROS domain；生成配置只允许正常运行身份的差异，并与 A/B 的归一化配置摘要一致。
4. 自动等待三台健康并仍在录制、IMU/Global/shared ROS context 正常，核对并固定三台真实 worker 身份，等目标相机出现非空检查点，再仅对本轮目标容器注入 SIGKILL。
5. 执行受限时、日志上限保护的恢复审计：两个独立 reader、实际读回与逐帧/索引/载荷哈希、恢复数量等于最终检查点且不小于注入前前缀；普通完整 reader 必须拒绝 partial，不得生成 COMPLETE，审计前后检查点原始字节不变。两种 IMU 样本均需非空。随后复核目标已停止，另外两台仍健康、未发生数据/IMU/录制错误，并再次核对运行身份。
6. 最后停止本次实验资源并汇总。整体成功要求正常 A/B、独立恢复和清理全部成功。SIGKILL 通过仅代表可靠前缀恢复，不代表断电测试或整个目标完整录制。

30 FPS、默认健康等待300秒时，正常每组每台31,680对，六组仅采集约106分钟，另加构建、审计与冷却；恢复实验在非空检查点后即注入，不必等录满。128 GiB 是每台每组限额，不是总容量或空间预留。必须预算六组原生数据、DDS观察数据、恢复数据及余量，不能边测边迁移/归档制造额外磁盘竞争。

健康等待默认300秒，审计/构建默认各3600秒，可用 `--healthy-timeout`、`--completion-timeout`、`--audit-timeout`、`--build-timeout` 修改。`--pairs` 不得低于原工具计算的安全下限；`--poll-seconds` 为1–10秒，组间 `--cooldown-seconds` 默认为30秒。此入口不启用 perf；需要 profile 时另开单独实验，避免混入诊断开销对照。

## 结果与失败回滚

```bash
sudo cat /srv/sdk-test/c063450-full-lifecycle-01/summary.md
sudo python3 -m json.tool /srv/sdk-test/c063450-full-lifecycle-01/summary.json
sudo cat /srv/sdk-test/c063450-full-lifecycle-01/ab/summary.md
```

整体以退出码0、`result.json.ok=true`、`summary.json.ok=true` 为准。根汇总保留完整 A/B 统计及独立恢复结果：CPU、同时采样 RSS 峰值/生命周期 HWM、追加 P99 界限、验证耗时、SDK配对/队列/锁等待指标及各轮原值；口径沿用 [AB.md](AB.md)，不合并百分位或将 SDK off 不可用计作零。正常 A/B 通过、恢复失败时整体仍失败，正常 A/B 的独立统计不冒充恢复通过。

证据包括 `controller/` 子进程身份与有界日志、`ab/` 六组及部分完成结果、`crash-before-injection.json`、`crash-injection.json`、`crash-after-audit-status.json`、`crash-final-status.json`、`crash/` 配置/状态/录制/注入及恢复审计文件。SDK恢复诊断目录为 `/dev/shm/gdp-retest-{run_token}`，实际位置保存在 `crash/run.json`；保持该目录到证据整理完成，重启会丢失 tmpfs。故障后不自动清理证据或恢复生产。

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/run_retest.py rollback \
  --root /srv/sdk-test/c063450-full-lifecycle-01
```

启动子进程、恢复容器启动和故障注入与回滚资源快照共用进程间锁；手动回滚先发布 ABORT，再等待已进入的启动操作结束后清理，防止回滚返回后又启动下一阶段。Ctrl-C/SIGTERM 会进入同一清理路径；手动回滚先写 ABORT，阻止控制器继续启动后续阶段，然后按出生时间/UID/进程组/token 停止自建子进程，再调用嵌套 A/B 回滚，按实验/run/固定镜像身份停止恢复审计容器及三相机容器。恢复审计使用固定命名且带运行标签的只读无USB容器，避免仅结束 Docker CLI 后遗留审计进程。身份不匹配拒绝发信号并保存错误；配置或镜像自检损坏不阻断基于已存身份的回滚。清理失败会撤销整体成功。所有录制和失败证据保留；确认实验资源停止后通过生产正常入口恢复。

测试使用合成后端与真实宿主子进程，未新增三相机实测结论。另收到的 `b459003` + 外部 LinkerHand MCAP 报告不属于本检查点流水线验收；其 SDK/DDS/IMU 条件也不同。现场保持预览、外部录制器和其他系统负载在各组一致，建议关闭预览；不要把外部 MCAP 性能差异归因于本原生录制优化。

## 本提交验证

2026-11-01：隔离 Linux 环境完整 Python 回归411项，1项既有条件跳过，其余通过。新增复测专项13项，覆盖配置副本与冻结、至少三轮、固定镜像与限额、故障/中断/清理失败、两轮/hash/IMU恢复拒绝路径、存活相机和再次身份检查、实际宿主子进程、真实进程间启动/回滚互斥；另增加具名审计容器回归并验证批量控制器轮询间隔。没有修改或重跑 Rust 数据面；没有进行新一轮物理三相机测试，不将合成控制流程通过当作实机性能或恢复通过。

## 结果打包

控制器结束后，可用 `pack_results.py` 一键收集配置、镜像摘要、性能采样、恢复验收和失败日志，排除原始录制，生成压缩包与 SHA256 清单。命令、限额和缺失说明见 [PACK.md](PACK.md)。
