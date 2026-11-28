# OPLUS 互联

OPLUS 互联是面向 OPPO / OnePlus 设备的 Linux 原生客户端

## 功能

- 连接设备
- 剪贴板双向同步（手机/平板）
- 消息通知镜像（手机/平板）
- 键鼠共享（手机/平板）
- 手机投屏
- 平板屏幕共享

### 屏幕共享说明

- 采集与编码使用独立管道，静止画面会重复最近帧；采集失败或视频协商超时会报告错误。
- 可在设置中调整帧率、初始码率和动态码率。数值输入时校验，会在下次启动屏幕共享时生效。
- 支持的合成器可将虚拟屏调整为平板比例；不支持时回退到合成器尺寸，可能出现黑边。KWin 6.8.0 以下还可能受到采集帧率限制。
- 平板触摸反控需要系统远程桌面输入授权。虚拟屏反控需要 portal 提供输出映射标识，以及合成器支持带输出名称的 xdg-output 协议。

## 构建与运行

```bash
makepkg -si                                                       # Arch Linux
nix run .#                                                        # Nix（提供 nixosModules.default）
cargo run --release --manifest-path oplus-connect-gui/Cargo.toml  # 源码
```

需 VA-API 驱动、PipeWire 与 `xdg-desktop-portal`；启用防火墙时需放行局域网回连。

## 配置

| 环境变量                                        | 说明                                                                               |
| ----------------------------------------------- | ---------------------------------------------------------------------------------- |
| `OPLUS_CONNECT_RUNTIME_DIR`                     | 指定 daemon 与 GUI 共用的私有 IPC 目录；需要是当前用户所有、权限 `0700` 的绝对路径 |
| `OPLUS_CONNECT_LOG`                             | 日志级别 `trace`/`debug`/`info`/`warn`/`error`/`off`，默认 `info`                  |
| `OPLUS_CONNECT_KMS_IDLE_RELEASE_SECS`           | 键鼠共享在持续无输入事件达该秒数后自动把捕获释放回电脑（默认 10，`0` 关闭）        |
| `OPLUS_AUTO_ACCEPT=1`                           | 跳过 GUI 审批自动接受连接，仅建议在无头调试时使用                                  |
| `OPLUS_EXPAND_CAST_PIN_CAPTURE=0`               | 禁止请求调整虚拟屏采集尺寸；默认请求，失败时回退                                   |
| `OPLUS_EXPAND_CAST_BITRATE=<kbps>`              | 覆盖界面码率并关闭自适应，范围 500–50000 kbps                                      |
| `OPLUS_EXPAND_CAST_PIPELINE_LATENCY=auto\|<ms>` | 覆盖 pipeline latency；默认 0，`auto` 恢复按元素自动协商                           |
| `OPLUS_EXPAND_CAST_STATS`                       | 设置任意值时每 5 秒打印 WebRTC 统计，排查链路问题时使用                            |
| `OPLUS_PROTOCOL_LOG_DIR`                        | 把协议抓包写入该目录（`0600`，拒绝符号链接）；含解密后的剪贴板/通知内容，默认关闭  |
| `OPLUS_PC_NOTICE_CLIPBOARD=0\|1`                | `pcAbility.Notice_ClipBoard`，默认 `0`，与对照的官方 Windows 客户端一致            |
| `OPLUS_PC_NOTICE_CROSS_DEVICE=0\|1`             | `pcAbility.Notice_CrossDevice`，默认 `0`；独立的 `Notice_CrossDevice` 消息不受影响 |
| `OPLUS_PC_CAST_UNLOCK_MODE=auto\|every`         | `Synergy_CastUnlock_Mode`，默认 `every`；`auto` 是旧的 Linux 值，仅用于回退对照    |
| `OPLUS_CLIPBOARD_READ_TIMEOUT_MS`               | 慢速剪贴板读取的告警阈值，不丢弃结果；范围 100–30000 ms，默认 500                 |

## 项目结构

```text
oplus-connect-daemon/   # 设备协议、连接状态、剪贴板与通知后端
oplus-connect-gui/      # GPUI Kit 桌面客户端
docs/                   # 协议与调试文档
PKGBUILD                # Arch Linux 软件包构建配方
flake.nix               # Nix flake 打包与开发环境
nix/                    # NixOS 模块
oplus-connect.desktop   # 桌面入口
LICENSE                 # MIT 许可证
```

## 开发

GitHub Actions 在 Arch Linux 容器中检查格式、运行后端与 GUI 测试并构建 pacman 软件包，同时执行 Nix flake 格式、模块与构建检查。

```bash
# GUI 测试
cargo test --manifest-path oplus-connect-gui/Cargo.toml

# 后端测试
cargo test --manifest-path oplus-connect-daemon/Cargo.toml
```

协议细节见 [docs/oplus-connect-protocol.md](docs/oplus-connect-protocol.md)

## 许可证

本项目以 [MIT 许可证](LICENSE) 发布。
