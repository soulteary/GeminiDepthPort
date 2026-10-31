# SDK 2.10.6 第一阶段诊断实验

固定 SDK 基线：`575347ca742d219ef61b1e214e97043f17f48d23`（2.10.6）。配套 GDP 基线为 `4f06290`。只做观测：不改变配对的毫秒比较、半周期窗口、聚合模式、队列容量、旧帧排除策略、传感器配置、SDK 时间戳、回调顺序或录制协议。默认关闭。生产 `capture.Dockerfile`、SDK 来源/摘要门禁、Rust 采集和录制代码均未改动。

本目录保存可重新应用的 `sdk-diagnostics.patch`、相同驱动的原版/补丁构建、功能与开销测试、只读现场采样器及测量报告。SDK 实验检出使用本地分支 `gdp/2.10.6-diagnostics`；不要向官方仓库推送实验。补丁只改内部 pipeline 源和私有工具头，不修改任何 `include/` 公开头或新增导出 C API。新代码使用 SDK 的 C++11 标准，Linux 支持环境变量与私有文件输出；其他平台保持关闭。

## 启用与边界

仅对加载了实验库的测试进程显式设置：

```bash
install -d -m 700 /dev/shm/gdp-sdk-diag/camera_a
export OB_GDP_DIAGNOSTICS=1
export OB_GDP_DIAGNOSTICS_DIR=/dev/shm/gdp-sdk-diag/camera_a
# 再启动使用实验 SDK 的专用采集程序。
```

三个容器各用一个新建私有 bind 目录，确保 SDK 进程用户拥有它；容器内使用其真实挂载路径。环境变量要传入实际加载 SDK 的 worker，不能仅设置在宿主机 shell。输出目录不得为末级符号链接、不得允许组/其他用户访问。缺少目录、目录不合格、初始化失败或超过进程内 16 个活跃诊断实例时，启动时给出 disabled 原因，诊断不启用，SDK 采集策略不变。目录拒绝不等于诊断验收通过；必须实际取得有效快照。

未设置 `OB_GDP_DIAGNOSTICS`、值不是精确字符串 `1` 时，不创建报告线程、文件或状态数组，不在诊断调用点读时钟或更新原子统计；仍有空指针检查和内部对象的指针存储，不能宣称二进制完全零开销。

每个 Pipeline 一个后台线程，约每 5 秒输出一次最新快照，销毁时 join 并再输出最终值。名称包含 PID、创建时单调时间和实例编号，初始化时以 exclusive-create 预留，不覆盖已有实例。每份快照不超过 64 KiB、文件权限 0600，通过同目录临时文件和 rename 发布。建议 SDK 先输出到有容量预算的本机 tmpfs，由只读采样器另行保存，避免与录制磁盘争用。普通文件 I/O 仍可能受文件系统阻塞影响，SDK 销毁等待报告线程 join，不承诺存储故障下的硬截止退出；不要使用 NFS 等可能长时间挂起的诊断目录。诊断不 fsync，不具有录制检查点持久性；I/O 或格式化失败仅增加 sink_errors 并从后台告警，不抛回采集线程。文件属于私有诊断，可能包含原始帧序号及时间戳，不自动上传。

计数器和直方图为固定数组；事件环固定 64 条。事件写入使用 try-lock，遇到争用就放弃该条事件并计数，不等待报告线程。报告线程只在复制固定事件数组时短暂持有这个锁，JSON 构造和文件写入不在 SDK 热路径或事件锁内。快照中的不同原子字段不是事务性同一时刻读数；运行中各计数不能强行做严格等式，停止后的最终快照用于确定性核对。

## 字段解释

- `input_frames`：进入聚合器的原始帧数，streams 按 SDK OBFrameType 分列。不是 USB 端收到的全部帧，也不包含 GDP 直接 sensor 接口下的 IMU 回调。
- `candidate_outside_window`：候选比较超出既有窗口的次数。同一帧可能多次成为候选，不能作为唯一丢帧数。
- `aggregate_queue_limit_hits`：聚合队列大小达到既有阈值的次数；它会触发 SDK 原有处理，不等于当时已经丢帧。`aggregate_queue_peak` 在原有聚合锁内观察，不增加原队列锁。
- `policy_dropped_sets/frames`：不满足聚合输出策略而实际未输出的集合/帧数。streams.policy_dropped 按类型分列，事件包含原序号、device/system 时间。
- `emitted_sets/partial_sets`：聚合器调用既有输出回调的次数及其中的不完整集合。只代表提交尝试，不代表消费者已收到。
- `output_queue_dropped_sets`：Pipeline 的既有 full 分支实际成功 dequeue 的旧集合数。若消费者竞态已取走集合，没有出队对象就不计为实际排除。没有修改 SDK 的队列并发实现。
- `output_enqueue_failures`：原有 enqueue 返回 false 的次数，原 SDK 对返回值的处理策略不变。
- `wait_timeouts`：waitForFrame 返回空的次数，包含非阻塞轮询；不等于相机无帧或丢帧。
- `flushed_frames`：配置重置、清理及销毁时清空聚合队列的帧数，与运行中的配对排除分开。
- `sync_mode/aggregate_output_mode`：实际 SDK 枚举值；streams 另有 normal/sync 队列阈值和 `half_gap_ms`，不修改原算法。

锁 site 为 `pipeline_callback` 和 `aggregator_push`，各自报告 wait 和 hold 的次数、累计纳秒、最大值、64 个 log2 纳秒桶。bucket 0 是 [0,1] ns，bucket k>0 是 [2^k,2^(k+1)-1] ns。wait 从调用原锁前到取得锁，hold 从取得锁到原 unlock 前；包含诊断自身及调度开销。外层 Pipeline hold 包含聚合器与输出回调，两个 site 的 hold 有嵌套，不能相加当作总延迟。没有把 waitForFrame 等待帧的时间冒充 mutex 等待，也没有测全部 SDK 锁。

事件 reason：0=候选窗口外，a/b 为原算法使用的 ref/target **毫秒**；1=聚合阈值命中，a/b 为队列大小/阈值；2=输出策略排除，sequence 为原帧序号、a/b 为 device/system **微秒**；3=输出队列实际排除，a 为配置容量。事件流类型使用 SDK OBFrameType 数值。输出策略排除事件保留原始来源，不推算缺帧，也不把 system 相等当作异常。

`events_total` 为成功进入事件环的累计条数，`events_overwritten` 为覆盖旧槽的条数；`event_skipped_contention` 是 try-lock 放弃事件的次数。最后 64 条只是样本，不是无损每帧日志。`events_snapshot_available=false` 表示该轮后台没取得事件复制锁，计数仍可用。所有时钟均只用于观测；`snapshot_unix_ns/snapshot_monotonic_ns` 是近邻时钟锚点，不是硬件曝光时间。

## 原版与补丁构建

先准备本地可读取固定提交的官方 SDK Git 检出。构建在独立输出目录进行，不修改该检出的源码，不从宿主机替换生产 SDK。

```bash
docker build -f experiments/sdk-2.10.6-diagnostics/Dockerfile \
  -t gdp-sdk-diagnostics-build:local .
bash experiments/sdk-2.10.6-diagnostics/build.sh \
  /absolute/OrbbecSDK_v2-git /var/tmp/gdp-sdk-build-01
```

开发构建器现基于公开 `ubuntu:22.04`，不再依赖 GDP 私有/本地基础镜像。原阶段一开销报告来自当时记录的 Debian/GCC 环境，仍保留其原始结果；更换工具链后应重新跑 on/off/before，不能将旧报告当作新镜像实机数据。

脚本 git archive 原始提交，再复制后应用补丁；原版与补丁的 SDK 都以 Release、相同依赖/选项构建，未改配对和 USB 参数。源码提取和驱动复制在构建容器内使用当前文件时间，避免宿主机/虚拟机时间偏差造成 Make 反复重建。SDK 本身会在 libuvc 源树生成配置头，因此实验副本可写。不要把生成文件混入补丁。

## 功能与开销对照

```bash
docker run --rm --init --network none --cap-drop ALL --read-only --tmpfs /tmp \
  --user "$(id -u):$(id -g)" \
  -v "$PWD:/work:ro" -v /var/tmp/gdp-sdk-build-01:/experiment \
  -w /tmp gdp-sdk-diagnostics-build:local -c '
    python3 /work/experiments/sdk-2.10.6-diagnostics/run.py \
      --before /experiment/before --after /experiment/after \
      --before-source /experiment/before-source --after-source /experiment/after-source \
      --output /experiment/measure-01 --repeats 5 --paced-seconds 8 --pipelines 3
  '
```

输出目录的实际拥有者须与容器用户一致；在 rootless Docker/Podman 中先检查 UID 映射，不能仅通过加权限或 chmod 777 绕过诊断的私有目录检查。使用新输出目录，失败证据保留，不重用旧结果。

测试驱动链接真正的 SDK Pipeline/FrameAggregator 与 FrameFactory，但设备是内存夹具，不启动硬件流。`-fno-access-control` 仅作用于测试驱动以注入输入和控制争用，不加入 SDK 编译 flags，也不修改公开头。7 种行为测试包括完整对、时间偏差、不完整输出、单流饥饿/聚合阈值、慢消费者输出溢出以及两处真实锁争用。对照输出集合/帧数、原序号、device/system 时间和载荷内容的行为摘要。该摘要用于确定性对照，不替代 GDP 的生产 SHA256。

另测试关闭时不输出、非法/符号链接/公开目录拒绝、64 条环覆盖与跳过计数、多生产线程更新、16 个实例上限、锁 wait/hold，以及诊断目录被删除后采集仍成功。版本与 406 个公开 C 导出符号一致，逐文件核对 include/ 公开 C/C++ 头完全一致。

开销为三组：未修改原版 before、补丁但关闭 off、补丁开启 on；逐轮反转先后顺序。吞吐场景不限制速度，节奏场景为 30 对/秒，三个独立进程模拟三个隔离 worker。每个 Pipeline 使用 2×2 图像，不包含 USB、IMU、真实传感器线程、DDS、原生录制或完整图像复制成本。CPU 包含进程所有线程、初始化/销毁和诊断导出，当前驱动的 RSS 读取 /proc/self/status VmHWM，另保留 rusage_maxrss_kib 用于发现 exec 前继承高水位，callback P99 是驱动调用 onFrameCallback 的实际排序分位。无并行构建/程序测试。不能把微基准百分比直接外推为实机整体开销；报告保留关闭路径和开启路径的全部结果。

## 现场采样与 GDP 关联

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/collect.py \
  --source a=/srv/sdk-diag/a --source b=/srv/sdk-diag/b --source c=/srv/sdk-diag/c \
  --seconds 600 --interval 1 --max-mib 64 --output /var/tmp/sdk-diag-capture-01
```

这里只读取用户指定的 SDK 快照，不设置环境、启动设备、发送信号或修改配置。输出目录为新建 0700，JSONL/摘要为 0600；每次限制 8 个来源、每来源 256 个历史实例文件、总输出字节和运行时长。空的初始预留文件会等待后续快照；全部来源必须实际出现有效数据。长期复用目录可能超过历史实例文件上限，使用每轮独立目录。

采样器保存 SDK 原快照、读取时主机的 wall/monotonic 锚点和新增的事件样本，区分开始前已覆盖的事件与采样期间未观察到的事件。事件遗漏不等于录制丢样；累计诊断计数才用于数量分析。SDK 只在 5 秒刷新时提供新信息，提高读文件频率不会获得逐帧时间分辨率。使用 Unix/monotonic 配对锚点检查 Linux time namespace/NTP 差异后，再与 GDP `checkpoint_probe.py` 的队列、fsync/验证阶段和 CPU profile 关联；嵌套 span 不相加。

现场先确认实际 SDK 库二进制摘要和实验补丁身份。GDP 的生产 runtime 有固定 SDK 来源/哈希门禁，本实验不绕过门禁、不修改原生产 manifest、不将自编译库伪装成厂商发布库。现已提供独立的三相机实验镜像、runtime 绑定和启动/采样/回滚工具，操作见 [实机实验说明](HARDWARE.md)。生产管理入口继续拒绝实验 SDK；第一阶段报告仍仅描述 SDK 合成验证。

GDP 全工作区回归另核对时间戳、两轮实际读回、逐帧/索引哈希、写入/提交故障和 SIGKILL/前缀恢复；SDK 诊断不改变这些路径。它也不能修复或证明已经消除物理采样率差异、66 ms 缺口或断电持久性。

实际结果、测量口径修正与限制见 [实验报告](REPORT.md)。原版吞吐驱动的复现补丁 probe-metric-v1.patch 只回退计时完成后的内存读数；不修改 SDK。


三相机实机实验：见 [HARDWARE.md](HARDWARE.md)，使用 `build_runtime.sh` 构建和 `hardware.py` 控制；公开基础构建、镜像烟测和回归结果见 [RUNTIME-REPORT.md](RUNTIME-REPORT.md)。


一键运行三相机测试：使用 [run_hardware.py / ONE-CLICK.md](ONE-CLICK.md)，统一 root UID 自动等待健康、采样、录制完成和两轮读回/DDS 验收，失败保留证据并提供独立回滚命令。
