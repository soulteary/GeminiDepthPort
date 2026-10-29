# 参与贡献

从 main 创建聚焦的工作分支。接口或配置变更应同时更新文档和相关测试。
第三方代码必须记录公开来源、固定版本、许可及修改，并保留适用声明。
贡献代码采用本项目 Apache-2.0，第三方组件保留原许可。

## 开发检查

宿主机需要 Rust 1.85+、C/C++ 编译器与 Python 测试依赖。Docker/ROS 检查另需对应环境。
在仓库根目录运行：

```sh
pip install -r requirements-test.txt
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo build --locked -p geminidepthport
python3 tests/rust/test_management.py
python3 -m unittest discover -s tests -v
python3 -m compileall -q driver_build.py scripts runtime tests
node --check web/app.js
node tests/test_web_form.js
bash -n deploy.sh runtime/entrypoint.sh
ORBBEC_DEPLOY_DIR="$PWD" docker compose --profile cameras config --quiet
```

Node.js 只用于前端开发检查，不是 Web 运行依赖。`tests/legacy_reference/` 仅作为合成
回归的行为参考，不进入管理镜像。Linux 隔离管理测试见[测试参考](docs/development/linux-management-fixture.md)，
ROS bridge 检查见 [ros/README.md](ros/README.md)，CI 是容器构建检查的执行依据。

## Rust 覆盖率

覆盖率检查独立使用 Rust 1.90.0 和 cargo-llvm-cov 0.9.1；原有 Rust 1.85 数据面检查继续保留。
Linux 主机需要 C/C++ 编译器。先安装工具，再运行：

```sh
rustup toolchain install 1.90.0 --profile minimal --component llvm-tools-preview
cargo +1.90.0 install cargo-llvm-cov --version 0.9.1 --locked
python3 -m pip install -r requirements-test.txt -r requirements-coverage.txt
python3 -m unittest discover -s scripts/testing -p 'test_*.py' -v
bash scripts/testing/rust_coverage.sh all
```

`unit` 仅运行整个 workspace 的 Rust library/binary 单元测试；`combined` 运行 workspace 测试，
并在同一插桩环境构建 bins/examples，追加 Python 管理、native gate、SDK grid 和 recording 测试。
所需二进制缺失、外部 Python 套件跳过测试，或任一外部套件未产生或更新 Rust profile 都会失败。
所有这些检查使用合成数据，
不打开设备。CI 还设置 `GDP_COVERAGE_REAL_COMPOSE=1`，使用 Docker Compose CLI 校验生成配置，
无需 Docker daemon。原有容器和真实设备验证仍有独立的验收边界。

两个范围分别输出到 `target/coverage-reports/unit/` 和 `target/coverage-reports/combined/`。
`production.lcov`、`summary.json`、`summary.md` 和 `index.html` 只统计生产 Rust 的可执行行：
使用固定版本的 Rust AST 解析器排除精确 `#[cfg(test)]` 项和内建 `#[test]` 函数，
生产/测试混合行与未知 cfg 表达式保留；解析失败会阻止报告生成。不会排除生产适配器、CLI、
unsafe 代码或错误分支，也不会改写源文件。原始 `raw.lcov`、`raw.json` 保留供复核，
函数、region 和 branch 数字仅存在于原始报告中，不冒充过滤后的生产指标。
报告阶段同样显式选择整个 workspace，并保存 `cargo metadata` 的 package 清单；
转换器按清单校验每个 workspace package 均出现在生产报告中，漏报 package 会失败。

覆盖率报告不表示硬件、ROS 传输或物理精度已通过验收。强制终止的子进程可能来不及写出 profile；
相关生命周期断言仍须完整通过。CI 要求 Linux `combined` 的生产 Rust 行覆盖率至少为 90%；
`unit` 继续单独报告，不套用这一合并门槛。比较前后结果时须使用相同平台、工具链和测试范围。
调整门槛应同时审查按 package 的分母及未覆盖行，不能通过放宽测试或排除生产代码来提高数字。
在容器中运行 Linux 测试时请启用 `--init`，确保孤儿进程被正常回收；测试仍不访问真实设备。

## 文档与验证

文档按读者任务组织，每项行为只设一个完整定义位置：

- `README.md`：项目定位、最短启动路径和主要入口。
- `docs/guides/`：部署、配置、接入和故障处理步骤。
- `docs/interfaces/`：CLI、HTTP、ROS 和数据字段参考。
- `docs/contracts/`：版本化语义、不变量、状态转换和失败边界。
- `docs/development/`：模块职责、实现原因、构建与调试。
- `docs/validation/`：验收方法、冻结产物与历史结果。

crate 和工具 README 保留本地入口，用链接引用共享语义；指南不复制完整契约。
代码注释解释所有权、锁、单位、时钟和失败后的状态，不复述函数名或开发批次。
`python3 -m unittest discover -s tests -p test_documentation.py -v` 检查本地文件链接、
Markdown 标题锚点和 Shell 示例语法；完整 Python 测试与 CI 已包含该检查。
操作文档描述当前行为；接口语义写入对应版本化契约。开发批次、设计讨论与执行流水
不进入用户指南。测试报告记录冻结产物、方法、成功与失败、测量范围和未验证项，
不得以新的二进制指纹替代旧测试产物，也不得通过放宽门槛掩盖失败。

真实 inventory、设备身份、标定、原始图像、凭据和 `.env`、`state/`、`work/` 不得提交。
公开测试使用合成身份；报告只保留可复核的必要汇总和产物摘要。
