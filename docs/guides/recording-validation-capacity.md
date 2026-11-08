# 用有限磁盘空间执行录制验收

六组 600 秒测试的容量要求来自验收工具的保留方式，不是采集服务的运行门槛。默认 `all` 模式保留六组正常录制和独立恢复数据；可显式选择 `verified-window`，在逐组完成验收后释放成功组的像素载荷及 DDS 哈希数据库，保留配置、原生索引、哈希、审计结果和删除回执。

两种模式都执行相同的采集时长、帧对目标、Global/IMU/DDS 匹配、源缺口规则、两轮独立读回及逐帧哈希。滚动模式改变的是事后原始数据保留范围：被释放的成功组不能再完整重放。默认行为不变，失败组、未完成组、独立恢复目录和旧测试根均不清理。

## 先预演容量

`run_retest.py capacity` 不使用 Docker、SDK、管理员权限，不创建输出目录或改动配置。它按目标目录最近已存在的父目录所在文件系统读取普通用户可用空间，输出估算、总容量、可用量和缺口。正式执行仍重新检查目标文件系统，预演不预留磁盘空间。

在仓库根目录按现场路径填写：

```sh
diagnostics_plan='/absolute/private/three-camera-plan.json'
diagnostics_root='/srv/sdk-test/new-verified-window-run'
diagnostics_image='local/gdp-sdk-diagnostics:tested-version'
python3 experiments/sdk-2.10.6-diagnostics/run_retest.py capacity \
  --plan "$diagnostics_plan" --image "$diagnostics_image" --root "$diagnostics_root" \
  --rounds 3 --first on --seconds 600 --max-bytes 137438953472 \
  --ros-domain-id 78 --source-gap-policy reject \
  --checkpoint-batching half-credit --retention verified-window
```

预演不足返回非零退出码，但仍输出预算明细；`capacity_exceeds_filesystem=true` 表示需求超过整卷，单纯清理无法解决。`--max-bytes` 是每台录制的字节限额，不用于压低按尺寸、帧对数和元数据计算的测试容量。

以三台彩色 640×480、深度 640×400、30 FPS、每台每组 31,680 对和六组为例：

- `all`：估算 **1,056,712,228,864 字节**。
- `verified-window`：估算 **256,498,663,424 字节**，约减少 75.7%。

滚动估算保留一整组活动／恢复录制、DDS 数据库及回滚日志，以及六组的索引／IMU 元数据、每组三份有界审计和诊断余量，再加 25% 与 1 GiB。它包含保留元数据的增长，不假设空间恒等于一份像素数据。两种估算均不是文件系统配额，也不防止其他程序并发占盘。

## 执行滚动验收

预演通过、维护环境及镜像就绪后，把上面的 `capacity` 换为 `run`，添加现场整盘名称、恢复相机选择和 `--execute`。例如 `--disk nvme0n1 --camera b --execute`；盘名和相机标识必须与实际计划一致。

执行使用全新根，不能复用旧失败根。每一正常组依次完成：

1. 实际采集、两轮独立审计、哈希和 DDS 对照。
2. 生成真实汇总并核对组间配置一致。
3. 核对该组全部受管采集容器已停止。
4. 再检查三台均有完整目标的 V3 完成目录及成功审计。
5. 记录待释放文件的 SHA256、大小和文件身份，可靠写入 `payload-retention.json` 的 `prepared` 回执。
6. 复核文件身份，仅删除明确列出的 `payload-*.bin` 和已完成对照的 `wire.sqlite3`，同步目录，最后可靠发布 `released` 回执。

索引、快照、完成清单、审计输出、状态、日志和回执均保留；停止其他服务或释放录制保护不属于保留策略。符号链接、硬链接、活跃数据库日志、partial、失败／未完整审计组、身份变化及旧回执都会拒绝释放。释放中断时保持 `prepared`，整轮失败，不自动续删。

六组正常验收后仍执行独立 SIGKILL 恢复。恢复目录不释放，不能以一次成功组的回执代替恢复结果。包和总结果明确标注 `retention_policy`；滚动包会警告原始重放不可用。`COMPLETE` 说明当时完成过录制，若存在释放回执，不能据此认为原始像素仍完整存在。

## 三 writer 长尾诊断

长测资源采样独立于控制循环，覆盖启动屏障、正常写入、错误清理及 trace 最终排空。最终行标记 `final=true`，失败侧车也保存资源回执；已退出进程的 RSS 不补成有效数值。资源采样与 trace 的完整性分别判断，不能把有最后一行解释成整个窗口没有采样间隙。

`RAW_QUEUE_OVERFLOW` 的排查仍需在原盘比较 `half-credit` 和 `full-credit`，分别使用新根，并显式保持相同 FIFO、全局未确认额度、时长和状态负载。已有长测入口接受 `--checkpoint-batching`，例如：

```sh
sudo bash scripts/validation/run_writer_long_test.sh \
  --binary /absolute/capture-probe-writer-long-driver \
  --output /srv/sdk-test/new-half-credit-run \
  --seconds 600 --repeats 3 --disk nvme0n1 \
  --max-output-bytes 549755813888 --max-wall-seconds 14400 \
  --trace-max-bytes 4294967296 --trace-buffer-kib 256 \
  --checkpoint-batching half-credit
```

另一独立实验改用新输出根及 `--checkpoint-batching full-credit`。上述合成长测仍完整保留原始数据，不继承相机验收的滚动释放选项；先核对其自身预算。任一实验失败保留该根，不在同一根续跑。完整组数、状态 off/on 对照、读回、哈希和恢复必须分别报告，不能只比较失败前 P99。

此工具修正没有证明原盘持续录制溢出已经消除，也没有修改生产默认批次策略。任意长的磁盘停顿仍可能超过有界 FIFO 的承受范围；不能通过放大队列、跳过同步、丢帧或放宽心跳来取得“通过”。实机接受条件继续见[录制验收入口](../validation/README.md#录制与恢复验收入口)。

按设备覆盖内核线程的新追踪入口及其独立预算见[文件系统与块层归因](storage-attribution.md)。
