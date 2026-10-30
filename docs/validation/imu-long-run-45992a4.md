# 45992a4 三相机与 IMU 实机长测

被测采集服务固定为 `45992a4c0e81dd7a23b0b0f17fafd14a24d926b7`，Orbbec SDK 2.10.6、现有固定 SDK/驱动来源、ROS Humble/Fast DDS。新增工具是测试侧代码，不改采集服务。以下命令在专用 Linux 实机环境运行；本机合成 DDS 测试通过不等于相机长测通过。

## 工具与证据

- `scripts/validation/imu_long_test.py`：三个容器的身份/配置校验、CPU/内存/队列/追加 P99 采样、DDS 与原生录制比对，以及单个容器的显式 SIGKILL 和只读恢复验收。
- `ros/imu_long_run_observer.py`：实际订阅三相机图像、图像元数据、accel/gyro。SQLite 有容量上限，不保存图像载荷，只保存载荷 SHA256、header、尺寸和 IMU 数值；IMU CDR 摘要来自观察器本身。检查可靠/volatile QoS，以及八个图像/CameraInfo/元数据/IMU endpoint 的 GID 前 12 字节在会话内相同、跨相机不同且运行期间稳定。
- `audit_imu_long_run`：用生产读取器完整校验录制，并逐条导出可比对的原始 IMU 与图像摘要；随后新开读取器进行第二次实际读盘、哈希校验。额外审计不替代采集服务生产检查点和最终读回。原生 V3 生产两轮次数由最终状态另行核对。
- 已有 `checkpoint_probe.py` 可补充物理磁盘/cgroup/perf 关联，见 [检查点采样](../development/checkpoint-observation.md)。

证据目录应位于本地磁盘，权限 0700；文件默认 0600。包含真实序列号、路径、配置、会话和性能资料，不自动上传。观察器的 SQLite 上限不含少量 journal/摘要文件；主机需预留额外空间，ENOSPC、超限、异常退出均按失败处理。采集和观察器最好使用不同磁盘。禁止为通过测试改写时间戳、补样、丢弃失败记录或清空错误计数。

## 准备

1. 在干净的 `45992a4` 源码及既有受控构建流程中构建生产镜像。记录 source commit、SDK/固件版本、image ID、生成清单和 capture-service SHA256。镜像必须带 `org.opencontainers.image.revision=45992a4c0e81dd7a23b0b0f17fafd14a24d926b7` 标签，例如构建时增加 `--label`。标签只是来源声明，不能代替受信构建记录或二进制摘要。
2. 三个容器分别绑定实际序列号、唯一 namespace/frame prefix、已验证的 30 FPS 图像档位；三台均需实际支持 IMU 与所选量程/频率。建议先做每台 120 秒能力验证，再三台同时运行。不能假定不同型号均有相同 IMU 支持。
3. 每个生成后的 `capture.json` 显式启用 IMU（例：accel/gyro 各 200 Hz，queue_capacity=128）、Global 消息映射、`pairing.clock=device`，保持原 `max_delta_us`。可选择 `strict-rgbd`；`complete` 发布独立图像而原生录制只保留完整对，工具会揭示这种范围差异，不能隐去未录图像。
4. 原生录制使用 `storage_format=chunked-v3`，开启 `checkpoint_observation`；分别测试 `checkpoint_pipeline=false/true`，其余配置保持一致。启用实际质量策略与快照绑定，确认状态中的 raw_recording 非 null。外部 ROSbag2 不能替代内置录制。
5. 所有测试容器禁止 `--rm`，并设 `restart: "no"`，服务 `max_restarts=0`，关闭自动拉起该测试实例的管理任务。保持原有 fsync、哈希、队列上限、保护协议。配置变更要重新生成并通过 `capture-service --config ... --validate-config`；不要修改运行中的 generation。
6. 正常录制必须达到 `max_pairs` 自然完成。**提前停止服务会产生未完成录制，不能当作正常完成。** 对 1 小时测量，例设 115200 对（30 FPS 约 64 分钟），足够容纳启动、暖机和观察窗口；按实际掉帧率留更长等待。8 小时轮次按相同原则扩大目标。空间按每帧真实尺寸、两路字节数、IMU、索引与额外审计输出估算，至少另留 20% 余量；`max_bytes` 也必须覆盖目标。

构建审计器（支持工具所在源码树，采集二进制仍是上述固定版本）：

```bash
cargo build --release --locked -p offline-depth --example audit_imu_long_run
```

复制 `scripts/validation/imu-long-run.plan.example.json` 为私有 `plan.json`。示例中的路径、容器名、摘要都必须换成实际值；不要直接运行占位配置。三份 config_host 是生成后的 capture.json，不是 inventory。status_host 和 recording_host 必须是容器状态/录制目录的实际 **bind mount** 对应宿主机路径；脚本会核对 Docker Mounts。最终目录名字从状态的 `raw_recording.job_path` 读取并映射到宿主机，不要猜测（实际名字可附带会话标识）。

```json
{
  "commit": "45992a4c0e81dd7a23b0b0f17fafd14a24d926b7",
  "cameras": [
    {"name":"a","container":"gdp-test-a","namespace":"/camera_a","config_host":"/srv/gdp-test/a/capture.json","status_host":"/srv/gdp-test/a/status.json","recording_host":"/srv/gdp-test/a/raw/ACTUAL_SESSION","binary_sha256":"TRUSTED_CAPTURE_SERVICE_SHA256"},
    {"name":"b","container":"gdp-test-b","namespace":"/camera_b","config_host":"/srv/gdp-test/b/capture.json","status_host":"/srv/gdp-test/b/status.json","recording_host":"/srv/gdp-test/b/raw/ACTUAL_SESSION","binary_sha256":"TRUSTED_CAPTURE_SERVICE_SHA256"},
    {"name":"c","container":"gdp-test-c","namespace":"/camera_c","config_host":"/srv/gdp-test/c/capture.json","status_host":"/srv/gdp-test/c/status.json","recording_host":"/srv/gdp-test/c/raw/ACTUAL_SESSION","binary_sha256":"TRUSTED_CAPTURE_SERVICE_SHA256"}
  ]
}
```

主机采样需要 Linux `/proc`、Docker metadata、状态文件读取权限，通常使用 sudo。工具检查运行中二进制摘要、镜像 revision、配置 SHA256、不同 serial、会话稳定性、无重启、状态不陈旧、IMU shared_ros_context=true。三容器必须已 ready，native recording 仍在进行。

## 正常长测

每种检查点模式先做三轮 1 小时，再做至少一轮 8 小时。轮次使用全新 generation/录制名和输出目录。8 小时示例将 sample 的 seconds 改为 28800、max-mib 改为 4096，观察器 seconds 改为 28950、max-mib 改为 32768；按这些时长再设置足够的 max_pairs 和真实磁盘容量。对照性能时按 A/B、B/A 交替顺序，固定型号、固件、SDK、USB 拓扑、图像格式、IMU 频率、磁盘、内存限制及缓存策略，不同时编译/验证其他大任务。

1. 启动三相机并确认 ready。立即启动 ROS 观察器；在与被测容器相同 ROS_DOMAIN_ID、RMW、Fast DDS 网络配置的 ROS Humble 环境运行，确认能够发现所有 publisher。Docker 观察器需正确挂载工具目录/证据目录并使用现有受控 DDS 网络配置；不要仅为连通而切换 RMW。下面示例为已配置的主机 ROS 环境：

```bash
umask 077
mkdir -m 700 /var/tmp/gdp-long-01
source /opt/ros/humble/setup.bash
# ROS_DOMAIN_ID、RMW_IMPLEMENTATION 等沿用被测部署环境。
python3 ros/imu_long_run_observer.py \
  --namespaces /camera_a /camera_b /camera_c \
  --seconds 3750 --max-mib 4096 \
  --output /var/tmp/gdp-long-01/wire.db
```

观察器在前台运行；另开终端继续。`.ready` 只表示订阅已创建，不证明 DDS 已匹配；实际结果以 `.summary.json` 和 graph 表为准。前 60 秒允许发现，之后缺少/重复 publisher 或 GID/QoS 改变即失败。

2. 等待约 30 秒暖机（此时录制仍应在进行），运行一小时采样：

```bash
sudo python3 scripts/validation/imu_long_test.py --plan /srv/gdp-test/plan.json sample \
  --seconds 3600 --interval 1 --max-mib 512 \
  --output /var/tmp/gdp-long-01/host
```

若需要磁盘延迟与 CPU profile，另外运行 checkpoint_probe.py，并将其 observer CPU/RSS 与本工具、ROS 观察器单列；不要计入 capture CPU。脚本在采样结束前发现录制已 complete 会失败，避免把空闲阶段混入录制性能。

3. 等待 ROS 观察器正常结束、三台原生录制达到 complete，再分别正常停止服务以保存 stopped 关键状态。不要提前停。保留原有配置、状态、日志、镜像 ID/可执行文件及 SDK 版本。先关闭观察器再停止服务，可避免把计划内 publisher 消失当成长测失败。

4. 对三个最终录制逐一审计。下面 SESSION_A/B/C 均须取自实际状态，审计器需有只读目录权限；重定向在当前 shell 下执行，注意文件属主权限。

```bash
set -euo pipefail
umask 077
target/release/examples/audit_imu_long_run /srv/gdp-test/a/raw/SESSION_A > /var/tmp/gdp-long-01/a.audit.jsonl
target/release/examples/audit_imu_long_run /srv/gdp-test/b/raw/SESSION_B > /var/tmp/gdp-long-01/b.audit.jsonl
target/release/examples/audit_imu_long_run /srv/gdp-test/c/raw/SESSION_C > /var/tmp/gdp-long-01/c.audit.jsonl
sudo python3 scripts/validation/imu_long_test.py --plan /srv/gdp-test/plan.json check \
  --sample-dir /var/tmp/gdp-long-01/host --wire /var/tmp/gdp-long-01/wire.db \
  --audit a=/var/tmp/gdp-long-01/a.audit.jsonl \
  --audit b=/var/tmp/gdp-long-01/b.audit.jsonl \
  --audit c=/var/tmp/gdp-long-01/c.audit.jsonl \
  --trim-seconds 30 --output /var/tmp/gdp-long-01/acceptance.json
```

每个审计输出只有在进程退出码 0 且末尾 summary 成功时有效。异常可能留下部分 JSONL，不能当作完成结果。`check` 默认严格验收源序号无缺口；返回 `integrity_ok=true, zero_source_gaps=false, ok=false` 表示读回/匹配通过但源序号有缺口，不能称为完全无丢样。

DDS/native 双向逐样本比对只覆盖共同时间范围裁去首尾各 30 秒的窗口，避开订阅发现、启动排除及录制收尾边界。原生全量的两次独立审计覆盖全部已录帧。边界不在 DDS 完整性证明内；若需边界也无遗漏，必须在启动采集前开启观察器并另做带会话屏障的验收，不能通过把 trim 改为 0 自动获得该保证。

### 哈希拒绝负向检查

从已经完整验收的录制做独立副本，只修改副本；不得修改原始证据。以下 Linux GNU cp 示例用 reflink/COW 或实际复制，不用硬链接。执行前确认路径均为本轮专用目录。

```bash
mkdir -m 700 /var/tmp/gdp-long-01/hash-negative
cp -a --reflink=auto /srv/gdp-test/a/raw/SESSION_A /var/tmp/gdp-long-01/hash-negative/copy
python3 - /var/tmp/gdp-long-01/hash-negative/copy/payload-00000000000000000001.bin <<'PYCODE'
import sys
from pathlib import Path
with Path(sys.argv[1]).open('r+b') as f:
    first = f.read(1)
    assert first
    f.seek(0)
    f.write(bytes([first[0] ^ 1]))
PYCODE
if target/release/examples/audit_imu_long_run /var/tmp/gdp-long-01/hash-negative/copy \
  > /var/tmp/gdp-long-01/hash-negative/rejected.jsonl \
  2> /var/tmp/gdp-long-01/hash-negative/error.log; then
  echo 'FAIL: corrupted copy was accepted' >&2
  exit 1
fi
```

必须确认失败原因是受影响记录的哈希/内容校验，而不是权限、路径、空间或命令不存在。保留 stderr 和副本变更记录；原始录制再次审计仍应通过。对任一完整 V3 目录，不能以重算索引哈希让损坏副本通过。

## 崩溃恢复轮次

这是独立轮次，不能复用正常长测的 complete 目录。三台仍并行采集；每轮只杀一台，依次轮换 a/b/c，每种 checkpoint_pipeline 模式各至少三次。至少等待 2 个检查点后注入，覆盖正常写入期和检查点读回/落盘期；用已有 checkpoint_probe 的 active span 观察择时，采样是快照，不宣称精确命中单个 syscall。先结束本轮正常观察/采样，再注入；SIGKILL 后这些正常长测工具因目标消失而失败是预期，不把该失败当成恢复结果。

复制 plan 为 `crash-plan.json`，被杀相机的 recording_host 指向实际 `raw_recording.partial_path` 映射到主机的 `.partial` 目录，其他身份/配置不变。确认 Docker 与服务自动重启均禁用。只执行精确命名的测试容器，不用通配符，不杀 USB 或主机进程。

```bash
sudo python3 scripts/validation/imu_long_test.py --plan /srv/gdp-test/crash-plan.json crash \
  --camera b --execute --output /var/tmp/gdp-crash-b/injection
sudo python3 scripts/validation/imu_long_test.py --plan /srv/gdp-test/crash-plan.json recover \
  --camera b --injection /var/tmp/gdp-crash-b/injection \
  --auditor "$PWD/target/release/examples/audit_imu_long_run" \
  --output /var/tmp/gdp-crash-b/recovery
```

`crash` 固定检查并使用完整 container ID，保存注入前状态、检查点、配置/镜像/二进制身份；只向该容器发 SIGKILL。`recover` 要求容器仍停止，没有 COMPLETE；最后检查点数量不得小于注入前检查点，恢复审计必须恰好读取最后检查点 record_count，两次真实读回成功，普通 complete 打开必须失败；检查点字节与 COMPLETE 状态不得被审计修改。未提交尾部可以被忽略，不补造数据。两个未被杀相机必须继续健康、无重启/溢出/时间戳错误，再按正常路径完成验收。

SIGKILL 验证的是进程崩溃，不是断电、内核崩溃或存储缓存断电持久性。若要声称断电恢复，必须另设受控电源/存储测试，保留控制器缓存和写入屏障配置。

## 验收标准

### 必须通过的正确性

- 被测身份均为固定 commit 的受信构建；三台真实 serial 与 namespace/配置绑定正确。IMU/global 能力已逐台验证。每次状态 config_sha256/session_id 稳定，零重启，最终 stopped/complete 状态实际落盘。
- 每个 IMU 传感器的 sequence、device、可用 global 严格递增；system 可相等、不可倒退。原值不改写，XYZ 有限、frame_id 正确、Global 不缺失。实际 DDS header 必须逐流严格递增，等于原生 global_timestamp_us×1000，数值和 frame_id 逐条一致。图像元数据的 global 映射、SDK 序号、device/system 与原生证据一致；图像载荷 SHA256 和尺寸/stride 一致。
- 正常观察窗口内四类数据双向匹配，无观察器漏收、重复或原生漏录。QoS 正确，八个发布端 GID 证明相机内共享、跨相机隔离，期间不变；仅状态字段 shared_ros_context=true 不足以单独验收。
- 每台原生 written_pairs/imu_samples_submitted 与完整审计数量一致；`validation_image_bytes == payload_bytes_submitted*2`，`validation_imu_samples == imu_samples_submitted*2`。这是生产 V3 检查点+最终读回，不用额外审计的两次代替。原生读取器对全量图像、索引、证据、快照及 IMU 进度的实际校验通过。V2 不属于本轮。
- 零 IMU 错误、发布/时间戳/单调性错误、原生队列溢出、未解释重启、观测期间 trace event 丢失。保留 startup_samples_excluded、imu_samples_outside_recording_excluded、pending、received/published 计数，解释启动和收尾边界；不把 total received、ROS published、native submitted 混作同一范围。
- 完全无缺口验收额外要求四类源 sequence gap 均为 0、每流有效速率接近配置值（建议长窗口 ±1%；先核实设备档位允差）。检查 max_interval_ns，30 FPS 超过 50 ms、200 Hz 超过 7.5 ms 必须逐次解释。SDK 周期性 66 ms 深度缺口仍算缺口，哪怕录制读回和哈希完全匹配。
- 崩溃每轮满足最后已确认检查点前缀保留、无伪 COMPLETE、完整读取入口拒绝 partial、恢复审计两遍成功；其他相机持续采集。保存恢复点外尾部数量/范围作为“未确认数据”，不声称完整。

### 性能

`host/summary.json` 提供每台 worker/supervisor 平均 CPU 核数、worker RSS/HWM 峰值，以及采样器自身 CPU/RSS；samples.jsonl 提供 supervisor、cgroup memory.current/memory.peak/memory.stat、IO、队列、检查点事件和累计 append 统计。wire.db.summary.json 单列 ROS 观察器 CPU/RSS。容器匿名内存、文件缓存、进程 RSS 和整个容器工作集不是同一指标。

最终 acceptance.json 每台提供 append P99 的 [lower_bound_ns, upper_bound_ns]，来自整段原生录制的累计直方图；不是 1 小时窗口的精确 P99，亦不是物理磁盘请求 P99。采样中的每个 P99 也是截至该时点的累计值，不能相减。相机间 P99 不相加、不平均替代总分位数。

先用相同监控负载测至少 3 个固定配置基线，分别保留有/无 DDS 哈希观察器的性能轮次，观察器本身会读取/哈希每张图像并写 SQLite，可能显著增加 CPU、DDS 背压和磁盘开销。无观察器轮次不能作为 DDS 完整性通过证据。建议预先约定：采集 worker+supervisor CPU 中位数增幅不超过 10%，追加 P99 上界增幅不超过 20%，并且其绝对值低于业务预算（例如 30 FPS 取 33.333 ms）；越界需定位后复测，不能在缺少基线时宣称性能通过。

IMU 队列高水位不得达到容量且溢出必须为零；图像/原生队列不得溢出。暖机后匿名内存不应持续无界增长；建议比较每 10 分钟匿名内存中位数并预先约定斜率和绝对预算，文件缓存增长单列。所有资源限制/CPU throttling/OOM 事件必须保存。若存储排队/fsync 尾延迟升高，用 checkpoint_probe + diskstats + perf 关联，不把容器总 CPU 或设备平均 await 当作追加 P99。

## 留存与复现

每轮保留 plan、生成配置、版本/固件/SDK 来源、USB 拓扑、镜像/二进制摘要、最终状态、私有日志、两个工具的摘要、SQLite、audit JSONL、acceptance/recovery 报告和 perf（若启用）。对这些**停止写入后的**文件另生成 SHA256 清单；生成的清单只能证明之后未变，不能代替从原生读取器验证采集时已写入的哈希。不要把私有资料提交到仓库。

工具回归：Python `tests/test_imu_long_test.py`；Rust `cargo test --locked -p offline-depth --test long_run_audit`；实际 DDS 合成 `ros/tests/smoke_long_run.py` 已加入标准 run_smokes.sh。实机执行成功之前，报告中 hardware_verified 必须保持未验证。
