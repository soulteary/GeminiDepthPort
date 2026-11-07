# 管理服务开发参考

`geminidepthport` 是管理 CLI/Web 入口。管理镜像包含 Rust 二进制、Docker CLI/Compose
和部署 shell；ROS 健康与标定工具在采集容器执行，宿主机不需 Python 或 ROS。
操作命令与保护规则见[生命周期](../guides/lifecycle.md)，配置字段见[配置](../guides/configuration.md)。

## 模块

- `src/config.rs`：schema 校验、确认摘要、配置变更失效和纯迁移预览。
- `src/generation.rs`：配置生成、产物清单与当前版本指针。
- `src/control.rs`：操作锁、设备发现、容器归属和受检生命周期。
- `src/server.rs`：本机 Web、请求保护、任务和预览。
- `src/calibration.rs`、`src/capture_snapshot.rs`：标定及原生快照读取。
- `src/recording.rs`、`src/managed_quality.rs`、`src/quality_history.rs`：保护与质量查询。
- `src/util.rs`：限制权限的原子写入、摘要和有界子进程。

## 本地运行

```sh
cargo build --locked --release -p geminidepthport
./target/release/geminidepthport --root "$PWD" validate --draft
./target/release/geminidepthport --root "$PWD" serve --port 8765
./target/release/geminidepthport --help
```

未指定 `--root` 时使用 `ORBBEC_DEPLOY_DIR`，否则使用当前目录。不要为同一部署同时
运行宿主机版和容器版 Web。开发检查见[贡献指南](../../CONTRIBUTING.md)。

## 持久化与并发约束

`state/web.lock` 保证单一 Web 管理者；变更使用 `state/control.lock` 非阻塞排他锁。
配置写入在原子替换前复核读取 revision，外部编辑导致 `INVENTORY_DRIFT` 时保留外部内容。
这不提供对不遵守文件锁的编辑器的文件系统级 CAS；直接编辑仍应在维护窗口进行。

配置与 generation 使用同文件系统临时文件、fsync 和原子替换。激活失败时保留可能
已被当前指针引用的版本，不为清理而制造悬空指针。序列化摘要兼容既有配置；原始
inventory 字节改变后需重新生成。

预览容器使用配置创建者 UID/GID，HOME/ROS_HOME 位于 `/tmp`。确认绑定当前配置、
镜像产物及固定容器实例，执行探针前续租。后台任务和内部命令有独立进程组及截止时间，
超时或关闭时清理所属后代。进程组约束见[清理契约](../contracts/worker-process-group-cleanup.md)。

`tests/legacy_reference/` 仅为合成回归的行为参考，不进入生产管理镜像。
[Linux 合成测试](linux-management-fixture.md)不访问真实设备或 Docker daemon；
它与真实硬件证据的区别见[验证索引](../validation/README.md)。

## Web 型号与后端选择

`GET /api/state` 的 `configuration_options` 来自当前型号目录，包含型号／后端限制、Rust 参数允许项、默认镜像标签及首次切换的兼容 Rust 策略。UI 不推断实际支持的流档位，也不自动选择扫描到的第一台设备。

`POST /api/config` 接受 `revision`、全量 `cameras` 更新及可选的 `capture_backend`、`image`、`acquisition`、`discard_unsupported_parameters`。相机更新可指定 `profile`；仍不接受 launch 路径、确认结果或任意 Rust 服务策略。未知字段、格式或后端组合拒绝保存。

切换后端必须显式提交镜像标签；型号、后端或镜像变化要求正式相机和所有预览均已停止。移除旧配置中不兼容的参数需要显式布尔确认，新提交参数仍经过校验。型号变更时即使保留同一序列号，也重新检查检测到的型号。后端、镜像或共享采集策略变化清除全部确认；仅改型号清除该槽位确认。HTTP 变更仍先获取 mutation lock 并检查录制保护，保存前复核 revision。

首次从官方切换 Rust 初始化兼容策略；编辑已存在的 Rust 配置不会重置心跳或其他服务预算。保存只更新 inventory，不构建镜像、不切换运行实例，也不自动启动相机。实际运行仍需新的镜像校验、档位确认和 generation。用户操作见 [Web 指南](../guides/web.md#切换型号或后端)。
