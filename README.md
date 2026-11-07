# GeminiDepthPort

![奥比中光与灵心巧手二次元角色在科技工坊中协作，展示视觉识别与灵巧抓取](docs/images/banner.png)

Orbbec Gemini RGB-D 相机采集服务，通过本地 Web 配置、预览和管理独立相机容器。
支持一至三台启用设备；默认提供三个未绑定、未启用的配置槽位。

管理入口是 Rust CLI/Web 服务。采集默认使用官方 Orbbec ROS 2 驱动，也可显式选择
[Rust 采集后端](docs/guides/rust-backend.md)。两种后端使用相同的管理入口，
具体型号、参数和保护能力以对应后端说明为准。

## 快速开始

需要原生 Linux、rootful Docker Engine、Compose 插件、systemd、udev 和 USB 设备。
宿主机无需安装 ROS；仅支持本机 Docker daemon。

```sh
git clone https://github.com/soulteary/GeminiDepthPort.git
cd GeminiDepthPort
sudo ./deploy.sh install
```

安装会构建镜像、安装 udev 规则并启用开机服务。首次配置未绑定设备时只启动管理界面，
不会打开相机。浏览器访问 `http://127.0.0.1:8765`：

1. 扫描设备，核对槽位型号与设备匹配，再绑定序列号。型号和后端需预先在[部署配置](docs/guides/configuration.md#准备部署配置)中选择，Web 暂不提供切换。
2. 填写实机支持的彩色/深度档位、命名空间、时间来源与检查策略，启用所需设备。
3. 逐台保存、启动预览并执行“检查出帧并确认此档位”。
4. 停止全部预览，执行“检查并启动相机组”。

```sh
./deploy.sh status
./deploy.sh logs
./deploy.sh stop
```

详细步骤、权限和开机管理见[部署指南](docs/guides/deployment.md)。Web 通过 Docker socket
管理容器，仅供可信管理用户使用；保持回环监听，远程管理使用 SSH 隧道。

## 数据与控制接口

- 彩色：原生 `yuyv`；深度：毫米 `16UC1`，保持原始深度坐标系。
- 每台相机显式绑定命名空间与光学帧，发布 Image、CameraInfo 及适用的静态 TF。
- 配置变更使相关档位确认失效；正式启动逐台检查出帧，再复查整组就绪。
- 录制保护阻止受管配置变更和停机；Rust 后端可提供绑定实际会话的原生重启保护。
- 标定快照、质量查询、原生录制与离线转换具有各自的证据和完成状态。

从[消费方接入](docs/guides/consumer-integration.md)开始，详细语义见[相机接口](docs/interfaces/camera.md)
和[版本化契约](docs/README.md#接口与契约)。

## 配置与运行资料

从[公开接入示例](examples/README.md)选择官方三机或 Rust 单机草稿，先离线校验，再按真实设备完成预览确认。

`inventory.json` 是部署配置入口，`inventory.v2.example.json` 是未绑定模板。
实际设备绑定、标定和凭据只保存在受控部署目录。`state/`、`work/`、`.env`
由 Git 忽略；不要提交已填写的真实 inventory。正式配置由管理器生成到
`state/generations/`，不要手工修改生成文件。

## 当前限制

三相机原生持续录制的 `RAW_QUEUE_OVERFLOW` 尚未关闭，完整长测和独立恢复仍待实机验收。
最近的软件修正和 CI 通过尚未证明持续录制可靠性或通用性能收益。功能范围、Web 尚未提供的操作及其他限制见[当前状态](docs/guides/current-status.md)；录制方式与格式选择见[录制指南](docs/guides/recording.md)。

## 验证范围

已有单机长期消费、多机短时消费、录制读回和管理闭环记录，见[验证索引](docs/validation/README.md)。
不同记录对应不同冻结产物；它们不等于最终发布版本的完整生产验收。
物理精度、硬件曝光同步、实际 USB 拔插及同负载性能收益仍需独立验证。

## 文档与开发

[文档首页](docs/README.md)提供部署、接口、运维、开发参考及验证报告入口。
[贡献指南](CONTRIBUTING.md)包含构建与离线检查命令。

本项目采用 [Apache-2.0](LICENSE)，为社区项目，非 Orbbec 官方产品。
SDK、ROS 与系统依赖保留各自许可，见 [NOTICE](NOTICE) 和[第三方说明](THIRD_PARTY_NOTICES.md)。
