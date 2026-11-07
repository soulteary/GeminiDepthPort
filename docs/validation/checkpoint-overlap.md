# checkpoint 与追加重叠修正

基线 `83c06d878fd74cdb41e65224a607d4bbe7eabccb`。本次只处理 writer 的两处不必要反压，不把合成通过等同于正式三相机长测通过；旧失败证据保持原字节。

## 修改及不变量

半额度封块目标用于及时派发后台 checkpoint。原实现遇到前一 checkpoint 未完成时，在这个目标处立即等待，可能尚未用完全局额度。现在保留唯一 active chunk，在既有总额度内继续追加；每对入场前仍检查 pending + active 的 32 对／16 MiB 上限。额度耗尽、显式 checkpoint、finish 和析构仍等待原任务；不启动第二个并行 checkpoint，不增加原生队列的 16 个槽位或单个 in-flight 槽位。单对不可分割超大帧沿用原有例外。

原 64 KiB 索引缓冲可能在后台同步 `records.jsonl` 的同时由前台刷入后续索引。现在仅在存在 pending checkpoint 时暂存后续记录，最多 32 个既有帧额度 × 每行 64 KiB = 2 MiB；该空间只保存索引元数据，不保存额外帧载荷。没有 pending 时仍按 64 KiB 刷入。缓冲按需增长并复用，实际峰值及容量峰值分别报告为 `index_buffer_peak_bytes`、`index_buffer_capacity_peak_bytes`；`pipelined_index_buffer_limit_bytes` 是硬上限。该改动允许每 writer 多保留至多约 2 MiB 元数据，必须结合实机 RSS 验收，不能称为零内存代价。

封块前仍先刷入该前缀的全部索引，再依次完成 payload sync、index sync、directory sync、实际读回与逐帧哈希、checkpoint 文件 sync、rename 和 directory sync。结束时第二轮完整读回与最终提交守卫未改变。缓冲中的活动尾部没有被 checkpoint 确认；崩溃恢复仍仅接受持久化 checkpoint 指定的前缀。文件格式与时间戳校验不变，心跳期限未调整。

完整额度候选仍需显式配置，未提升为默认策略。半额度策略的实际 chunk 边界在 checkpoint 忙时允许变化；在比较观察开关是否改变文件字节的测试中使用显式边界，不能要求不同调度下自动封块布局完全相同。

## 回归与性能验收

故障矩阵分别覆盖 pair credit、byte credit 和较大 IMU 索引元数据，注入慢 payload/index/directory sync、读回、checkpoint write/rename、错误、损坏及 SIGKILL。阻塞期间核对活动追加、全局额度、索引文件长度不变和已确认恢复前缀；释放后核对最终数据与两轮读回。真正超出有限缓冲能力的停顿仍必须 `RAW_QUEUE_OVERFLOW`，不得静默丢帧或等待放宽心跳。

现场继续使用[已有长测脚本](writer-long-run.md)，固定新镜像与 revision，使用全新输出目录。先做不少于 600 秒的三 writer 同盘状态 on/off 对照及独立恢复，再做三相机六组和独立相机恢复。请求 `half-credit` 保持生产默认；记录新增缓冲峰值、原始 append P99、各同步阶段、CPU、RSS 和恢复结果。任何失败都保留原始原因和样本，不能用截短组的低 CPU 比较收益。

本地结果和限制见 `reports/checkpoint-overlap-20261107/`。合成短对照不能证明 MiniPC 的持续录制溢出已关闭，也不能将所有 ext4/jbd2 等待归因于 SSD。

实机三 writer 默认策略对照示例（每个 revision 使用不同目录，失败停止，不在旧 root 上续跑）：

```bash
sudo bash scripts/validation/run_writer_long_test.sh \
  --binary /absolute/path/to/NEW-release-driver \
  --output /srv/sdk-test/NEW-checkpoint-overlap-half-credit-600 \
  --checkpoint-batching half-credit --seconds 600 --repeats 3 \
  --disk nvme0n1 --max-output-bytes 549755813888 --max-wall-seconds 14400
```

二进制按 [writer 长测说明](writer-long-run.md)构建；它是包含测试适配器的 release lib-test driver，不是 `capture-service` 可执行程序，也不是 `service_supervision` integration-test driver。控制器必须先核对目标 ignored child 存在，不能把“运行 0 个测试”当作实验通过。两个 revision 的 common driver、fixture 与状态适配器来源必须一致。
