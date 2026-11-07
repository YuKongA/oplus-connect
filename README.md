# OPLUS 互联

OPLUS 互联是面向 OPPO / OnePlus 设备的 Linux 原生客户端

## 功能

- 连接、管理设备
- 剪贴板双向同步
- 手机消息通知镜像
- 手机投屏：手机或电脑发起，支持视频、音频与键鼠反控
- 平板键鼠共享：键鼠双向自由穿梭；平板可主动请求
- 平板屏幕共享：支持镜像屏、虚拟扩展屏与反向触控

### 屏幕共享说明

- 可在设置中调整帧率、初始码率和动态码率；设置于下次共享生效。
- 支持的合成器可将虚拟屏调整为平板比例；不支持时回退到合成器尺寸，可能出现黑边。KWin 6.7 及更早版本还可能受到采集帧率限制。
- 采集与编码使用独立管道，静止画面会重复最近帧；采集失败或视频协商超时会报告错误。

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

平板键鼠共享需要 Wayland InputCapture portal v2。屏幕共享基于 PipeWire 和 GStreamer WebRTC，首次使用时需在系统弹窗中选择镜像屏或虚拟扩展屏并授权。

「设置 → 平板屏幕共享」支持 1–144 fps、500–50000 kbps 和动态码率；默认 30 fps、6000 kbps，动态码率开启。实际效果取决于设备、编码性能和网络质量。

## 配置

| 环境变量                                        | 说明                                                                               |
| ----------------------------------------------- | ---------------------------------------------------------------------------------- |
| `OPLUS_CONNECT_RUNTIME_DIR`                     | 指定 daemon 与 GUI 共用的私有 IPC 目录；需要是当前用户所有、权限 `0700` 的绝对路径 |
| `OPLUS_AUTO_ACCEPT=1`                           | 跳过 GUI 审批自动接受连接，仅建议在无头调试时使用                                  |
| `OPLUS_EXPAND_CAST_PIN_CAPTURE=0`               | 关闭「向合成器请求平板采集尺寸」；默认开启，合成器拒绝时自动回退                   |
| `OPLUS_EXPAND_CAST_BITRATE=<kbps>`              | 覆盖界面码率并关闭自适应，范围 500–50000 kbps                                      |
| `OPLUS_EXPAND_CAST_PIPELINE_LATENCY=auto\|<ms>` | 覆盖 pipeline latency；默认 0，`auto` 恢复按元素自动协商                           |
| `OPLUS_EXPAND_CAST_STATS=1`                     | 每 5 秒打印完整 WebRTC 统计，排查链路问题时使用                                    |
| `OPLUS_EXPAND_CAST_FAKE_SINK=1`                 | 诊断用：编码后本地丢弃，不发送给平板                                               |

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
