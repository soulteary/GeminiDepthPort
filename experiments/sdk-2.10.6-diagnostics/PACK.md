# 一键打包三相机复测证据

`pack_results.py` 离线、只读地收集 `run_retest.py` 的输出目录，流式生成 `.tar.gz` 和外部 `.sha256` 文件，不调用 Docker、不启动/停止相机、不上传数据，也不修改源配置或录制。正常和失败实验均可打包；打包成功不等于实机验收通过。

## 运行

先等复测控制器结束。如实验中断，先执行 [RETEST.md](RETEST.md) 的回滚并等控制器退出，再打包。统一使用复测的 root UID/GID，源目录必须保持私有权限。输出放在源目录之外且使用新文件名：

```bash
sudo install -d -m 700 /srv/sdk-test/exports

sudo python3 experiments/sdk-2.10.6-diagnostics/pack_results.py \
  --root /srv/sdk-test/3dff4fd-retest-01 \
  --output /srv/sdk-test/exports/gdp-retest-01.tar.gz
```

脚本输出 JSON，包含 archive、外部 SHA256、文件数、原始证据字节数及 warnings。已有压缩包或同名 `.sha256` 会被拒绝，不覆盖、不删除旧包。源文件/目录在读取过程中变化会使打包失败并删除本次临时包，请待实验完全停止后用新输出路径重试。

## 收集和排除

保留原目录层级，归入包内 `gdp-retest-evidence/retest/`：

- 根 result/summary/manifest、冻结的三机 capture/health 计划及配置摘要。
- 各组生成的配置、runtime/SDK/bridge/采集与校验器 SHA256 清单、运行身份和镜像 ID；构建日志及已有镜像摘要文件。不会导出镜像本身或查询缺失的镜像信息。
- on/off 性能 JSONL、CPU/内存/追加 P99/验证耗时汇总、SDK 配对/队列/锁等待采样、观察器摘要及日志。
- 组目录内独立 writer-tails 观察器的 JSON/JSONL、身份 manifest、摘要及原始 `SHA256SUMS`；须先等该观察器结束。原始清单按字节保留，也受外层包的 SHA256 保护。可选 perf 二进制仍排除，需另行保留和回传，不能因证据包生成成功便删除它。
- 独立 SIGKILL 注入、前后状态、恢复 reader/hash 验收与 stderr、失败日志、回滚记录、控制器子进程身份。
- 原生 recordings 树只定点读取小型 checkpoint/snapshot/manifest/COMPLETE/INTERRUPTED 元数据，不遍历载荷或逐帧文件树。

排除原生载荷、`records.jsonl` 原始索引/IMU记录、DDS `wire.sqlite3` 及 WAL/SHM、rosbag/MCAP、SDK/镜像二进制、原始图像和 `perf.data` 等非文本文件。保留对应 JSON 摘要、索引哈希及采样日志，因此本包用于分析和复核验收证据，不能独立重新执行原始像素读回。原始录制留在实机。

另自动收集 `/dev/shm/gdp-retest-{run_token}` 中的私有 SDK 恢复诊断文件，归入 `sdk-recovery/`；路径由已保存的32位 run token推导，不读取配置中任意外部路径。目录不存在时明确记录 warning（可能恢复阶段尚未启动或重启后 tmpfs 丢失），不伪造诊断结果。请在重启前打包。

默认单文件上限256 MiB、证据总量上限2048 MiB、最多20,000个源文件。超大单文件跳过并记入 warnings/EXCLUDED；总量或文件数超限则失败，不悄悄截断采样。需要时显式提高限额，例如：

```bash
sudo python3 experiments/sdk-2.10.6-diagnostics/pack_results.py \
  --root /srv/sdk-test/3dff4fd-retest-01 \
  --output /srv/sdk-test/exports/gdp-retest-01-expanded.tar.gz \
  --max-file-mib 512 --max-total-mib 4096 --max-files 30000
```

不跟随符号链接，不读取 FIFO/socket/device；这些情况记录为排除项及 warning。文件名控制字符、非法相对路径、源所有者不一致、读取变化及输出冲突拒绝打包。包中只有普通文件，不产生符号链接或绝对路径成员。源配置中的序列号、路径、运行身份及日志原样保留；没有自动上传步骤。

## SHA256 校验与内容查看

先校验压缩包，再解压到新目录并校验包内清单：

```bash
sudo bash -c 'cd /srv/sdk-test/exports && sha256sum -c gdp-retest-01.tar.gz.sha256'
sudo tar -tzf /srv/sdk-test/exports/gdp-retest-01.tar.gz

sudo install -d -m 700 /srv/sdk-test/unpacked-retest-01
sudo tar -xzf /srv/sdk-test/exports/gdp-retest-01.tar.gz \
  -C /srv/sdk-test/unpacked-retest-01

sudo bash -c 'cd /srv/sdk-test/unpacked-retest-01/gdp-retest-evidence && sha256sum -c SHA256SUMS'
sudo cat /srv/sdk-test/unpacked-retest-01/gdp-retest-evidence/PACKAGE.json
sudo cat /srv/sdk-test/unpacked-retest-01/gdp-retest-evidence/EXCLUDED.json
```

包内 `SOURCE_MANIFEST.json` 列出每个收集文件的来源、相对成员名、大小和 SHA256；`EXCLUDED.json` 列出原始数据树及其他排除项；`PACKAGE.json` 保存运行/镜像摘要、原始 run_ok、限额和缺失警告。`SHA256SUMS` 覆盖所有收集文件及这三个生成的 JSON，自身不自哈希；外部 sidecar 覆盖最终压缩包。SHA256用于校验传输完整性，不是数字签名。

## 验证

2026-11-01：新增9项专项回归，覆盖真实CLI、配置/采样/镜像/恢复/失败日志收集、包内及外部SHA256、raw/recordings相机名称、原始数据排除、缺失SDK tmpfs与超大证据标注、符号链接/FIFO、源变化、所有者/私有目录、预算及并发输出冲突。隔离Linux完整Python回归420项，1项既有条件跳过，其余通过。本次未修改Rust采集/录制、生产SDK或默认配置；测试使用合成证据，没有新增实机验收结论。
