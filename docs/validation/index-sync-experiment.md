# 索引同步隔离实验

目的：基于 `61642b8` 对照索引 `sync_all` 与 `sync_data`，不改变生产默认路径。实验源码由指定提交的 Git archive 导出，候选副本仅替换 `records.sync_all()` 及诊断调用名称；payload、目录、checkpoint 提交、两轮实际读回、逐帧哈希和恢复协议不变。构建遇到源码契约变化会拒绝继续。

两个启动器都固定 half-credit，保留 32 对／16 MiB 未确认额度和 16 对 FIFO。不存在自动 AE 调整或相机配置修改。本工具使用已有合成 writer，未运行 capture-service，因此不验证心跳、SDK、ROS 或状态持久化后台负载；生产这些路径均未修改。

## Linux 构建和故障回归

在项目根目录执行，需要 Git、Rust/Cargo、C 编译器和 Python 3。所有输出目录必须不存在，输出盘不能指向旧证据目录。构建不接受工作区未提交内容；默认固定基线提交。

```sh
python3 scripts/validation/index_sync_ab.py \
  --revision 61642b8 --output /srv/sdk-test/index-sync-build-01
python3 scripts/validation/index_sync_faults.py \
  --build /srv/sdk-test/index-sync-build-01 \
  --output /srv/sdk-test/index-sync-faults-01
```

故障矩阵对两个版本的第二次索引同步分别注入 100 ms 延迟和 EIO。延迟之后仍执行真实同步，检查完整读回及原有额度；EIO 必须传播为失败且不能发布 COMPLETE。随后在无注入环境用恢复读取器对此前 checkpoint 前缀执行两轮读取、逐帧图像／证据哈希和 IMU 比较，恢复前后文件 SHA256 必须相同。注入标记必须证明实际命中了 fsync 或 fdatasync，不能将未触发注入视为通过。

## 同盘性能对照

以下为测试者执行的至少 600 秒对照，不在开发机代替实机运行：

```sh
python3 scripts/validation/writer_contention_ab.py \
  --before /srv/sdk-test/index-sync-build-01/sync_all.bin \
  --after /srv/sdk-test/index-sync-build-01/sync_data.bin \
  --output /srv/sdk-test/index-sync-600-01 \
  --seconds 600 --repeats 3 \
  --max-output-bytes 824633720832 --max-wall-seconds 24000
```

复用工具交替执行单 writer 与三 writer、两个版本和三轮重复，含独立 SIGKILL 前缀恢复。600 秒是每个 writer 的采集时长，整套运行远长于 600 秒。该矩阵会保留大量原始数据，启动前检查计划容量和普通可用空间；上述 768 GiB 是硬配置上限，不是实际空间承诺。容量不足应停止并回传预检结果，不能清理旧证据或缩短时长后声称正式通过。可先用 `--seconds 2`、新目录和 4 GiB 上限验证工具，不能作为长测结论。

回传 build-manifest.json、两份构建日志、故障 result.json 和子日志、性能 manifest/summary、各组 JSON、资源采样、两版 recovery 目录及 SHA256SUMS。两个启动器依赖旁边的 .elf 文件，不能只复制启动器；构建清单同时记录启动器和 ELF 的 SHA256。

评估 CPU、追加 P99／最大值、验证耗时、RSS、吞吐、队列峰值、额度峰值及完整性结果。若只有均值改善或失败推迟，不视为解决 RAW_QUEUE_OVERFLOW。保持原盘、相同背景负载和观察器配置；本工具无文件系统／块层追踪，不能据此宣称 jbd2 或 SSD 根因。追踪实验另行使用现有有界观察器，并比较观察器开销。

`sync_data` 在 Linux 上仍需同步恢复读取必需的文件长度等元数据，可能与 `sync_all` 等待同一日志提交；不能保证性能收益。SIGKILL 仅验证进程崩溃，不代表物理掉电。候选无论结果如何都不自动合入生产。

## 本地工具验证（2026-11-13）

Linux Docker VM 中每个 writer 2 秒、60 对，交替三轮，共 12 组单／三 writer，全部完成；两个版本的独立 SIGKILL 恢复通过。四项索引故障用例通过，其中两项 EIO 的已确认前缀均完成两轮只读恢复，恢复前后 SHA256 不变。

三 writer 的九个进程结果中，进程追加 P99 的中位数从 17.930 ms 到 14.075 ms；进程 CPU 秒中位数从 0.3845 到 0.3920；验证耗时中位数从 186.082 ms 到 187.657 ms；三进程采样 RSS 峰值的轮次中位数从 16.383 到 16.363 MiB。每进程仅 60 次追加，P99 等于该进程最大值，不能外推长尾分布。CPU 包含合成输入、哈希及额外读取器。

这不是稳定收益证据，更没有复现此前约 383 ms 的 index_sync 长尾。保留生产 sync_all，等待测试者的原盘长对照。摘要、构建清单、故障结果和 SHA256 归档于 [本轮结果](reports/index-sync-ab-20261113/performance-summary.json)。清单中的 `/work` 是本地容器路径，二进制不能跨架构复用，测试者应重新构建。
