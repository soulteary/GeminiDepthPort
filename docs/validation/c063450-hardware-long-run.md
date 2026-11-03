# c063450 三相机全程采样长测

## 固定对象与新目录

使用包含本版工具的源码运行控制器。采集镜像必须来自 `c063450c6c28f0bdec1d6a4f180871bd95fead96` 或可验证的后继提交；入口核对 runtime inventory 和 Git ancestry，不接受旧的 3dff4fd 镜像。若要精确测 c063450，可从独立源码工作树构建：

```bash
git worktree add /srv/GeminiDepthPort-c063450 c063450c6c28f0bdec1d6a4f180871bd95fead96
cd /srv/GeminiDepthPort-c063450
sudo bash experiments/sdk-2.10.6-diagnostics/build_runtime.sh local/gdp-sdk-diagnostics:c063450 linux/amd64
```

ARM64 改为 linux/arm64。用新版工具目录运行以下命令（已有固定镜像时不加 --build）：

```bash
sudo python3 /srv/GeminiDepthPort-tools/experiments/sdk-2.10.6-diagnostics/run_retest.py run \
  --plan /srv/sdk-test/three-camera.json \
  --image local/gdp-sdk-diagnostics:c063450 \
  --root /srv/sdk-test/c063450-full-lifecycle-01 \
  --rounds 3 --first on --seconds 600 \
  --max-bytes 137438953472 --ros-domain-id 78 \
  --disk nvme0n1 --camera b --execute
```

工具冻结三台实际 capture/health 配置，保留 IMU 队列、时间戳、QoS 等设置；仅按现有复测策略启用 chunked-v3/checkpoint pipeline。记录镜像 ID、runtime revision/hash 和配置摘要。主机应为原生 Linux/rootful Docker，先经正常生产入口释放相机，并记录设备、固件、USB 拓扑和背景负载。

输出根必须不存在；不得复用 e8912cb-retest-01、3dff4fd-retest-02 或任何旧目录。保存旧日志、failure 文件、partial、COMPLETE、官方失败汇总及 SHA256，不移动、删改或重新标记这些结果。空间不足时先停止新测试，不能通过删除旧失败数据绕过预算。每台每组 128 GiB 不是六组加恢复的总空间预算。

## 采样范围与文件

- `ab/<组>/lifecycle/samples.jsonl` 从相机启动前持续到 complete 后停止，或失败清理结束，覆盖 wait-healthy、600 秒窗口、wait-complete 与停止阶段。包含目标间隔 1 秒的独立资源采样（保留实际时间和最大间隔）以及控制器轮询状态；后者将完整错误状态先写入日志，再执行验收检查。
- `crash-lifecycle/` 覆盖独立恢复阶段的启动、注入、审计和清理。被主动 SIGKILL 的目标之后不可读是预期事实，不当作它继续健康；另外两台仍按原恢复规则检查。
- `summary.json` 列出采样数量、字节数、时间边界、线程 CPU、每台资源样本数、观察到的录制状态和错误。正常组必须 sampling_ok=true 且三台 complete_observed=true，日志/历史超限或正常运行阶段读取失败不得接受。
- 原有 `checkpoint-samples/`、SDK 诊断及 `wire.sqlite3` 仍使用独立固定 600 秒窗口。CPU/RSS 全程曲线另行分析，不与旧窗口混算；磁盘累计计数可按单调时间差计算吞吐，P99 仍保留其原始直方图区间，不能平均百分位。

资源行包含进程 CPU ticks/RSS、cgroup、磁盘计数、检查点事件，以及 IMU、status_persistence 和 cleanup 字段。全程采样不增加传感器频率、不更改队列、不缩减数据读回。初始化和停止后的读取失败也保留在日志中；状态缺失不解释为零开销或无故障。采样线程 CPU 不含其启动 Docker 查询子进程的 CPU，不等于所有观察器总开销。

## 正常组验收

1. 三台固定身份、配置和镜像一致，无重启、采集/ROS/时间戳错误。Global 来源必须显式有效；device/global 递增和 System 时间规则沿用现有校验，不能换用接收时间补洞。
2. IMU 两种流均有数据；queue_high_water 不超过配置中的 queue_capacity，imu.error 为空、failure_observation 为空。正常组不允许 IMU_QUEUE_OVERFLOW；原生整对队列溢出也必须为 0，它与 IMU 队列是不同指标。
3. `status_persistence` 必须含 queue_wait、critical_wait、容量 16 的 critical_events。事件含提交/完成序号、进入/返回时间和结果；持久化错误为 0，所有完成的关键事件成功。`cleanup` 的完成历史不超过 8 条。active 与历史均是快照，不把旧 span 外推为整个卡顿。
4. 三台 raw complete，written_pairs=max_pairs=计划目标，COMPLETE 有效。正常审计必须 audit_version=2、两次独立实际读回、逐帧哈希和原生 counts/归一化 ROS 深度各自校验通过；IMU/Global/DDS 双向匹配通过。全程采样不是替代审计。
5. 图像源序号缺口另报；录制/DDS 匹配通过不等于源端零丢帧。六组全部接受且清理成功后才能比较 on/off；一组通过不能宣布整套成功。

## 错误与独立恢复验收

出现故障时正常组应失败并停止后续组。保存首次 failure 文件、其源文件时间、最后正常状态、后续状态和全程采样，检查 IMU 错误是否与 ready=false、SESSION_FAILED/worker_reason、cleanup 阶段一致。短阶段可能落在轮询间隔内，未观察到某阶段不能直接证明未执行；也不能根据旧的 imu.error=null 排除随后错误。

IMU 出错且原生写入仍在读回时，未通过提交守卫的事务不得生成 COMPLETE。若原生事务已在更早时刻提交，后续采集错误不能将它伪装成未提交。若落盘本身阻塞，失败状态无法凭空变得可靠；原心跳/退出期限保持不变，监督器超时仍是失败而非成功。

六组正常验收后，独立 SIGKILL 必须在新 crash 目录执行：目标停止、另外两台保持健康，恢复数量等于最终确认检查点且不少于注入前前缀；两种 IMU 非空；两个独立 reader 实际读回并验证哈希；普通完整 reader 拒绝 partial，无 COMPLETE，审计前后检查点字节不变。保留 recovered 与 complete 的区别，SIGKILL 不代表物理断电验收。

## 交付

保留根/各组 result、summary、lifecycle、固定窗口采样、失败文件、runtime/config 摘要和审计结果。控制器结束后使用现有 pack_results.py 打包到新的文件名；全程 JSON/JSONL 会进入证据包和 SHA256 清单，原始载荷仍在现场保留。逐项检查打包警告、日志限额与恢复阶段是否实际执行，不能把未执行写成通过。
