# OPLUS 互联

OPLUS 互联是面向 OPPO / OnePlus 设备的 Linux 原生客户端

## 功能

- 连接、管理设备
- 剪贴板双向同步
- 手机消息通知镜像
- 手机投屏：视频、音频与键鼠反控
- 平板键鼠共享：键鼠双向自由穿梭

## 构建与运行

需要 GStreamer 1.24+，以及 WebRTC/libnice、VA 硬解与音频输出插件。Debian/Ubuntu：

```bash
sudo apt install pkg-config libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libgstreamer-plugins-bad1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-nice gstreamer1.0-pipewire mesa-va-drivers
```

GUI 会自动启动内嵌 daemon，正常使用只需运行 GUI：

```bash
cargo run --release --manifest-path oplus-connect-gui/Cargo.toml
```

单独调试后端：`cargo run --manifest-path oplus-connect-daemon/Cargo.toml`。

发布构建为动态链接，运行时依赖需由打包声明。平板键鼠共享基于 Wayland InputCapture portal v2，使用时需系统授权

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
