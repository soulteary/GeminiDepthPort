# 6c2c25a 覆盖率 CI 跟进

首轮 CI：data-plane 3 项和 offline 6 项通过；coverage run 42365177980 / job 128381620607 失败。保留该失败，不将后续成功重写为首轮通过。

两个 checkpoint/status fsync 故障注入用例在 LLVM 插桩的共享 runner 上先触发了非目标 IMU 批次或 SDK 队列溢出。仅将这两个夹具的 accel/gyro profile 改为各 50 Hz，保持真实 mock 回调与时间戳、持续发布断言、原有故障屏障和精确溢出上下文断言。它们验证持久化故障隔离，不用于证明 200 Hz 吞吐；独立 normal-rate 等待测试仍为各 200 Hz。

生产队列、心跳期限、同步屏障、两轮读回、哈希及恢复逻辑未改。此次跟进也不解决正式相机持续录制的 RAW_QUEUE_OVERFLOW，现场验收仍未通过。

本地 Linux / Rust 1.90 / LLVM 插桩验证：目标 3 项通过；完整 service_supervision 72 项通过、5 项显式实验忽略；格式检查通过。日志与首轮失败日志压缩归档，SHA256SUMS 校验压缩文件本身。后续远端 CI 以新提交对应 Actions 为准。
