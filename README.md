# OPLUS 互联

OPLUS 互联是面向 OPPO / OnePlus 设备的 Linux 原生客户端

## 功能

- 连接、管理设备
- 剪贴板双向同步
- 手机消息通知镜像

## 构建与运行

GUI 会自动启动内嵌 daemon，正常使用只需运行 GUI：

```bash
cargo run --release --manifest-path oplus-connect-gui/Cargo.toml
```

构建 release 二进制：

```bash
cargo build --release --manifest-path oplus-connect-gui/Cargo.toml
./oplus-connect-gui/target/release/oplus-connect-gui
```

调试后端时，可单独运行 daemon：

```bash
cargo run --manifest-path oplus-connect-daemon/Cargo.toml
```

## 配置

| 环境变量                    | 说明                                                                               |
| --------------------------- | ---------------------------------------------------------------------------------- |
| `OPLUS_CONNECT_RUNTIME_DIR` | 指定 daemon 与 GUI 共用的私有 IPC 目录；需要是当前用户所有、权限 `0700` 的绝对路径 |
| `OPLUS_AUTO_ACCEPT=1`       | 跳过 GUI 审批自动接受连接，仅建议在无头调试时使用                                  |

## 项目结构

```text
oplus-connect-daemon/   # 设备协议、连接状态、剪贴板与通知后端
oplus-connect-gui/      # GPUI Kit 桌面客户端
docs/                   # 协议与调试文档
packaging/              # 桌面入口与本地图标安装脚本
```

## 开发

```bash
# GUI 测试
cargo test --manifest-path oplus-connect-gui/Cargo.toml

# 后端测试
cargo test --manifest-path oplus-connect-daemon/Cargo.toml
```

协议细节见 [docs/oplus-connect-protocol.md](docs/oplus-connect-protocol.md)
