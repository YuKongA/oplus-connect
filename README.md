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

- 可在设置中调整帧率、初始码率和动态码率；设置于下次共享生效。
- 支持的合成器可将虚拟屏调整为平板比例；不支持时回退到合成器尺寸，可能出现黑边。KWin 6.7 及更早版本还可能受到采集帧率限制。
- 采集与编码使用独立管道，静止画面会重复最近帧；采集失败或视频协商超时会报告错误。
- 平板触摸反控需要系统远程桌面输入授权；触摸映射到所选屏幕，黑边不响应。虚拟屏反控还需要 portal 提供输出映射标识，以及合成器支持带输出名称的 xdg-output 协议。

## 构建与运行

在 Arch Linux x86_64 上，从已有的 Git 检出目录构建并安装 pacman 软件包：

```bash
sudo pacman -S --needed base-devel git
OPLUS_CONNECT_SOURCE_URL="file://$PWD" makepkg -si
```

上述命令构建本地仓库当前分支已提交的代码，不包含未提交更改；不设置 `OPLUS_CONNECT_SOURCE_URL` 时使用远端最新提交。构建会运行 GUI 测试。安装后从应用菜单启动，或运行 `oplus-connect`。VA-API 需要适配显卡的驱动；无可用 H.264 硬件编码器时可安装 `gst-plugins-ugly` 使用软件编码。

使用 Nix flake 构建（需要启用 `nix-command` 与 `flakes`）：

```bash
nix run .#            # 构建并直接运行
nix build .#          # 构建，产物位于 ./result/bin/oplus-connect
nix develop           # 带全部构建依赖的开发环境
nix profile install . # 安装到当前用户的 profile
```

flake 暴露 `packages.default`（GUI，已内嵌 daemon）、`apps.default`、`devShells.default`、`nixosModules.default` 和 `overlays.default`。

NixOS 上通过模块声明式启用：

```nix
{
  inputs.oplus-connect.url = "github:YuKongA/oplus-connect";

  outputs =
    { nixpkgs, oplus-connect, ... }:
    {
      nixosConfigurations.host = nixpkgs.lib.nixosSystem {
        modules = [
          oplus-connect.nixosModules.default
          {
            programs.oplus-connect.enable = true;
            # 默认开放设备回连端口；不需要时关闭。
            # programs.oplus-connect.openFirewall = false;
          }
        ];
      };
    };
}
```

模块会安装软件包、开放设备发现与串流使用的 TCP/UDP 端口，并默认启用 PipeWire、`xdg.portal` 与 Vulkan 图形支持。GUI 在内嵌 daemon 中运行后端，因此不需要额外的系统服务。

Nix 构建使用 nixpkgs 提供的 GStreamer，`gst-plugins-bad` 已启用 VA-API 插件。平板屏幕共享只使用 H.264 编码：优先 `vah264enc`，不可用时回退 `x264enc` 软件编码；手机投屏由手机编码（默认 H.265），电脑端用 `vah264dec`/`vah265dec` 硬解，缺少这些元素时会直接报错而不回退软件解码。部分驱动（例如 NVIDIA 的 NVDEC VA 驱动）需要设置 `GST_VA_ALL_DRIVERS=1` 才会注册 VA 元素，且仍可能缺少 `vah264enc`/`vapostproc`。屏幕共享还需要 PipeWire 及 `xdg-desktop-portal` 后端（NixOS 上启用 `xdg.portal.enable`），Vulkan 驱动由系统提供（NixOS 上启用 `hardware.graphics`）。

直接从源码运行需要支持 Rust 工具链和 GStreamer，GUI 会自动启动内嵌 daemon：

```bash
cargo run --release --manifest-path oplus-connect-gui/Cargo.toml
```

单独调试后端：`cargo run --manifest-path oplus-connect-daemon/Cargo.toml`。

键鼠共享需要 Wayland InputCapture portal v2。屏幕共享及触摸反控基于 PipeWire、GStreamer WebRTC 和 RemoteDesktop portal；电脑端发起时在设备详情选择镜像或扩展，再在系统弹窗中授权屏幕与输入控制。

键鼠共享卡死与手机/平板无关，属电脑端输入捕获链路（Wayland portal/EIS）停止投递事件：日志会停在 `kms sharing heartbeat` 的 `idle_ms` 持续增长、同时 `/kms/status` 仍为 connected。此时 `Ctrl+Alt+Escape` 也可能收不到按键，只能结束共享（设备侧断开或退出程序）来释放捕获。`OPLUS_CONNECT_LOG=debug` 启动并在终端保留输出可记录 EIS 启停、被丢弃的输入、跨回事件、会话结束原因和每 5 秒一次的共享心跳。

「设置 → 平板屏幕共享」支持 1–144 fps、500–50000 kbps 和动态码率；默认 30 fps、6000 kbps，动态码率开启。实际效果取决于设备、编码性能和网络质量。

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

GitHub Actions 在 Arch Linux 容器中检查格式、运行后端与 GUI 测试，并构建 pacman 软件包作为工作流产物。

```bash
# GUI 测试
cargo test --manifest-path oplus-connect-gui/Cargo.toml

# 后端测试
cargo test --manifest-path oplus-connect-daemon/Cargo.toml
```

协议细节见 [docs/oplus-connect-protocol.md](docs/oplus-connect-protocol.md)

## 许可证

本项目以 [MIT 许可证](LICENSE) 发布。
