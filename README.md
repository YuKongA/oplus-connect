# OPLUS 互联

OPLUS 互联是面向 OPPO / OnePlus 设备的 Linux 原生客户端

## 功能

- 连接、管理设备
- 剪贴板双向同步
- 手机消息通知镜像
- 手机投屏：视频、音频与键鼠反控
- 平板键鼠共享：键鼠双向自由穿梭
- 平板屏幕共享：由电脑端发起，可镜像当前屏幕或新建虚拟扩展屏，画面保持原始宽高比、硬件编码（H.264）与反向触控

### 尚未实现

- **平板端发起的镜像屏与扩展屏**：目前只有电脑端（设备详情 → 屏幕共享）能建立会话。平板侧发起的请求只完成能力字段应答（`Oplus_ExpandCast` / `ex_ability_sync`），不会启动采集管道，平板会停在加载状态。能力字段 `ScreenShareArrange` 仍固定为 `0`。
- **虚拟扩展屏的黑边**：虚拟输出由合成器按主显示器比例创建，3:2 平板配 16:9 桌面必然上下留边。portal 的 `ScreenCast` 只有 v5（KDE 6.8 仍未实现 `ScreencastStreams`），没有 `SetSize`，应用侧无法指定虚拟屏尺寸。
- **真实帧率**：KWin 6.7 的 screencast pacing 缺陷（KDE bug 524129）使采集端只有 15–19 fps，编码侧仍为 30 fps。修复提交只在 `Plasma/6.8` 与 `master`，未回移至 6.7 分支。

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

发布构建为动态链接，运行时依赖需由打包声明。平板键鼠共享基于 Wayland InputCapture portal v2；屏幕共享由电脑端发起，基于 PipeWire 与 GStreamer WebRTC，系统弹窗中选择「镜像屏幕」或「虚拟扩展屏」，首次使用需系统授权。

## 配置

| 环境变量                    | 说明                                                                               |
| --------------------------- | ---------------------------------------------------------------------------------- |
| `OPLUS_CONNECT_RUNTIME_DIR` | 指定 daemon 与 GUI 共用的私有 IPC 目录；需要是当前用户所有、权限 `0700` 的绝对路径 |
| `OPLUS_AUTO_ACCEPT=1`       | 跳过 GUI 审批自动接受连接，仅建议在无头调试时使用                                  |
| `OPLUS_EXPAND_CAST_PIN_CAPTURE=1` | 向合成器请求平板的采集尺寸；当前 KWin 拒绝该协商，仅用于验证新合成器 |

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
