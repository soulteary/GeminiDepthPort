# 故障排查

先保留 `status`、健康检查和私有日志，再按故障边界定位。录制保护生效时不要直接
删除保护文件、重建容器或修改配置；先确认录制结束并通过凭据释放。

## 无法启动

- 未绑定、未启用、缺少档位或确认失效：返回 Web 完成配置及实际预览确认。
- `CONTROL_BUSY`：另一个受管操作持锁，等待其结束后重试。
- `INVENTORY_DRIFT`：重新读取配置；维护窗口内保存、确认并重新生成。
- `IMAGE_CONTRACT_UNSUPPORTED`：检查后端、镜像来源和运行清单，重新构建后确认档位。
- 设备忙：检查其他采集程序是否持有设备；消费探针只应订阅，不能另开 SDK。

## 部分相机失败

启动按相机顺序推进，失败保留已启动实例，整组不会就绪。检查目标设备权限、USB
连接速度、档位和日志，不要把部分成功当作正式就绪。SDK 的 `UVC_ERROR_NO_MEM`
可能与宿主机 USBFS 缓冲上限有关，见 [USBFS 指南](usbfs.md)。

## DDS 图可见但无图像

核对 domain、RMW、QoS 和实际 Image/CameraInfo 端点。Rust 后端与消费者必须同时
使用文档规定的 `ROS_LOCALHOST_ONLY=0` 和仅允许 loopback 的 Fast DDS XML；仅改变
环境变量不够。详见[通信设置](consumer-integration.md)。

多发布器静态 TF 需使用能保留全部样本的订阅历史；按[消费验收](../validation/consumer-acceptance.md)
检查迟订阅和变换数值。不要只用节点发现或小消息成功来判断图像交付。

## worker 退出与恢复

Rust 后端有界监督 SDK worker。实际记录中，异常退出后首轮 SDK 重开可能不出帧，
多次有限重试才能恢复；不能保证一次重启成功。保护期间不得自动重开。
私有状态目录中的 `failure-<session>.json` 可保留 SDK 原始错误，不能公开上传。
达到预算仍失败时保持故障状态，在录制结束后的维护窗口排查设备与服务。

## 原生录制队列溢出

`RAW_QUEUE_OVERFLOW` 表示原生整对写入队列已满，任务失败；已排入队列或已追加的数量不能当作完成数量。
它与 `IMU_QUEUE_OVERFLOW`、`RAW_IMU_QUEUE_OVERFLOW` 是不同指标，排查时保留具体错误码。

1. 保存首次 failure 文件、最终状态、队列深度／最老队龄、active append／checkpoint 阶段及系统日志。
2. 将相同单调时间窗口的磁盘延迟、CPU／调度和状态持久化等待关联起来。先定位阻塞阶段，不能仅凭一次 fsync 长尾认定 SSD 故障。
3. 等任务和清理结束后，保留原 `.partial` 和 checkpoint，按[恢复步骤](recording.md#失败后的前缀恢复)读取已确认前缀。
4. 在新目录使用相同负载复测，保持队列、心跳、同步和读回要求。最近的软件修正尚未关闭正式持续录制溢出，见[当前限制](current-status.md#持续录制尚未完成正式验收)。

不要覆盖失败目录、清空错误计数，或把恢复前缀改标为完整录制。当前验证工具见[验收入口](../validation/README.md#录制与恢复验收入口)。

## 离线转换失败

`D2C_OUTPUT_OVERFLOW` 表示变换后的毫米值超出 u16；按契约整帧失败，不能截断或
改零取得成功。`.partial`、`FAILED` 或 `INTERRUPTED` 产物不能作为完整录制。
几何与输出语义见[离线工具](../../crates/offline-depth/README.md)。

更多已知限制与失败证据见[验证索引](../validation/README.md)。
