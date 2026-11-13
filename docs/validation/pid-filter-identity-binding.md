# 宿主观察器 PID filter 绑定修正

`61642b8` 实机报告中观察器身份绑定失败、零成功身份样本，原始追踪只覆盖启动早期；本修正不改变该轮失败、29 项原关联缺口或旧 SHA256。录制溢出根因仍未关闭，缺失的失败窗口不能补采。

## 故障机制与修正

旧路径使用 `Path.open("a")` 打开 tracefs 的 set_event_pid。CPython 的追加打开会尝试 SEEK_END；当控制文件以 EINVAL 拒绝该定位时，错误发生在实际 TID 写入前。Linux 受控测试用真实 CPython 和 LD_PRELOAD 模拟这一控制文件契约，复现旧路径的 lseek_end／EINVAL，验证修复路径不执行该定位。

模型使用临时普通文件，并模拟 tracefs 的添加 PID 语义；它不是 MiniPC 原 Ubuntu 内核上的运行时复现，不能据此倒推历史错误的精确 syscall 已得到证明。原机调用栈／系统调用证据仍须由测试者补充。

实现依据可对照 [CPython 3.12.3 的追加打开](https://github.com/python/cpython/blob/v3.12.3/Modules/_io/fileio.c#L452)及 [Linux v7.0 的 PID filter 打开／写入](https://github.com/torvalds/linux/blob/v7.0/kernel/trace/trace_events.c#L2681)。上游代码在 O_TRUNC 时清空过滤项，写入路径使用已有 PID 集合；这不替代实际 Ubuntu 补丁集验证。

新路径只对私有实例的既有控制文件执行 `os.open(O_WRONLY|O_CLOEXEC|O_NOFOLLOW)`、一次 `os.write` 和 close，不使用 O_APPEND、O_TRUNC 或 O_CREAT，不清空此前过滤项、不创建缺失控件。短写视为失败，不跨 PID token 边界重试；EIO、ENOENT、符号链接等失败保留。

pid-filter-seeded 回执包含 syscall_receipt。身份错误行包含 syscall_context：操作阶段、路径、flags、requested／written bytes、errno 及来源、前 64 个请求 TID、完整 payload SHA256、observer PID/TID 和单调时间。短写的 EIO 是观察器校验错误，errno_source=observer_validation，不能当作内核返回 EIO。文件及身份资料仍应私有交付。

只有控制写入成功、worker start_ticks 再次复核并记录回执后，才发布 known_threads、thread_pins、bound worker。快照结束时间位于绑定之后，不能将 proc 身份向前回填到尚未绑定的时间。原 NSpid、namespace、cgroup、实例／配置校验及线程代际规则保持严格。

## 完整性判定

采集收尾新增 identity_binding_complete 和 raw_counters_and_removal_complete，分别表达身份绑定与原始计数／实例移除结果。没有成功身份样本、缺少任一相机绑定、绑定错误或未完成 seed 的 worker，都不能获得 capture_complete=true。该结论仍不等于离线关联或硬件验收通过。

离线 overall_identity_history_complete 不再由空集合得到 true：必须有正数成功样本、零身份错误、三个相机的预期 worker 均已实际锚定且登记在 bound_workers 中。缺失或无效计数按不可用处理。无身份绑定会显式加入 identity-history-incomplete，使 association_complete=false。

全历史 generation 缺口和未知 birth 仍影响全历史标志；对窗口外缺口继续保留原来的分窗规则，不将 unrelated 的窗口外缺口自动升级成窗口内失败。PID 重用或未证明身份不能静默接受。

## 回归与验证边界

```sh
python3 -m unittest discover -s tests -p test_pid_filter_binding.py -v
python3 -m unittest discover -s tests -p test_host_observation.py -v
```

Linux 专属受控 syscall 模型验证旧追加路径失败、新写入保留此前 PID，以及 EIO／短写上下文；另覆盖缺失控件、无效 TID、符号链接和失败不发布身份。

生命周期回归以构造的 proc/runtime 与私有实例替身驱动真实 sample_identities／finish，覆盖 recording、draining、readback 三阶段保持 armed、收尾后停止与生成可校验清单。它不访问 USB、Docker 或真实 tracefs。真实分析器端到端回归使用完整 SHA 清单的零身份输入，不 mock 汇总函数，确认输出为 incomplete 且输入不变。现有 wrapper 故障回归继续保证追踪失败只设置停止标记，当前 driver 正常结束后才收尾，不提前杀 writer。

在新的私有派生目录重读实际 `61642b8` 证据，零身份结果变为 overall_identity_history_complete=false、association_complete=false；原 29 项关联缺口保留，并新增身份缺失缺口，输入文件哈希不变。真实包、身份资料及派生原始输出未提交到公共仓库。

## 测试者复核

使用本修正提交的干净工具源码、新输出目录和固定构建来源，在原机执行原有生命周期方案；旧失败数据和日志原样保留。先检查每路 pid-filter-seeded、identity-sample 与 syscall_receipt；任何失败都回传原始 identities.jsonl、ready／summary、停止标记和 errno 上下文，不跳过身份校验继续计为完整。

正常运行时需核对成功绑定覆盖三相机、原始 trace 时间覆盖 recording、draining、两轮完整读回及 driver 收尾、周期丢失计数连续、最终排空和实例移除。失败时保留当前组 writer 收尾结果并停止后续组。回传原始追踪、身份／统计／资源流、driver 结果、离线关联缺口、完整性结果和 SHA256；缺失窗口不能插补。

此次仅修观察器与分析器，未改队列上限、心跳期限、同步屏障、两轮读回、逐帧哈希或崩溃恢复，也不把合成回归当作实机复测。正式验收仍由测试者完成。
