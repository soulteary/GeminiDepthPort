# 三相机 SDK 诊断实验镜像

本入口只用于受控实验。生产 `capture.Dockerfile`、官方 SDK 来源、Rust 生产 runtime 门禁、时间戳校验与录制代码均未修改。诊断补丁仍是第一阶段原补丁，默认关闭，未调整配对窗口、队列、同步或丢弃策略；它用于定位原因，不承诺消除 66 ms 缺口。

完整自动流程推荐使用 [一键实机入口](ONE-CLICK.md)，它统一 UID、自动等待、采样、验收并保留回滚入口。下文为分步高级操作。

## 构建与身份绑定

在仓库根目录执行，构建机器需要 Docker、Git 和公开网络。无需提前准备任何 GDP 本地基础镜像。默认构建 Linux AMD64；ARM64 机器显式选择 `linux/arm64`。不要将不同架构的 SDK 库混用。

```bash
bash experiments/sdk-2.10.6-diagnostics/build_runtime.sh \
  local/gdp-sdk-diagnostics:2.10.6 linux/amd64
```

可先运行不打开设备的镜像集成烟测，输出目录必须是新目录：

```bash
python3 experiments/sdk-2.10.6-diagnostics/smoke_runtime_image.py \
  --image local/gdp-sdk-diagnostics:2.10.6 --output /var/tmp/sdk-image-smoke-01
```

该烟测还会替换 SDK/补丁字节、伪造清单种类以及省略配置，确认镜像拒绝启动；每个容器均无网络、无 USB。它不调整真实库或生产镜像。

依赖的公开基础为 `rust:1.85.1-bullseye` 和 `ros:humble-ros-base-jammy`。构建直接获取 SDK 固定提交 `575347ca742d219ef61b1e214e97043f17f48d23`，校验并应用 SHA256 为 `0db54c7c1bcafa9d18cd51bddcd4cf0d0c733c967ff0f9542f7796b770a0757c` 的补丁。编译器、架构、SDK 实际摘要、406 个公开 C 导出、33 个原始公开头及 CMake 生成的 `Export.h`（共 34 个安装头）的校验结果进入 `/opt/gdp-experiment/sdk-build.json`；SDK 和第三方许可证随镜像分发。

`/opt/gdp-experiment/runtime.json` 进一步绑定 SDK、ROS bridge、capture-service、两轮读回校验器及实验脚本、补丁、许可清单中每个文件的 SHA256。SDK 构建记录、运行清单和实际 SDK 字节必须一致。镜像构建及容器启动均复核清单，采集服务继续复核原有三项二进制摘要。镜像 ID、运行清单摘要、配置和健康配置摘要、run/instance 身份另外写入每台相机的 `experiment.json`。不接收 `LD_PRELOAD`/`LD_AUDIT` 替换。

镜像 backend 标签为 `rust-sdk-diagnostics-v1`，实验身份为 `sdk-2.10.6-diagnostics-v1`，不生成生产 `/opt/gdp-capture/runtime.json`。**生产管理入口会拒绝它；请使用下面的独立工具，不能把实验 SDK 的摘要填进生产清单以绕过门禁。** `gdp_revision` 代表构建时已提交的 GDP 服务/ROS/录制源码；构建脚本拒绝这些目录有未提交修改，实验新增文件本身另由文件清单绑定。基础镜像标签和 apt 包会随时间变化，发布实验产物时一并保存镜像 ID、构建日志和 runtime 清单；需要跨机器完全相同的实验请导出/导入同一个镜像，不要仅凭标签重新构建。

```bash
docker image inspect local/gdp-sdk-diagnostics:2.10.6 > /var/tmp/sdk-experiment-image.json
docker run --rm --network none --read-only --cap-drop ALL \
  --entrypoint cat local/gdp-sdk-diagnostics:2.10.6 \
  /opt/gdp-experiment/runtime.json > /var/tmp/sdk-experiment-runtime.json
```

## 准备三台相机

实机宿主应为原生 Linux，Docker Compose v2，USB/udev 已按现有 GDP 部署正确授权。下面实机控制命令统一使用 `sudo`/root UID，准备、容器、采样与审计保持同一 UID，避免私有目录被不同 UID 的观察器挂载后无法读取。rootless Docker 的 UID 映射必须先验证；不能将私有诊断目录改成 0777。

复制 [三相机计划模板](three-camera.plan.example.json)，填入三台相机**已验证**的 `capture.json` 和对应 `health.json` 绝对路径。保留原文件，模板只指向它们，不修改源配置。源配置须包含：

- 三个不同序列号、namespace、图像及 IMU frame ID，已验证的流分辨率/FPS、IMU 频率/量程和 QoS。
- `imu.enabled=true`、`pairing.clock=device`，显式 Global 采集策略与 `ros.stamp_source=sdk-global-unix`。
- 当前主线生成的 `quality_policy` 和匹配的 `quality_runtime`。原生录制要求完整帧对采集策略；`complete` 策略允许发布不成对图像，当前原生帧对录制器会拒绝它。需要更换策略时应先独立验证并提供相应源配置，工具不会静默改变采集策略。

例如手动第一轮测量 120 秒，建议设每台 9,000 对（约 300 秒），预留启动、DDS 发现和收尾余量；一键入口会按健康等待上限自动计算更保守的目标。实际是否覆盖完整测量窗口以状态和记录为准。字节限额须高于预计载荷，磁盘空闲容量还需覆盖索引、检查点和三台相机总量；达到字节上限不是正常完成验收。下面每台 128 GiB 只是示例限额。

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py prepare \
  --plan /srv/sdk-test/three-camera.json \
  --image local/gdp-sdk-diagnostics:2.10.6 \
  --run-dir /srv/sdk-test/on-01 \
  --diagnostics-root /dev/shm/gdp-sdk-on-01 \
  --mode on --pairs 9000 --max-bytes 137438953472 --ros-domain-id 78
```

`prepare` 不启动相机。它运行无网络、无 USB 的镜像自检和三份真实服务配置校验，复制并绑定源配置，在新目录生成 `compose.json`。仅更新实验 SDK/bridge 摘要、实验状态/控制/录制路径和身份，禁用重启，设置显式录制限额并开启检查点观测。已有队列容量及 checkpoint pipeline 设置保留；源配置未定义时队列为 16、pipeline 关闭。源健康检查内容保持不变，但要求两路真实图像 topic、frame ID、尺寸与可靠 QoS 一致；发现加采样时长之和须小于 24 秒，以符合健康检查截止时间。

三个容器使用固定镜像 ID、独立状态/录制/诊断目录、只读根文件系统、去除 capabilities，无 Docker socket，显式挂载 USB/udev 和私有 tmpfs。SDK 诊断每 5 秒刷新，优先放在 `/dev/shm`，避免将诊断输出混入录制盘延迟。每轮使用新目录，不重复启动已产生状态或录制文件的运行。

## 启动、采样、正常验收

先通过生产 GDP 原有保护/停止流程释放相机，再启动实验。工具发现已有容器挂载 USB 时拒绝启动，不会替用户停止生产采集。USB 未通过容器暴露的其他宿主进程需由现场确认已退出。

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py start \
  --run-dir /srv/sdk-test/on-01 --execute
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py status \
  --run-dir /srv/sdk-test/on-01
```

等三台 `health=healthy` 且录制仍在进行后采样。健康检查核对当前 worker/session、Global/IMU/shared context 和实际 ROS 图像消费；开启诊断时还要求当前 worker 的 SDK 快照持续更新且无 sink error。

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py sample \
  --run-dir /srv/sdk-test/on-01 --seconds 120 --disk nvme0n1
```

`--disk` 替换为实际 Linux 块设备名，可重复；不用猜测设备名。可另加 `--perf` 采集 30 秒、49 Hz 的 profile，权限或工具不足会保留失败结果，不改 sysctl。建议先做无 perf 的 A/B，再单独 profile，避免采样成本混入诊断开销。

采样同时运行现有 `checkpoint_probe.py`、SDK 只读采样器和实际 DDS 观察器。证据在 `checkpoint-samples/`、`sdk-samples/`、`wire.sqlite3` 和各自日志/摘要；关闭模式不启动 SDK 采样器。`sample-commands --run-dir ... --seconds ...` 可输出同一组 argv 供现场集成。输出上限、CPU、内存和漏观测计数均保留；诊断事件是有界抽样，事件遗漏不能算成录制丢样。

测量完成后继续等待三台 `raw_recording.state=complete`，再停止实验并做两轮独立读回。提前停止会留下 partial，不能冒充正常完成。

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py stop --run-dir /srv/sdk-test/on-01
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py audit --run-dir /srv/sdk-test/on-01
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py verify-dds \
  --run-dir /srv/sdk-test/on-01 --trim-seconds 5
```

`audit` 在无 USB、无网络、只读录制挂载下调用镜像内固定摘要的当前主线校验器。第一轮读取并导出逐帧、逐 IMU 样本证据，第二个新 reader 再实际读取并校验所有帧/索引/载荷哈希；检查帧对/IMU 总数、COMPLETE 与检查点，并确认审计未修改检查点。两轮不是复用内存结果；页缓存不作清空，因此不能称为两次物理介质读取。

`verify-dds` 使用主线纯比较器，将实际 ROS 图像哈希、独立深度单位归一化参考哈希、Global header 映射、metadata 和 IMU 值逐样本双向关联。边界各排除显式 5 秒，公共窗口不足即失败；原始 source 的序号缺口、system 重复和最大间隔单独报告。system 允许非递减，device/global 要求严格递增，不将 system 同值误报为丢帧。DDS 观察器还核对 QoS、每个相机内共享 participant、相机间 participant 隔离。

验收必须全部满足：三份配置/镜像/SDK 绑定一致；无重启、时间戳回退、发布失败、IMU 溢出、录制队列溢出、持久化错误；开启模式三路诊断快照有效且 sink error 为零；每台完成目标帧对且 native/IMU 数量匹配；读回两轮与哈希通过；公共窗口 DDS/native 双向匹配。SDK 排除/溢出计数可非零，应与 GDP 拒绝原因和帧号缺口一起解释，不因诊断启用就宣布“完全无缺帧”。

性能报告按 on/off 至少交替三轮，使用**同一镜像 ID**、同一配置、相机、固件、USB 拓扑、磁盘、测量窗口和录制限额；仅更换新目录与 `--mode`。记录 `sdk-build.json`/runtime、每台及合计 worker CPU 核数、RSS/HWM、cgroup 内存（与文件缓存区分）、append P99、checkpoint/fsync/两轮验证耗时、队列峰值、SDK lock wait/hold 和配对排除/溢出累计增量。保留 SDK 关闭时无快照证据；perf/观察器 CPU 与被测采集 CPU分开列出。不要将原生驱动对比、不同 ROS 版本或外部 rosbag2 数据混作本镜像诊断开销。

## 崩溃恢复单独实验

另准备新目录与足够长的录制目标，启动并等健康。只选择本轮实验中一台相机，在尚有 `.partial` 且检查点已写入时注入 SIGKILL：

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py crash \
  --run-dir /srv/sdk-test/crash-01 --camera b --execute
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py audit \
  --run-dir /srv/sdk-test/crash-01 --camera b --recovered
sudo python3 experiments/sdk-2.10.6-diagnostics/hardware.py rollback \
  --run-dir /srv/sdk-test/crash-01
```

工具核对 run label、镜像 ID 和固定容器 ID，禁用自动重启，保存注入前状态及检查点。恢复必须保留已确认前缀，执行两轮新 reader、逐帧哈希，恢复帧对数等于最终检查点；普通完整 reader 必须拒绝该 partial，不得生成 COMPLETE，读后检查点须字节不变。只能宣称 SIGKILL 的检查点前缀恢复通过，不能替代物理断电或未 checkpoint 尾部的持久性证明。保留失败数据、stderr、注入与审计日志。

## 回滚

`rollback` 与 `stop` 相同：先核对三台实验容器身份，随后只停止本轮运行，不删除容器、配置、镜像或录制。若身份不一致则停止操作，保留现场证据。配置被改动或镜像自检失败不会阻断回滚：停止入口只依赖保存的运行身份，并逐个核对实际容器的实验/run 标签及固定镜像 ID；部分启动只创建了一两台容器时也能回滚。生产配置和 SDK 从未被替换，所以不需复制任何库回去。确认实验容器均停止、USB 已释放后，使用原生产 GDP 的正常启动流程恢复原镜像/原配置；不要把实验 `compose.json` 交给生产管理器。

关闭 SDK 诊断需要新建 `--mode off` 的运行，不在活跃 worker 中修改环境或换库。撤回本实验时停止其容器即可，生产门禁仍保持原状。实机尚未在本开发环境执行；合成回归和镜像自检结果见 [运行镜像验证记录](RUNTIME-REPORT.md)。
