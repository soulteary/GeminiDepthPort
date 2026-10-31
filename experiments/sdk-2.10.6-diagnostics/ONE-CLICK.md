# 一键三相机实机测试

入口 `run_hardware.py` 在独立实验目录依次执行可选构建、配置准备、启动、等待健康、同步采样、等待录制完成、停止、两轮读回审计和 DDS/native 验收。生产 SDK、门禁、配对/IMU/录制实现均未修改，也不会替用户停止或恢复生产采集。

## 使用前准备

使用原生 Linux、**本机 rootful Docker**、Compose v2。整个流程统一通过 `sudo` 使用同一 UID/GID，包括容器、采样器、DDS 观察器、审计和回滚。脚本拒绝非 root、rootless 和远程 Docker；不自动 sudo、不 chmod 777、不改生产配置。确保 USB/udev 授权已完成，先通过生产 GDP 正常保护/停止流程释放相机。

将 [三相机计划模板](three-camera.plan.example.json) 复制到自己的路径，填写三台相机已验证的 `capture.json`、`health.json` 绝对路径。要求与 [手动流程](HARDWARE.md) 相同：IMU 启用、device 配对、Global 映射、匹配的 quality 策略、两路可靠图像健康检查及独立序列号/namespace/frame ID。`complete` 采集策略不适用于当前原生帧对录制器；脚本不会静默改成别的策略。

## 一条命令运行

在 GDP 仓库根目录执行；输出根目录必须不存在，父目录所在磁盘应能承载三台相机的录制总量。

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/run_hardware.py run \
  --plan /srv/sdk-test/three-camera.json \
  --image local/gdp-sdk-diagnostics:one-click \
  --build --platform linux/amd64 \
  --root /srv/sdk-test/on-01 \
  --mode on --seconds 600 \
  --max-bytes 137438953472 --ros-domain-id 78 \
  --disk nvme0n1 --execute
```

ARM64 改为 `--platform linux/arm64`。已有已验证的实验镜像时省略 `--build`，使用它的镜像标签或 ID；后续配置仍固定到实际不可变镜像 ID。`--disk` 替换为真实录制盘设备名，可重复，也可省略。需要 CPU profile 的单独一轮可加 `--perf`；要求得到非空 perf 产物，否则本轮失败，`recorded_not_validated` 仍不代表 profile 内容经过自动正确性认证。

默认采样 600 秒、健康等待最长 300 秒。脚本按最高源 FPS 计算 `1.1 × FPS × (健康等待上限 + 采样秒数 + 60)` 的帧对目标，向上取整。30 FPS 时每台为 **31,680 对，约 17.6 分钟采集**；600 秒只是采样窗口，脚本会继续等录制完成再审计。显式 `--pairs` 不能低于计算下限，避免启动/发现期间已经消耗录制配额。字节限额是每台上限，必须高于实际预期数据量，限额耗尽不能算完成；脚本不会替用户扩容或删除旧数据。

可调整 `--healthy-timeout`、`--completion-timeout`、`--audit-timeout`、`--build-timeout`，均有有限上限；录制完成等待默认按最低源 FPS 和目标数量留出两倍时间加 120 秒。默认每 5 秒读取一次已发布状态，不修改采集热路径；采样期间持续检查容器与状态，出现提前录满、容器退出、不健康、时间戳/发布/持久化错误或队列错误即中止验收。

关闭诊断对照组使用同一镜像、相同配置、限额、窗口、硬件和磁盘，另起新根目录并改为 `--mode off`，不要再次构建：

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/run_hardware.py run \
  --plan /srv/sdk-test/three-camera.json \
  --image local/gdp-sdk-diagnostics:one-click \
  --root /srv/sdk-test/off-01 \
  --mode off --seconds 600 \
  --max-bytes 137438953472 --ros-domain-id 78 \
  --disk nvme0n1 --execute
```

每组结束并释放 USB 后再启动下一组。建议交替至少三轮。关闭组不启动 SDK 快照采样器；仍执行相同的 CPU/内存/追加检查点采样、实际 DDS 观察和完整审计。脚本/观察器有自身开销，不把它们的 CPU 或 profile 开销当成采集性能收益。

## 结果与失败证据

退出码 0 且根目录 `result.json` 中 `ok=true`、`hardware_acceptance_verified=true`，才表示本轮正常录制与 DDS 联合验收通过。还会检查采样器真实摘要：仅子进程退出 0、采样时间不足、存在错误/中断或缺失摘要均不算通过。

`source_gap_free` 和 `source_sequence_gaps` 独立记录 SDK 源序号缺口；正常录制/双向匹配通过不表示消除了 66 ms 间隔或完全无缺帧。`crash_recovery_verified=false` 明确说明本流程没有自动注入崩溃，SIGKILL 恢复仍按手动流程在另一新目录进行。

输出包括：

- `result.json`、`settings.json`、`events.jsonl`、`children.json`：UID/GID、阶段、固定镜像/清单身份、失败原因、清理结果和回滚 argv。
- `latest-status.json`、`final-status.json`、`camera-*.log`：阶段状态与身份验证后的容器日志。
- `sdk-final-*/`：停止前后最后的私有、有界 SDK 快照字节，避免仅保留易失 tmpfs 路径。
- `capture/`：原有生成配置、状态、录制和 partial、检查点、审计 JSONL、DDS 数据库及比较结果。
- `capture/checkpoint-samples/`、`capture/sdk-samples/` 和各采样日志：性能、诊断与观测证据。

结果与关键阶段文件原子替换并 fsync；日志和 SDK 文件复制有上限。失败不重用目录、不续写、不删除、也不伪造 COMPLETE。日志超过上限、超时和 Ctrl-C/SIGTERM 均进入失败路径；清理错误会将本轮标记为失败。磁盘/硬件故障仍可能阻止最新证据落盘，以实际已保存文件为准。

## 回滚与恢复生产

失败或取消时脚本会先停止本轮独立命名、带 run 标签与固定镜像身份的 DDS 观察容器，终止自己启动的采样进程，再通过原有身份检查停止实验相机容器。没有 Docker socket 挂载、没有全局 `docker stop`/`compose down`、不停止其他采集。身份不一致时拒绝对该容器发信号并记录清理失败。

强制杀死脚本、Docker 暂时不可用或清理未完成后，可单独执行：

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/run_hardware.py rollback \
  --root /srv/sdk-test/on-01
```

回滚核对保存 UID/GID、run 身份、实际容器标签与镜像 ID，不依赖成功的镜像自检或未变动的配置。Linux 下还持久化自建进程的 PID、启动时钟、UID、独立 session/进程组及私有运行 token；强制杀死主脚本后的回滚会逐项复核这些身份再清理遗留采样进程，PID 已复用时拒绝发送信号，不按名称扫描或杀死其他任务。准备阶段失败且尚未创建任何运行元数据时记录“未启动”；已进入运行阶段但身份文件丢失时拒绝猜测目标。原有 `hardware.py rollback --run-dir /srv/sdk-test/on-01/capture` 仍可作为高级独立入口。

确认实验容器都停止、USB 已释放，再通过原生产 GDP 正常启动流程恢复原配置/原镜像。实验未替换生产 SDK，保留全部失败证据后再由用户决定归档；脚本不自动恢复生产，也不删除数据。

## 自动测试的范围

`tests/test_sdk_one_click.py` 使用受控状态/时钟/SDK 工具替身，测试 UID/平台与 Docker 拒绝、目标预算、健康/完成超时、采样失败、两轮/哈希验收、SDK 缺口区分、信号取消、部分启动与身份检查回滚；另启动真实子进程验证超时后的进程回收与日志保留。它不打开真实相机，不把模拟结果当作三相机实机验收。既有 SDK 镜像与原生读回/恢复实现保持不变；现场仍需运行上述命令获得本机结果。

验证结果与模拟/实机边界见 [ONE-CLICK-REPORT.md](ONE-CLICK-REPORT.md)。
