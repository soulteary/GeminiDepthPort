# 三 writer 长时状态后台／内核追踪验证归档

基线 `80585eca829eae58e2c4ceae2932a88253c5271c`，日期 2026-11-06。本次仅增加测试适配器、控制器和采样工具；生产录制、状态持久化实现及既有 checkpoint-worker 候选补丁均未改变。入口、预算和验收要求见[运行文档](../../writer-long-run.md)。

**未完成可接受的 600 秒对照。** `long-pair-600` 与 `native-long-pair-600-02` 均计划 600 秒、三 writer、off/on，但 off 组分别在约 290 秒、14 秒队列溢出，随后停止；on 和正式恢复没有执行。没有性能收益、物理 SSD 根因或相机验收通过的结论。独立 32 对 off/on 崩溃恢复回归通过，不能替代正式长测恢复。

`validation.json` 记录结果、来源与限制；`metadata.tar.gz` 保存本轮各次尝试的结果、日志、状态样本、资源样本及连续归一化内核事件。首次短测中的适配器自锁、后续心跳超时和清理错误均保留。`EXCLUSIONS.json` 逐项列出被排除文件；原生载荷、`records.jsonl`、ELF 和 160 GiB 稀疏 ext4 镜像不提交。元数据导出的原始全量清单改名为 `ORIGINAL_FULL_RUN_SHA256SUMS.txt`，只供定位完整数据，不能对元数据子集运行它并宣称全量交付。

完整新数据留在 `/private/tmp/gdp-writer-long-20261106-gxshjhgp`；ext4 原始数据留在其中 `native-linux-160g-01.ext4`。外层 `SHA256SUMS` 校验本目录归档文件；解开 `metadata.tar.gz` 后以 `ARCHIVE_SHA256SUMS` 校验本次实际打包子集。旧失败证据不在此包内，2,376 文件的再次核验结果见 `validation.json`，未修改旧目录。

`sources.tar.gz` 保存实验时的两个源码快照和最终工具源码身份；早期实际输出按当时版本保留，未用最终汇总逻辑重算。早期 `native-smoke-01` 的 `trace_complete=true` 仅覆盖返回的 off 组，整体原本已失败；最终代码要求所有计划组均有完整追踪，新增真实子进程回归。

完整 Linux Rust coverage/workspace/外部回归、Python 全集、37 项工具测试、MSRV lint 与 release 子进程回归日志以 gzip 无损保存。覆盖率 JSON／LCOV 取自本轮 Linux 测试卷，宿主旧 `target` 报告未使用。新增回归涉及真实子进程与状态 I/O；tracefs 的能力缺失、丢失、超限与解析合同另有受控 fixture，并有本次真实内核成功采样作为补充。
