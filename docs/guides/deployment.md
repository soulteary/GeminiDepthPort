# 部署与运维

## 运行条件

使用原生 Linux、rootful Docker Engine、本机 `/var/run/docker.sock`、Compose 插件、
udev、systemd 和 flock。宿主机不需要 ROS；远程与 rootless Docker context 不受支持。
管理容器通过 Docker socket 管理主机容器，只允许可信管理用户访问。

## 安装与首次配置

在仓库根目录执行：

```sh
sudo ./deploy.sh install
```

脚本构建管理和采集镜像，安装宿主机设备规则，并启用 `orbbec-stack.service`。
首次未绑定相机时只启动 Web。规则安装后建议重新插拔设备。

访问 `http://127.0.0.1:8765`，按[Web 指南](web.md)完成绑定、参数保存、逐台预览和
实际出帧确认。停止预览后再正式启动。型号和档位必须按实机填写，模板不能直接采集。

只构建并启动管理界面、不安装规则或开机服务：

```sh
./deploy.sh web
```

## CLI 入口

`deploy.sh` 使用管理容器完成日常操作，安装过程不把 `geminidepthport` 安装到宿主机 PATH。
其他指南中的 `geminidepthport ...` 指宿主 Rust CLI；需要它时按[构建要求](../../CONTRIBUTING.md#开发检查)准备 Rust、C/C++ 编译器等依赖，在仓库根目录执行：

```sh
cargo build --locked -p geminidepthport
./target/debug/geminidepthport --root "$PWD" validate --file inventory.v2.example.json --draft
```

后续示例中的命令名可以替换为该二进制的绝对路径。操作实际部署时用 `--root` 指向该部署目录，
配置、权限、操作锁和保护规则与 Web 相同。无需 CLI 的操作继续使用 Web 或 `deploy.sh`。

## 日常操作

```sh
./deploy.sh up       # 校验配置、逐台启动并检查整组
./deploy.sh status   # 查看项目容器
./deploy.sh logs     # 查看受管相机日志
./deploy.sh stop     # 先停相机/预览，再停管理服务
./deploy.sh down     # 停止并移除受管容器，保留宿主机配置
```

相同版本已健康运行时，重复 `up` 只复检，不先停机。部分启动失败保留已启动实例和诊断，
整组保持未就绪。停止失败时保留管理服务；不会为恢复而删除未知目录或接管其他项目容器。

配置维护顺序：结束录制并释放保护 → 停止采集及预览 → 修改配置 → 重新预览确认 → 启动。
生成版本、并发和保护行为见[生命周期](lifecycle.md)。

## 开机管理

```sh
sudo ./deploy.sh enable-autostart
sudo ./deploy.sh disable-autostart
systemctl status orbbec-stack.service
```

启用命令会立即启动检查式开机流程；禁用命令先停止相机组再禁用服务。
Web、CLI 和 systemd 使用同一操作锁与录制保护。不要叠加无条件启动驱动的另一套服务。
Docker 直接操作不受应用保护约束，不能作为日常维护入口。

## 访问与运行资料

从其他电脑访问时保持回环绑定：

```sh
ssh -L 8765:127.0.0.1:8765 用户@采集主机
```

`inventory.json` 保存设备配置；`.env` 保存本机部署环境；`state/` 保存生成版本、
任务、保护与证据；`work/` 保存预览临时文件。这些实际运行资料应只在受控环境保存。
配置目录与文件采用限制权限，容器以匹配 UID/GID 读取；不要用放宽权限来解决部署错误。

遇到设备、出帧或通信问题，先按[故障排查](troubleshooting.md)定位。
当前已验证范围和未验证项统一见[验证索引](../validation/README.md)。
