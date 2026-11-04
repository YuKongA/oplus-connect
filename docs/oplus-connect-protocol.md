# OPLUS 互联：跨端协议与实现状态

最后更新：2026-11-02  
目标设备：一加 15T / PLZ110（ColorOS 17）、OPPO Pad Mini / OPD2515（Android 17）  
官方客户端：OPPO 互联 17.20.3  
本文是协议、逆向结论和当前实现状态的唯一入口。原始 pcapng、logcat、二进制和一次性实验输出保留在 `analysis/` 与 `windows-protocol-evidence/`，不复制进本文。

---

## 0. 结论摘要

### 0.1 已经确定

1. 手机主动连接 PC 的 TCP `46999`，PC 先发 EPI offer；数据通道 framing 为 `u16 BE length + body`，长度不含长度字段本身。
2. 业务帧前 2 字节不是固定 `msgId`，而是 OAF transport 头中的 10 bit `sessionId`。
3. session 是本地 OAF session 的编号，和 agentId、channelId、`connectionID`、`DATA_u8MsgType=52` 都不是同一个东西。
4. 两端各自分配并通告自己的 session；数值不要求相同，也不要求不同。确认后的 session 是连接级双向通道，双方都可能用任意已确认 session 发包。
5. 对端通过 `0x0004 sub=01` 的 session info 列表获知 session；`0x0008 sub=02` 和 `0x0010` 不携带 transport session。
6. `0x0004 sub=02 status=0` 表示接受，并携带被接受的 session 列表。
7. 手机侧确实按 session 过滤。未知 session 会打出 `Session Details not present for N`，不会进入 DBS/topic dispatch。
8. 业务密文是 AES-128-GCM；key/IV/AAD 来自 `HMAC-SHA512(KSC, client_rc2 || server_rc2 || frame[3..11])` 的前 16/12/20 字节。
9. session 每次业务连接重新分配；agent handle 在同一服务端进程内跨重连、甚至删除配对后仍稳定。两者不是同一编号空间。
10. Linux 剪贴板双向同步已实现：`arboard` 读写系统剪贴板，远端写入不回环发送；
    daemon 已在 OPD2515 平板和一加 15T / PLZ110 上完成双向真机验收，两个方向都真正写入系统剪贴板。
11. OPD2515 的官方客户端与 Rust daemon 使用不同 endpoint、device id、agent
    handle 和每连接 session，但协议结构相同。
12. DBR 头 `9..17` 还是 8 字节 delivery sequence。接收端按它做重放去重；
    同一连接复用同一个值会让首帧之后的消息被静默丢弃。
13. Linux 通知同步已实现：PC 先发送 Add/Delete/Error 三条启动控制消息，
    topic sync 为 2 pub / 2 sub；手机推送 `HeySynergy_Notification_Add` 后，
    Linux 解析标题、正文和 PNG 图标并通过 D-Bus 展示。官方 Windows 抓包与
    现场记录仅作为实现依据，保存在 `windows-protocol-evidence/`。

### 0.2 当前实现状态

| 能力 | 官方客户端 | Rust harness / daemon |
| --- | --- | --- |
| SDP 发现 | 成功 | 成功；含 UDP QUERY 和 TCP HTTP QUERY |
| SCP Hello/Sync/SyncSecure | 成功 | 成功 |
| UKEY2 + KSC | 成功 | 成功 |
| EPI offer/answer | 成功 | 成功 |
| 10/11/12 数据认证 | 成功 | 成功 |
| 服务/profile 注册 | 成功 | 已发出，phone 有响应 |
| 手机→PC 业务帧接收/解密 | 成功 | 成功 |
| PC→手机 session 建立 | 成功 | 成功；OPD2515 `203`、PLZ110 `211` 均被 `status=0` 确认 |
| PC→手机 clipboard 落地 | 成功 | 成功（OPD2515、PLZ110） |
| 手机→PC clipboard 落地 | 成功 | 成功（OPD2515、PLZ110） |
| 手机→PC notification 落地 | 成功 | 成功；Linux D-Bus 展示标题、正文和 PNG 图标（PLZ110） |

### 0.3 当前审查结论

旧的“把 session 707 改成 700/702 再试”不是协议层面的正确修复方向。2026-10-31
两场重连证明 session 每次都变化，不能把某一历史编号当成白名单。历史实验中的
`status=1` 记录仍保留在证据中，但不是当前通知或剪贴板实现的阻塞结论，也不能用
固定 session 解决。

本轮确认并已修正的实现问题：

- 固定 `DEFAULT_APP_SESSION_ID=614` 与固定 oshare session `238` 均不符合官方行为；
- oshare 建会话曾把手机的 `0x1d3a` 当成本端 initiator，而本端注册的是 `0x1e98`；
- EPI endpoint id 被误认为每 offer 重建，官方实际在同一服务进程内复用；
- 代码必须从手机 `0x0008 sub=02` 分别学习 unite 与 oshare 的 peer handle；
- 接收过滤曾只认“手机通告的 session”，但实测手机会用 PC 通告的 session 回发，
  现已改为同时接受双方所有已确认 session。

2026-10-31 的剪贴板真机验收：OPD2515 本地 unite session `203` 获 `status=0`，
两个方向均通过系统剪贴板验证；PLZ110 以 PDID `E1C0DDE99004` 独立复验，本地
unite session `211`、oshare session `851` 均被接受，手机回发 `242`，双向成功，
`Session Details not present` 为 `0`。此前的阻塞点是 DBR 头 `9..17` 的 8 字节
sequence 每帧复用，手机将后续帧当重复消息丢弃；改为单调递增后
`handleTopicSync` 正常执行。

2026-11-01 后端审查修复：仅在 UKEY2 完成且 SyncSecure 通过加密验证后提交 KSC；非法 SCP 头立即断开，并限制帧缓冲与读取期限。手动审批按每连接 `connection_id` 校验，不再根据 IP 回退批准其他连接；IPC 已迁移到用户私有运行目录，合同以 `oplus-connect-daemon/src/ipc/` 的实现与测试为准。daemon/harness 使用同一个 Rust library，协议测试不再随两个 binary 重复运行。

剪贴板关闭后，即时回应、轮询及调试注入均禁止发送文本。Linux 通知工作线程在三秒限频窗口中保留最新快照并尾部补发，Delete 同时取消待更新状态。SDP 工作线程可停止并等待退出；daemon 支持 SIGINT/SIGTERM 清理。

---

## 1. 总体链路

```text
SDP 发现
  -> SCP TCP 10150/10152/10155
  -> SCP Hello / Sync
  -> UKEY2
  -> 加密 SyncSecure（16B KSC）
  -> 数据通道 TCP（46999/48999 等）
  -> EPI offer/answer
  -> 10/11/12 KSC 认证
  -> 0x0008 profile/agent 注册
  -> 0x0010 multichannel 协商
  -> 0x0004 sub=01/sub=02 建立 OAF session
  -> transport(local_session) 数据总线 / DBRouter
  -> Oplus_MacConnect 等 DBS topic
  -> 剪贴板、通知、文件、投屏等业务
```

### 1.1 角色

| 角色 | 官方实现 |
| --- | --- |
| 手机 | SCP client，数据通道 client |
| PC | SCP server，数据通道 server |
| PC 官方控制端口 | `10150`、`10152`、`10155` |
| PC 官方数据端口 | 典型为 `46999`，实际端口会由 SCP Sync 的 `custom_data` 通告 |
| 手机反向探测端口 | `10153` |

数据通道的授权方向是“手机连 PC”，不是 PC 主动连手机。SCP 方向有两种：手机发现
PC 后拨 `10150`/`10152`（PC 为 SCP server），或 PC 主动拨手机的 `10152`（PC 为
SCP client，见 §3.2）。无论哪种，业务数据通道仍由手机拨回 PC 通告的端口。

设备移除跟随**数据通道**，不跟随 SCP 控制连接：手机在 `SyncSecure` 和 business
interaction 之后会关闭 SCP 握手 socket（PLZ110 实测在 business interaction 后约
0.2 秒关闭），而剪贴板、通知继续走数据通道。daemon 在握手 socket 关闭时只安排一次
延迟清理，认证成功的数据通道会取消它；最后一个数据通道关闭时才从 `/devices` 移除
设备并广播 `disconnect_callback`。官方日志中的对应事件是
`EVT_ACCESSORY_DISCONNECTED_IND` 与各 agent 的 `DISCONNECT_IND`
（`panta-slice.log:330-360`、`560-620`）。多端口连接同一设备时，任一端口关闭都不会
提前清除设备。

---

## 2. SDP 发现

### 2.1 UDP 发现

官方和已实现的发现路径：

```text
phone -> 239.255.255.250:10150  QUERY
PC    -> phone_ip:10151          ANNOUNCE
```

PC 的 ANNOUNCE 源端口必须是 `10151`。手机侧只会处理符合 `NS: scp.device` 的查询/宣告。

ANNOUNCE 关键字段：

```text
MAN: "sdp/1"
NS: scp.device
DSP: 10151
CSP: <PC SCP 控制端口>
DMS: 0
PDID: <12 hex>
AD: <短账号摘要>
DT: 6
DN: <显示名>
VIRTUAL_IP: <PC 虚拟地址>
```

实测中 `VIRTUAL_IP` 可以是 `172.30.200.173` 或 WLAN 地址，具体取决于手机使用的链路。

### 2.2 TCP HTTP QUERY

PLZ110 在某些状态下不是直接建立 SCP，而是先连到 `CSP` 端口，发送：

```text
QUERY * HTTP/1.1
MAN: "sdp/1"
NS: scp.device
...
```

这仍然是 SDP 查询，不是 SCP 帧。PC 必须在同一条 TCP 连接上回复一个 `ANNOUNCE * HTTP/1.1`，然后手机才会继续走 SCP 或重新拨号。

当前 harness 已在 `probe` 中加入这条处理。若没有它，手机会连接 `10150` 后被当成非法 SCP 帧并在 5 秒后断开。

### 2.3 PDID 缓存

手机会按 `PDID` 缓存设备。用官方 PDID 重新起 harness 时，手机可能把新设备当作“旧的已连接设备”过滤掉，只发 UDP/TCP QUERY，不发起新的 SCP。

测试时应给 harness 一个新的 12 位十六进制 PDID，例如：

```text
e1c0dde99002
```

生产实现不能依赖固定 PDID；PDID 应由本机持久身份生成，测试时才使用临时新值。

### 2.4 发起方向与 PC 侧信息保留

发现仍是单向的：**PC 宣告，手机发现**。PC 发送 SDP `ANNOUNCE`（广播、组播和
已知对端单播）并在 `10150` 应答 `QUERY`，手机的 PTC 发现调度器把 PC 加入
「附近的设备」；PC 不扫描手机。

但连接可以由 PC 对已配对设备发起：PC 作为 SCP client 直接拨手机 `10152`，
完成后手机仍把数据通道拨回 PC 在 Sync `custom_data` 中通告的端口。因此：

- 首次连接必须由手机发现并拨入；之后 PC 只需记住设备 IP、device id 和 KSC。
- PC 发起的连接会触发手机侧确认，不是静默连接；用户拒绝时返回
  `rejected by user`（错误码 2516）。
- 数据通道方向不变：手机连 PC 的 `46999` 或 `custom_data` 端口。官方日志
  `analysis/official-reconnect-20261031-173856/panta-slice.log:900-927` 的
  `socketAccepted → conOpenConnection → epOpenConnection` 仍是这个顺序。
- 断开后手机和 PC 都可以发起重连；PC 端必须保持稳定的 PDID 与 KSC，否则
  手机会把它当成新设备（§2.3）。
- GUI 保存所有连接过的设备；侧栏只显示当前在线设备，设备详情页按在线优先、
  最近使用在前列出历史设备。离线设备保留地址并可在详情页重新「连接」；
  已连接设备可主动「断开」，daemon 关闭对应 Panta 数据通道后走正常断线流程。


---

## 3. SCP 传输层

### 3.1 帧格式

SCP over TCP：

```text
raw0 raw1 | payload_len | flags | reserved | tail[4] | payload[payload_len]
 40   30+       u16 BE      u16 BE   u16 BE
```

线上固定是 12 字节头。`payload_len` 只计算 12 字节头之后的
`payload`，不包含头本身；TCP 必须按完整字节流重组，不能按单次
`recv()` 或 TCP 段边界拆帧。

`raw1` 的低 4 bit 是标志位：

```text
base       0x30
response   | 0x04
encrypted  | 0x08
business   | 0x01
```

所以实测常见头部是 `0x30`、`0x34`、`0x38`、`0x3c`、`0x39`、
`0x3d`。不要用完整的 16 bit 常量比较第二字节。

头部关键常量：

```text
raw0         0x40
raw1 base    0x30
header len   12
tail         7b 08 <varint command>
command      active_connection = 2349
```

`flags` 常见为 `0x0002`（请求）或 `0x0000`（超时/错误）；
`reserved` 实测为 `0x0000`。

### 3.2 握手阶段

```text
SocketAccept
  -> ReadHello / WriteHello
  -> ReadSync / WriteSync
  -> UKEY2 ClientInit / ServerInit / ClientFinished / Finished
  -> ReadSyncSecure / WriteSyncSecure (16B KSC)
  -> business interaction
  -> SCP socket closed by phone (connection continues on data channel)
  -> disconnect
```

`SyncSecure` 与 business interaction 完成后，手机主动关闭这条 SCP 握手 socket；
连接并不因此断开，业务数据继续走 `46999` 等数据通道。判断“已连接”必须依据数据
通道，不能依据 SCP socket 是否存活。

PC 主动发起时顺序相反：PC 拨手机 `10152`，发送 Hello/Sync，作为 UKEY2 initiator
发送 ClientInit（next protocol 必须是 `AES_256_CBC-HMAC_SHA256`，手机 server 拒绝
GCM-SIV），随后发送加密 SyncSecure（PC PDID + KSC）和 business `interact hello`。
手机确认后回复 `accept`，并拨回 PC 在 Sync `custom_data` 中通告的数据端口。

Hello 必须回同 service/namespace/channel，sequence 按官方实现递增。UKEY2 完成后，SyncSecure 必须使用同一次握手派生的加密通道；不能把 UKEY2 的 AES key 当成业务 KSC。

实现先验证加密帧并解密，再检查 UKEY2 完成状态、device id 与 16 字节 KSC，之后才写入会话映射。明文 SyncSecure、错误密文或未完成握手不会改变已保存的 KSC。

### 3.3 应用层 message type

12 字节 SCP 头之后的 payload 是 `ScpWrapData` protobuf：

```protobuf
message ScpWrapData {
  int32 msg_type = 1;
  bytes data     = 2;
}
```

已知 `msg_type`：

| 值 | 名称 | 用途 |
| ---: | --- | --- |
| 0 | `Hello` | service/namespace/channel 握手 |
| 1 | `Sync` | 设备名、model、link type、`custom_data` |
| 2 | `SyncSecure` | 交换 16 B KSC、device id、6 B alias |
| 3 | `Ukey2Init` | 外层 UKEY2 消息封装 |
| 4 | `Ukey2Finished` | 外层 UKEY2 完成消息封装 |
| 7 | `Disconnect` | 断开原因 |
| 65535 | error | 手机侧协议错误 |

外层 SCP 的 `Sync=1`、`SyncSecure=2`、`Ukey2Init=3`、
`Ukey2Finished=4` 与内层 UKEY2 的 `ClientInit=2`、`ServerInit=3`、
`ClientFinished=4` 是两套枚举，不能混用。

关键 protobuf 字段：

```protobuf
message ScpHelloData {
  int32  sequence_no  = 1;
  string service_name = 2;   // active_connection
  string namespace    = 3;   // com.heytap.accessory
  string channel      = 4;
}

message ScpSyncData {
  string device_name         = 1;
  int32  key_type            = 2;
  bytes  custom_data         = 3;  // 可通告额外数据端口
  int32  device_type         = 4;
  int32  active_trigger_mode = 5;
  string model_id            = 6;
  int32  link_type           = 7;
}

message ScpSyncSecure {
  string device_id = 1;
  bytes  ksc       = 2;  // 16 B
  bytes  ksc_alias = 3;  // 6 B
}
```

### 3.4 UKEY2 与 SCP 通道密钥

UKEY2 使用公开的 ClientInit / ServerInit / ClientFinished 握手。当前实现和
真机互通按以下参数派生 SCP 通道密钥：

```text
ikm_1  = SHA-256(ECDH shared secret)
salt_1 = "UKEY2 v1 next"
info_1 = raw(ClientInit) || raw(ServerInit)
prk_1  = HKDF-SHA256(ikm=ikm_1, salt=salt_1, info=info_1)

salt_2 = SHA-256("D2D")
info_2 = "client"
key    = HKDF-SHA256(ikm=prk_1, salt=salt_2, info=info_2, len=32)
```

SCP 加密使用 AES-256-GCM，无 AAD，密文后附 16 B tag。nonce 取
`ClientFinished` 字段 2 的前 12 字节；同一 SCP 握手内的双向报文沿用该
key/nonce。该密钥只保护 SCP 通道，不能拿它代替业务 KSC。

### 3.5 KSC

`SyncSecure` 中的 16 字节 KSC 是同一配对连接的业务密钥材料。数据通道 10/11/12 认证和 DBS AES-GCM 都使用它，但用途和派生不同。

---

## 4. 数据通道 EPI 与认证

### 4.1 EPI framing

```text
u16 BE body_len | body
```

PC 先发 offer，`body[0] = 0x01`；手机回 answer，`body[0] = 0x02`。

官方 offer 为 109 字节，answer 为 104 字节，结构都是：

```text
msgType(1) + u16 BE(2) + endpointId(36) + flags(1) + tlvCount(1) + TLVs
```

精确偏移：

| 偏移 | 长度 | offer | answer |
| ---: | ---: | --- | --- |
| 0 | 1 | `0x01` | `0x02` |
| 1 | 2 | 常量 `0x0002` | 手机常量通常 `0x000a` |
| 3 | 36 | `OAFP_M...`，同一服务进程内跨重连复用 | `OAFP_B...`，同设备跨连接可复用 |
| 39 | 1 | 实测 `0x20` | 实测 `0x20` |
| 40 | 1 | TLV count `0x0d` | TLV count `0x0d` |
| 41 | n | 13 个 TLV | 13 个 TLV |
| 41+n | 0/2 | 无 | 手机 answer 末尾可能有 2 字节未知值 |

EPI 校验为 `length >= 0x29`，解析器只消费 `tlvCount` 个 TLV；answer
末尾的多余 2 字节会被忽略，已排除常见 CRC-16，但算法仍未确认。

#### offer 关键 TLV

| tag | 长度 | 含义 | PC 值 |
| ---: | ---: | --- | --- |
| 1 | 4 | apdu | `0x00100000` |
| 3 | 2 | max session | `0x03fe` |
| 4 | 2 | sl timeout | `0x2710` |
| 2 | 2 | ssdu | `0xfffb` |
| 5 | 2 | tl window | `0x000a` |
| 11 | 1 | compression | `0x02` |
| 13 | 1 | multichannel | `0x01` |
| 14 | 6 | opaque | 官方值可原样复制 |
| 8 | 1+n | friendlyName | PC 主机名 |
| 7 | 1+n | manufacturer | `manufa-test` |
| 6 | 1+n | product | `product-test` |
| 10 | 1 | device category | `0x06` = PC |
| 12 | 1 | profile count | 与后续服务列表一致 |

字符串 TLV 为 `tag + u8 length + UTF-8 bytes`。

#### endpoint id

`OAFP_M...` 是临时 endpoint id，不是设备 ID，也不是持久身份。2026-10-31
的 17:21 成功采集、17:39 普通重连和删除配对后重连都使用同一个
`OAFP_M000000000000E35E8EE8E09083E018`；官方服务进程没有重启。因此当前证据
支持“每进程一个值”，不支持旧结论“每次 offer 重新生成”。进程重启、服务升级或
重启系统后是否变化仍明确未知。实现应在自己进程内生成一次并复用。

### 4.2 10/11/12 KSC 认证

```text
phone -> PC  0x10  auth request
PC -> phone  0x11  auth response
phone -> PC  0x12  auth confirm
```

这些帧仍使用 `u16 BE body_len + body` framing，但认证帧长度必须精确：

| 帧 | 长度 | 精确字段 |
| --- | ---: | --- |
| `0x10` request | 38 | `msgType(1), platform(1), client_rc2(8), client_fc u64 BE(8), client_mac(8), device_id(6), alias(6)` |
| `0x11` response | 27 | `msgType(1), platform(1), reserved(1), server_rc2 ASCII digits(8), server_time u64 BE(8), server_mac(8)` |
| `0x12` confirm | 11 | `msgType(1), platform(1), reserved(1), confirm_mac(8)` |

平台字节只被记录，不参与校验：手机发 `0x02`，Windows 发 `0x01`。
`server_rc2` 是 8 个 ASCII 数字，不是随机二进制。

设备 id 的十六进制文本大小写不一致：SCP `SyncSecure` 使用大写（实测
`C77C702EC021`），panta `AUTH_REQUEST` 的 6 字节转 hex 为小写
（`c77c702ec021`）。会话计数必须统一大小写，否则 SCP socket 关闭时看不到仍在线的
数据通道，延迟清理会把正常连接误判为断开。

认证 MAC 是 HMAC-SHA512 截断到 8 字节。线上 u64 为大端，但 HMAC 输入里的
u64 必须先转成小端：

```text
request_mac = HMAC-SHA512(KSC, client_rc2 || LE64(client_fc))[:8]
response_mac = HMAC-SHA512(
    KSC, client_rc2 || server_rc2 || LE64(server_time)
)[:8]
confirm_mac = HMAC-SHA512(KSC, server_rc2 || LE64(client_fc))[:8]
```

confirm 使用请求里原始的 `client_fc`，不是 `server_time`。KSC 必须来自同一次
SCP `SyncSecure`，不能跨连接复用。

认证帧长度必须精确匹配，不能把 EPI 的“长度超过最小值即接受”规则套到认证帧。

### 4.3 Business interaction

认证完成后，手机可能发送加密 business payload：

```text
interact hello
reconnect interact hello
```

PC 回复加密 `accept`，随后手机才会继续服务和数据总线。

---

## 5. 服务、profile 和 agent

### 5.1 `0x0008 sub=0x01`：profile 列表

PC 的 profile 列表至少包含：

```text
system:streamtransfer
system:filetransfer
app:unite.oplus.onet
app:oshare.win.agent
```

列表里的名字用于后续 profile 选择、注册和 agent 绑定，不是 transport session。

### 5.2 `0x0008 sub=0x02`：agent 注册

注册项里出现 profile 名之前的 2 字节是 agent handle。官方客户端在不同服务
进程里会得到不同的本地 handle：

```text
旧采集 PC：7c88, 18be, 1e98
本次 PC：  54b0, 18be, 6295
手机：     8357, 53f5, 1d3a, cd17
```

这些是 agent handle，不是 session，也不是跨设备全局 ID。两端各自维护自己的
编号空间，必须通过注册帧获知对端 handle。PC 端不能复用抓包中的手机 handle
作为本端 handle，也不能假设官方 PC 的 handle 永远不变。

本次两场重连中，PC `54b0/18be/6295` 与手机 `8357/53f5/1d3a/cd17` 均保持稳定；
删除配对也不会重新分配它们。变化的是 transport session。Linux 实现可以为自己
选择固定的本地 handle，但注册帧、`sub=01` 的 initiator 和后续发送必须始终使用
同一套本地值；peer 的 acceptor 应从 `0x0008 sub=02` 解析。

### 5.3 `0x0010`：多通道

`0x0010 sub=0x01` 携带 channel type/name，PC 回 `sub=0x02`：

```text
type(1) status(1) "port;" 
```

它提供额外 channel/listener 端口，不提供 transport sessionId。

---

## 6. OAF session：`0x0004`

### 6.1 `sub=0x01` 精确布局

```text
body:
00 04 01
acceptorId   u16 BE
initiatorId  u16 BE
profile      ASCII, ends with ';'
infoCount    u16 BE
info[]:
  sessionId  u16 BE
  channelId  u16 BE
  qosType    u8
  qosClass   u8
  reserved   u8
```

语义：

- `initiatorId`：发起本端服务连接的 agent。
- `acceptorId`：被请求连接的对端 agent。
- `sessionId`：发起端本地 OAF session。它在整条连接内建立一条可双向复用的
  通路；发起端通常优先用该号发送，但确认后并不限制只有发起端或只有一个方向
  能使用它。

官方 `app:unite.oplus.onet` 示例：

```text
00 04 01 | 83 57 | 54 b0 | "app:unite.oplus.onet;" |
00 01 | 01 23 | 00 68 | 00 01 00
```

含义：手机 acceptor `0x8357`、PC initiator `0x54b0`、PC local session `291`、
channel `104`、qos `(0,1,0)`。agent handle 是本次官方服务进程的值；换成旧抓包
中的同角色 handle 也合法，但必须和本端注册帧一致。

手机反向请求：

```text
00 04 01 | 18 be | 53 f5 | "app:unite.oplus.onet;" |
00 01 | 03 14 | 00 68 | 04 01 00
```

含义：PC acceptor `0x18be`、phone initiator `0x53f5`、phone local session `788`、channel `104`。

### 6.2 `sub=0x02` 精确布局

```text
00 04 02
acceptorId   u16 BE   ; sub=01 的 initiator
initiatorId  u16 BE   ; sub=01 的 acceptor
profile      ASCII, ';'
status       u8
sessionCount u16 BE
sessionId[]  u16 BE
```

官方接受示例：

```text
00 04 02 | 53 f5 | 18 be | "app:unite.oplus.onet;" |
00 | 00 01 | 01 c6
```

`status=0` 表示接受，`session=454` 被确认。

### 6.3 `sub=0x05` 与 `sub=0x06`

- `sub=0x05`：agent/profile bind/ready，无 session list。
- `sub=0x06`：profile 后的 RSA/认证数据，不是 session 分配帧。

它们都不能替代 `sub=01/sub=02`。

### 6.4 手机侧 session 表

手机日志中可直接看到：

```text
PTC.DATATRANS.AFSessionDetails:
  updateRecord accessoryId:22052,sessionId:454

PTC.DATATRANS.SessionManager:
  ChannelToSessionMap add connectionId:..., sessionMap:
  {104=AFSession{mAccessoryId=22052, mId=454, mChannelType=1}}
```

路由键至少包含：

```text
accessoryId + connectionId + channelId -> sessionId
```

业务帧到来时先解码 transport session，再查这个表，最后才进入 DBS/topic。

### 6.5 已知的 session 行为

| 场景 | PC→手机 | 手机→PC |
| --- | ---: | ---: |
| 全新连接 A | `454` | `788` |
| 全新连接 B | `91` | `99` |
| 官方剪贴板成功连接 | `388` | `735` |
| 2026-10-31 最终官方复验（WLAN） | `109` | `591` |
| 普通断开后重连 | `291` | `323` |
| 删除配对后重连（主 unite） | `862` | `441` |
| 删除配对后重连（oshare 辅助） | `817` | 复用已确认 session |
| harness 成功确认 | `702` | 手机方向另有本地值 |
| harness 早期失败 | `707`（`status=1`） | 手机方向 `45` |
| harness OPD2515 双向剪贴板成功 | `203` | `701` |
| harness PLZ110 双向剪贴板成功 | `211`（oshare `851`） | `242` |

结论：

- session 每业务连接重新分配，不要求两方向相同，也不禁止两方向碰巧相同。
- session 与 agent handle 分离；agent 可以保持稳定，session 变化。
- `0x0998` 只是 session `614` 的 transport 编码，不是固定 opcode。
- session 一旦由某端 `sub=01` 通告并被 `sub=02 status=0` 确认，就进入该连接的
  session 表；后续双向数据可以携带同一个 session。2026-10-31 删除配对重连中，
  手机既在 `0x035e`（862）上发送，也在自己请求的 `0x01b9`（441）上发送。
- 两方向同为 `0x0998` 只是两台端点独立选中了 614，不是协议规定共享一个号。
- `DATA_u8MsgType=52` 是 DBRouter 内部类型，不是 session。
- OPD2515 最终实验中，PC 通告并由手机确认 `203`，手机回发使用手机自己的
  `701`；接收端能同时处理两边已确认的 session，不能强行把 `701` 改成 `203`。

常见“msgId”如果按两字节大端整数打印，需要先解 transport 头，不能直接当 ID：

| 线上 2 字节按 u16 | 10 bit session |
| ---: | ---: |
| `0x01fc` | `127` |
| `0x046c` | `283` |
| `0x0998` | `614` |

### 6.6 明确未知

- session 分配器的具体 PRNG/池算法没有定位到。
- `status=1` 的完整枚举语义未确认；目前只能确认该次连接没有被接受。
- `1..4` reserved session 的全部语义未穷举。
- 两端同时选择同一 session、session 冲突、断线复用时的行为未验证。

### 6.7 官方二进制静态位置

| 功能 | 地址 / 符号 |
| --- | --- |
| transport 头解析 | `pantaChannelService.exe+0x1400d33d0`，`parseTransportHdr` / `parseProtocolParams` |
| transport 头编码 | `0x1400cedb0`，从 session 记录 `+0x38` 取值并 `& 0x3ff` |
| `sub=01` 解析 | `parseCreateServiceRequest` `0x1400bbc70` |
| `sub=01` 处理 | `procSrvConnectionRequest` `0x1400c2c90` |
| `sub=02` 解析 | `parseCreateServiceReply` `0x1400bc160` |
| `sub=02` 处理 | `procSrvConnectionRequestResponse` `0x1400c27d0` |
| 建立 session map | `procSrvConnectionRequestResponseAccept` `0x1400c2530` |

这些是反汇编符号/地址证据；官方客户端没有公开源码注释。

---

## 7. 数据总线与 DBRouter

### 7.1 业务帧布局

业务帧由 transport 头 + 61 字节 DBR 头 + JSON/密文组成：

| 偏移 | 长度 | 字段 |
| ---: | ---: | --- |
| 0 | 2 | OAF transport 头（sessionId） |
| 2 | 1 | DBR `u8MsgType`/flags，数据总线常见 `0x00` |
| 3 | 6 | 发送设备 ID |
| 9 | 8 | DBR delivery sequence / 重放去重键（u64 BE） |
| 17 | 6 | 发送设备 ID |
| 23 | 6 | 对端设备 ID |
| 29 | 32 | route block；`route[0]` 常见 `0x52` |
| 61 | n | JSON 或密文 |

`9..17` 必须作为一个整体的 8 字节值递增，不能拆成独立的 4 字节序号和可随意
保持不变的 4 字节时间戳。官方 OPD2515 连续帧实测为：

```text
000000006ae5b3e0
000000006ae5b3e4
000000006ae5b530
000000006ae5b531
```

它常从接近 UNIX 秒的值起步，随后按消息递增。值本身不是 transport session，
也不是原提示词中所说的 `u16 msgId`。

### 7.2 Transport 编码

```text
sessionId = 10 bit
wire[0] = ((sid >> 6) & 0x0f) | (hasControlHeader << 4) | (version << 5)
wire[1] = ((sid & 0x3f) << 2) | control
```

普通业务帧 version=0、control=0。session 1 可用于明文控制面（例如 `00 04` 是 `0x0004`），普通应用数据应使用 `>4` 的 session。

### 7.3 业务密文

```text
key || iv || aad = HMAC-SHA512(
    KSC,
    client_rc2 || server_rc2 || frame[3..11]
)
key = digest[0..16]   AES-128
iv  = digest[16..28]
aad = digest[28..48]
```

密文是 AES-128-GCM；接收时用 GCM tag 验证，不要只按 JSON 猜测。

### 7.4 Topic

剪贴板使用：

```text
Oplus_MacConnect
```

通知使用：

```text
Oplus_HeySynergy_PcConnect_SyncNotification_Publisher
Oplus_HeySynergy_PcConnect_SyncNotification_Subscriber
```

消息 envelope 的关键字段：

```json
{
  "dbs_msg_topic": {"dbs_topic_name": "...", "dbs_topic_tag": 0},
  "dbs_msg_to_device": "",
  "dbs_msg_priority": 1,
  "dbs_msg_type": "msg_normal_topic_data",
  "dbs_msg_id": "default",
  "dbs_msg_encrypt": true,
  "dbs_msg_payload": "<inner JSON string>"
}
```

`dbs_msg_encrypt` 是 DBS 标志，不代表外层 transport 一定是密文；外层 transport 的 flags 要单独判断。

### 7.5 DBR sequence 重放去重

2026-10-31 的对照实验只改变 sequence 生成方式：

| 实验 | 8 字节 sequence | 手机结果 |
| --- | --- | --- |
| 修复前 | 8 条握手帧全部复用 `000000006ae5ef86` | 只处理第一条 link info；topic sync/pcAbility 等被当重复包静默丢弃 |
| 修复后 | 每帧递增，例如 `...f1fd`、`...f1fe`、`...f1ff`、`...f200` | `handleTopicSync` 出现，后续剪贴板双向成功 |

结论：session 过滤与 DBR sequence 去重是两套独立机制。session 正确但 sequence
重复，帧能通过 session 查表，却仍可能不进入 DBS/topic 分发。

---

## 8. 剪贴板

### 8.1 官方成功路径

官方 PC→手机成功时，手机日志顺序为：

```text
CastDbsReceiver clipboard_data
FileShareEngineService new clipboard pc to phone
SceneDispatcher any execute: 90
ClipBoardServiceManager processPureTextClipboardData
ClipBoardServiceManager onPrimaryClipChanged
```

`EngineProcessor pid:-2 / can't found synergy client` 在成功路径中也会出现，不能把它当成阻断点。

官方手机→PC 成功时，PC 侧顺序为：

```text
ONet_DBRouter DATA_u8MsgType: 52
session=<phone session>
PC DataBus on_topic_received
PC main.log set clipboard data!!
Windows clipboard-win open/set/close
```

官方双向成功证据：

```text
PC  -> phone: WIN-OK-CLIP-20261031-144145                 True
phone -> PC: 吃啥...WIN-OK-CLIP-20261031-144145            True
```

目录：`analysis/clip-success-20261031/`。

### 8.2 2026-10-31 官方客户端最终复验

本轮使用官方客户端重新完整连接，代理 TUN 全程关闭，连接实际走 WLAN：

```text
phone 192.168.2.181:49333 -> PC 192.168.2.233:46999
```

结果：

| 方向 | marker | 结果 |
| --- | --- | --- |
| PC -> 手机 | `OFFICIAL-FINAL-PC-20261031-173131` | `True` |
| 手机 -> PC | `OFFICIAL-FINAL-PHONE-20261031-173230` | `True` |

Windows `Get-Clipboard` 与手机 marker 精确匹配。手机侧 PC -> 手机链路仍是
`CastDbsReceiver -> FileShareEngineService -> SceneDispatcher 90 ->
ClipBoardServiceManager -> onPrimaryClipChanged`，说明 `pid:-2` 警告不是阻断点。

本次 transport：

| 方向 | sessionId | channel | DBR type |
| --- | ---: | ---: | ---: |
| PC -> 手机 | `109` | `104` | `52` |
| 手机 -> PC | `591` | `104` | `52` |

PC→手机发送日志先记录 `localAgentID=21680, channelID=104`，随后
`transport onMessageDispatched sessionId=109`；手机→PC 入站先记录
`onMessageReceived sessionId=0`，再映射为 `transPktToListener sessionId=591`。
这两个号分别由两端独立分配；确认后 session 本身是双向的，但发送实现仍应优先
使用本端在 `sub=01` 中通告并确认的号，不能从历史抓包硬编码。

证据：`analysis/official-final-20261031-172139/`。其中 WLAN pcap 从连接后开始，
覆盖双向剪贴板数据；初次失败阶段与 tunnel 重建阶段的 pcap 单独保留。

### 8.3 当前 harness 状态

harness 已能：

- 用新 PDID 出现在手机“附近的设备”；
- 接住 TCP 10150 的 HTTP SDP QUERY；
- 完成 SCP、UKEY2、KSC、EPI、10/11/12；
- 发送和接收 profile/agent 注册；
- 收到 PC→手机 session 的若干 `status=0` 成功响应；
- 接收并解密手机业务帧；
- 发送带正确 2 字节 transport session 头的 clipboard JSON。

2026-10-31 21:53:54 起使用 OPD2515 做了最终双向真机验收。连接参数与结果：

| 字段 | 值 |
| --- | --- |
| 平板 PDID | `E1C0DDE99002` |
| PC 数据端口 | `10150`（SCP 控制） |
| PC local unite session | `203` |
| PC→平板 marker | `TABLET-HARNESS-PC-CLIP-20261031-215354` |
| 平板→PC marker | `TABLET-HARNESS-ANDROID-CLIP-20261031-215354` |
| PC→平板系统剪贴板落地 | `True` |
| 平板→PC 系统剪贴板落地 | `True` |
| `Session Details not present` 次数 | `0` |

关键证据：

- `harness.out.txt:1715`：`accepted SCP business interaction`；
- `harness.out.txt:1746`：`local unite session=203 confirmed`；
- `harness.out.txt:1750`：回复手机 DBR HELLO；
- `phone.log:58221`：手机登记 `accessoryId:30088,sessionId:203`；
- `phone.log:58391`：手机执行 `handleTopicSync`；
- `phone.log:69028/69077/69089/69223`：PC marker 依次经过
  `CastDbsReceiver -> FileShareEngineService -> processPureTextClipboardData
  -> onPrimaryClipChanged`；
- `phone.log:77904/77960`：平板发布自己的 marker 并进入 DBS；
- `harness.out.txt:1981/1982`：harness 解密并识别 43 字节平板 marker；
- `harness.out.txt:1988`：harness 在已确认的本地 session `203` 上发送回应。

平板回发业务帧使用它自己的 session `701`，PC 发包使用 `203`；两者在同一次连接
中并存。实验期间 `Get-Clipboard -Raw` 首次读取到 43 字节 Android marker，因此
该方向为 `True`。采集停止后 Windows 剪贴板被外部内容再次覆盖，不影响实验时刻
的验收结论。

本次 dumpcap 启动失败，`dumpcap.err.txt` 保留错误输出，因此没有 pcapng；结论只
基于同一目录下的 harness/phone 日志与现场系统剪贴板读取。目录：
`analysis/harness-tablet-OPD2515-20261031-215354/`。

同日 22:13:13 又以一加 15T / PLZ110 做了独立复验，使用新 PDID
`E1C0DDE99004`：

| 字段 | 值 |
| --- | --- |
| 设备 build | `PLZ110_17.0.0.107(CN01)` |
| PC unite / oshare session | `211` / `851` |
| 手机 outbound session | `242` |
| PC→手机 marker | `PHONE-HARNESS-PC-CLIP-20261031-221300` |
| 手机→PC marker | `PHONE-HARNESS-Android-CLIP-20261031-221300` |
| PC→手机 | `True` |
| 手机→PC | `True` |
| `Session Details not present` | `0` |

关键日志：

- `harness.out.txt:94`：`local unite session=211 confirmed`；
- `harness.out.txt:121`：`local oshare session=851 confirmed`；
- `phone.log:27320/27353/27457`：PC marker 进入系统剪贴板；
- `phone.log:41374/41479`：手机发布 Android marker；
- `harness.out.txt:289-290`：harness 收到 42 字节 Android marker；
- `phone.log:27304`：`handleNormalTopicData` 分发到 `Oplus_MacConnect`。

这轮 pcapng 正常完成：`3553` 包、`180.02` 秒、数据 TCP stream `8`。证据目录：
`analysis/harness-phone-PLZ110-20261031-221300/`。pcapng SHA-256 前缀为
`E343DF414B4A60F3`。

### 8.4 已确认的单变量结果

最终修复没有改 session、topic、route head 或剪贴板 JSON，只修正了 DBR
`9..17` 的 8 字节 sequence 生成。结果由“手机静默丢弃握手后续帧”变为
`handleTopicSync` 和双向剪贴板均成功。因此不能再把以下任一历史值当作生产常量：

- session `614`、`203`、`701`；
- agent handle；
- endpoint id；
- DBR sequence；
- DBR Unix 时间附近值。

唯一允许固定的协议常量是 topic 名、`DATA_u8MsgType=52` 以及已由实验确认的
route block 形状；其中设备能力相关 topic 集合仍应按对端能力选择。

---

## 9. 通知、文件、投屏

通知同步已实现；投屏已接入认证信令、GUI 启停、H.264 WebRTC 接收和 VA 硬解，解码帧由 GPUI 投屏窗口绘制（窗口比例取自手机实际分辨率），手机真机出画已验证。文件数据面与对应界面入口仍未提供。

### 9.1 通知

Linux 通知同步已实现，并在 PLZ110 上完成真机验证：

- 订阅 `Oplus_HeySynergy_PcConnect_SyncNotification_Publisher`，topic sync 为
  2 pub / 2 sub；PC 先向 Subscriber 发送 Add/Delete/Error 三条启动控制消息，
  手机才会激活通知 publisher；
- 通知位于手机反向 unite DBRouter session，`dbs_msg_type` 为
  `msg_normal_topic_data`、`DATA_u8MsgType: 52`；
- 内层 `dbs_msg_payload` 是 JSON 字符串，`key` 为
  `HeySynergy_Notification_Add`，`value` 是 JSON 字符串；
- 标题和正文取自 `value.contractedChild.contractedTitle` 与
  `contractedContent`；应用图标取自 `headerView.smallIconByteStr` 的 base64 PNG；
- `notify-rust` 在当前 D-Bus 会话显示通知；同一 `notification.key` 更新原
  notification handle，`HeySynergy_Notification_Delete` 关闭并释放；
- 通知工作线程在三秒窗口内合并同一 key 的最新快照并尾部补发，避免最终状态
  丢失和桌面服务的 `ExcessNotificationGeneration` 拒绝；Delete 同时取消待更新快照。

Windows 后端使用 `winrt-toast-reborn`（稳定 tag/group 更新、toast history
关闭），尚未在 Windows 桌面实测。通知点击/回复回传手机尚未实现。

官方 Windows 抓包、会话编号和 toast 现场记录只作为协议依据，保存在
`windows-protocol-evidence/` 与 `analysis/windows-notification-activation-20261101-013518/`。
### 9.2 文件

`system:filetransfer` 和多通道协商已识别；文件数据面仍未完成，GUI 暂不提供文件互传或扫码入口。项目约束禁止用 ADB push/pull 代替协议实现。

### 9.3 投屏

官方投屏是独立链路：

- `phoneCast.exe` 监听 TCP `7236`
- ODMF/ODMF key 与 DBS 的 KSC 不同
- 媒体走 UDP，部分封装在 Wintun IP-in-IP 隧道

Rust `cast/` 提供显式启动、ODMF 认证与加密信令收发；GUI 的 `cast_session.rs` 和 `cast_media.rs` 提供 H.264/H.265 视频接收、GPUI 投屏窗口、Opus 音频播放与键鼠反控。中文输入暂不支持。

2026-11-03 补充（证据见 `analysis/cast-kms-20261102/` 与 `analysis/scp-log-recovery-20261102/`）：

- 7236/7239 的 ODMF 信令是 WebRTC SDP/ICE 的载体。`0x01` 帧格式为
  `0x01 + 13B AAD(ASCII 时间戳) + 12B IV + AES-256-GCM 密文 + 16B tag`；
  32B OdmfKey 可从 `phoneCast.dmp`（0x0bd2b1fe）提取，7236 与 7239 共用。
- 手机投屏：PC 为 offerer、手机 answerer；完整 offer/answer、ICE ufrag/pwd、
  DTLS SHA-256 指纹、`OdmfDataChannel`(id=1)、`sctp-port:5000` 已还原。
  官方日志打印的反控前缀为 24B，不能据此认定完整线上帧只有 24B。
  当前触摸、按键和滚轮载荷及时间戳长度见下文；反控走 DTLS/SCTP。
- ExpandCast：PC 为 offerer、平板 answerer；H.264 PT100
  (`profile-level-id=42e01f;packetization-mode=1`) + RS-FEC(49)/RTX(101)，
  `m=application`、`sctp-port:5000`。反向触控为 30B 消息
  （6B 头 + 24B 事件体，type 0x10 TOUCH）走 DataChannel(sid=0)；
  完整 offer/answer 在 `analysis/scp-log-recovery-20261102/expandcast-signaling/`。
- KMS：`key = SHA-256(pwd)[:16]`，AES-128-GCM（无 AAD），24B 帧头 +
  12B nonce + 密文 + 16B tag；平板是 TCP 客户端，PC 监听动态 `socket_port`；
  输入事件使用 Windows VK。
- 结论：协议证据已足够开始 Linux 实现；剩余主要是 WebRTC/媒体/输入/
  虚拟显示工程，以及反控包装层、SCTP PPID、配对/信任行为的少量验证。

#### 手机投屏启动与认证信令（2026-11-03）

`cast/auth.rs`、`cast/mod.rs`、`cast/server.rs`、`cast/control.rs` 实现运行时
身份生成、TCP 分帧与 GCM、双连接生命周期和私有 IPC。仅显式调用时启动，
要求已有认证且确认完成的 Panta 数据通道，不自动发起投屏或反控。

**认证证据闭环**：原始 `01-cast-mirror-wlan.pcapng` 的两条 TCP 连接均由 PC
先发 `OdmfAuthRequest`；手机返回 `OdmfAuthResponse.auth_info`。从对应 dump
中的 AuthInfo 布局取得 SharedKey 后，两条 AuthSuccess 均通过 GCM 验证，
解出的 JSON 包含 base64 `id` 和 `key`；key 等于 AuthInfo 的传输密钥。
用该 key 验证原始手机捕获的 34 条加密 JSON 信令全部成功。

- `odmf.dll` 的 `VerifyClientIdentity`（VA `0x1802ee100`）将 P-256 ECDH
  的原始 32 字节共享秘密直接用于 AES-256-GCM，**没有 SHA-256/HKDF**。
- 手机证明的明文是预先通过 Panta 发布的身份 ID，**不是** TCP 请求中的
  `msg` 数值；必须同时校验 GCM tag 和 ID。
- PC 用共享秘密加密 `{"id":"base64身份","key":"base64传输密钥"}`，
  放入 AuthSuccess。随后 `0x01` 信令使用分发的传输密钥。
- 线上另有 2 字节大端长度前缀。PairRequest 加密，但捕获中的 PairSuccess、
  ping/pong 是明文例外；认证后拒绝明文 SDP/ICE，不能将例外扩大成整体降级。

Linux 实现每次启动生成新的 P-256 密钥、八位十进制身份和 32 字节传输密钥，
不内嵌 dump 密钥或历史身份。AuthSuccess 与普通加密帧使用随机 nonce；
AAD 使用当前 13 位毫秒时间戳。advertisement 的 `timestamp:"28703"` 是保留的
近场协议参数，不是当前时间。新身份、随机 nonce 和动态端口已在本机与一加 15T
的投屏会话中验证；其他设备仍需互通验收。

Panta 追加 `Oplus_Cast` topic 后，先发送带实际监听地址、端口和新公钥的
`rsp_odmf_key`，再发送 `pc_devicetype_supportodmf`、`send_image_quality`、
`send_is_front_scene`。SDP 由 GUI 媒体引擎生成，daemon 只转发；
不发送虚构的 KMS input_service 或统计信息。

私有同用户 UDS 接口均为 POST JSON：

| 路径 | 请求字段 | 行为 |
| --- | --- | --- |
| `/cast/start` | device_id | 绑定动态端口并完成 Panta 通告；返回不表示有画面 |
| `/cast/status` | device_id | 返回 local_addr、status.state、connection_ids、last_error；未启动时地址/status 为 null |
| `/cast/stop` | device_id | 关闭全部 TCP 连接/监听，发送 screen_cast_disconnect，撤销 cast topic |
| `/cast/send` | device_id、connection_id、payload（JSON 对象） | 将内层信令封装为 cmd=send/msg=JSON 字符串，加密写入指定 TCP 连接 |
| `/cast/receive` | device_id、connection_id、max（可选，1..32） | 非阻塞批量收取；返回 payloads，默认单条模式同时保留 payload |
| `/cast/configure` | device_id、connection_id、width（1..4096）、fps（1..60） | 发送加密 customBroadcast / SET_PARAMETER，height=0 保持比例，rotatesWithContent/foldRotatesWithContent=true |

接收结果的 payload 为 `{"kind":"signaling","payload":内层JSON}` 或
`{"kind":"control","payload":完整ODMF对象}`；control 保留已观察到的
customBroadcast / TimeSyncResult，不代表本实现执行了对应服务。
后端传输状态为 listening/authenticating/signaling_ready/closed；GUI 根据实际解码帧另行显示媒体状态，不能把传输就绪当作出画。
只有完成认证和 PairRequest 的连接才取得可用于发送/接收的 connection_id。
每次 TCP 重连 token 改变，旧请求不跨连接重放；最多两条连接（官方视频/音频链路），
每条收发队列上限64，容纳多网卡桌面的一次 ICE 候选突发；溢出明确关闭该连接，消息不无限积压。握手10秒、写入5秒、
空闲90秒限时。并发写入保留正在读取的半帧；取消、Stop、Panta EOF 均清理连接。

KMS 和 cast 的 topic 集合统一由 Panta 维护：停止其中一个不会撤销另一个。
已实际运行私有临时 UDS + loopback TCP + 真实 Panta 认证的 smoke，独立 Python
P-256/AES-GCM 对端验证双连接认证、双向加密信令、控制消息、明文心跳、错误身份
拒绝、重连旧 token 拒绝、启动公钥轮换、两种停止顺序的 topic 保留、Stop/EOF
关闭连接及端口。这些为信令层验证；当天手机视频、音频和输入验收见下一节。

#### 投屏：视频、音频与反控（2026-11-03）

GUI 已连接设备卡片提供“开始投屏／停止投屏”。桌面进程负责媒体管线，daemon
只传认证及信令，**不通过 JSON IPC 传视频帧**。启动前检查 GStreamer WebRTC、
libnice、DTLS/SRTP/SCTP、VA 解码与 `appsink`，缺失时报错，不静默退回软件解码；
音频与 H.265 的元素只在对应开关启用时检查。构建最低 GStreamer 1.24；
本机实测 1.28.7/core、1.28.6/plugins-bad 和 RX 9070 XT。

每条认证 TCP token 对应独立 WebRTC peer，手机用两条：视频总线与音频总线。
GUI 先发送 `pc_codec`（H264/H265）和 SET_PARAMETER（请求 width=1080、fps=60、
height=0），再转发实际生成的 offer/ICE。请求值不等于实际画面参数，窗口尺寸来自
解码 CAPS。

**offer 形状**（两条连接相同）：`m=video recvonly`（H.264 PT 100 或 H.265 PT 35）、
`m=audio recvonly`（Opus PT 111）、`m=application`（OdmfDataChannel）。
手机按连接分别作答：视频总线回 `video sendonly`、`audio inactive`；音频总线回
`video inactive`、`audio sendonly`。BUNDLE 顺序与 mid（`video0`/`audio1`/`application2`）
保持 webrtcbin 生成值。

**媒体路径**（进程内，无独立输出窗口）：
`webrtcbin → rtph264depay/rtph265depay → h264parse/h265parse → vah264dec/vah265dec
→ vapostproc（显存内缩放）→ capsfilter → appsink`，帧以 BGRA 交给 GPUI 投屏窗口，
用应用自己的标题栏绘制。音频：`rtpopusdepay → opusdec → audioconvert → audioresample
→ autoaudiosink`。抖动缓冲初始 30 ms；丢包请求关键帧并等待恢复。

**窗口与尺寸**：投屏窗口是普通 GPUI 窗口，尺寸 = 视频区（长边上限 1560，按手机
实际分辨率等比换算）+ 标题栏高度；旋转或分辨率变化时重新计算 capsfilter 并
`resize` 窗口。关闭窗口等同于停止投屏（`on_window_closed` 在下一轮 effect 里处理，
避免重入更新 `RootView`）。具体数值：720×1560 是示例手机的实际结果，不再是常量。

**反控（UIBC over DataChannel）**：触摸载荷 34 字节、按键载荷 24 字节、滚轮载荷
31 字节，发送时均追加 8 字节大端毫秒时间戳，线上长度分别为 42/32/39 字节。
手机原生层移除时间戳后交给 Java 解析器；载荷以 `81 03 01 07` 开头，
eventType 为 2（触摸）、0（按键）、3（滚轮）。触摸坐标空间与显示帧尺寸一致，
尾部携带 `width u16/height u16/deviceId u32`，手机再映射到实际屏幕。
已实现：左键点击/拖拽、悬停（`ACTION_HOVER_MOVE`）、滚轮（累积预算、每 16 ms
发 1 单位，避免"一格跳到底"）、键盘（Android keycode + meta，组合键会先补发
Ctrl/Shift/Alt 的按下事件）。节流规则：`MOVE`/`HOVER` 位置未变或 <16 ms 丢弃，
`DOWN`/`UP` 不参与移动节流。曾用松手后 250 ms 的悬停间隔调整手感；
页面卡在拖动中间的已定位原因是 `UP` 被节流丢弃，不能据此认定悬停会取消惯性。
右键不提供长按替代行为；现有证据不足以确认原生右键协议。

**关键约束（实测确认）**：
- 本机音频在补齐 `a=rtpmap:111 opus/48000/2`、
  `a=fmtp:111 minptime=10;sr=48000;stereo=1;useinbandfec=0` 和官方 extmap 1–6 后出声。
  第 4 条为 `odmf-audio-timing`；未逐项隔离验证，不能断言每条都是必要条件。
- H.264 使用 **PT 100、profile-level-id=42e01f、packetization-mode=1、
  level-asymmetry-allowed=1**；H.265 使用 **PT 35、profile-id=1、level-id=120、
  tier-flag=0、tx-mode=SRST**，并声明 nack/nack pli/ccm fir/transport-cc。
- 保留 webrtcbin 生成的 `a=mid`，**不要**改写成数字：`set-local-description` 按 mid
  匹配 transceiver，改写会报“mid 0 not found”。
- 每个 ICE candidate 的内层 JSON 必须带 `id`（= 该 m-line 的实际 mid）和 `label`（= 索引）。
  手机 `OdmfSignalingClient` 缺 `id` 会抛 `JSONException: No value for id` 并丢弃该帧。
- **offer 必须先入队**。`set-local-description` 期间就开始产生候选，若不缓冲，
  候选会排在 offer 前面；会话按条发送时 offer 要等数十条候选才轮到，
  手机 17 秒超时后自行结束（`MEDIA_FAILED` / `Source Release`）。
- 时间同步回包字段必须是 `{"client_request_time":…,"server_response_time":…}`；
  回显请求或改字段名会让手机算出 epoch 级 offset。
- SCTP 与 DTLS 错误必须按实际失败路径处理，不能根据元素名称一概忽略；
  DTLS 也承载媒体的安全协商。音频 peer 缺少 application 段并非已证实结论。
- `connect_local` 回调有创建线程限制；从 GStreamer 工作线程触发会触发 glib
  线程守卫并导致进程 abort。跨线程回调需满足线程安全约束，输入发送集中在管线线程。

**启动时延**：一次真机会话从 start 请求到解码链建立约 0.74 秒，此前约 11 秒。
Panta 主循环直接等待各控制器的命令队列，避免跨设备共享唤醒和空闲读超时造成延迟。
该指标不是首帧实际呈现时间，也不是端到端显示延迟；不可将解码帧计数当作上屏帧率。

**设置页**「投屏」分组：键盘与鼠标、音频两个开关（默认启用，键鼠即时生效、音频下次
投屏生效），编码方式用 `Select` 选择 H.264/H.265（默认 H.265）。设置持久化到
`oppo-connect/cast-settings.json`。

音频与编码偏好在会话启动时快照，重连 peer 沿用同一配置。输入只路由到实际出画且
数据通道就绪的 peer；停用输入、失焦或关闭窗口会释放按键和触摸。队列有界，
输入过载/传输失效会终止会话，避免静默丢失抬起事件。设置通过临时文件原子替换保存。

**已知限制**：中文输入尚未实现，需要进一步验证 IME 提交文本与手机输入通道的对接；
不能将“没有已接入的文本路径”归因为 GPUI 无 IME 能力。右键未实现；
端到端延迟与长时间稳定性未测量。

#### KMS Rust 协议层与平板键鼠共享（2026-11-04）

`kms/` 实现平板键鼠共享的编解码、动态监听、Panta 通告和显式输入发送。
GUI 通过 KDE/Wayland InputCapture + EIS 捕获全局键鼠，并只向协议确认的
tablet（EPI category `10`）提供此入口；手机仍使用独立投屏链路。

24 字节帧头精确布局如下（多字节整数均为大端）：

| 偏移 | 长度 | 字段 |
| --- | --- | --- |
| 0 | 2 | magic `ab 12` |
| 2 | 1 | version `01` |
| 3 | 1 | event type；未知值保留，由业务层分派 |
| 4 | 2 | 加密标志：0 明文、1 AES-GCM |
| 6 | 4 | wire body 长度；加密时包含 nonce 和 tag |
| 10 | 6 | 48-bit sequence |
| 16 | 8 | 毫秒时间戳 |

`Codec::new(password, max_body_len)` 派生密钥并限制 wire body 长度；
`encode` 写入调用方复用的 buffer，每个加密帧生成新随机 nonce；
`decode` 原地解密并返回借用的明文；`read` 接收 TCP 分片/粘包，帧间 EOF 返回
`None`，帧中 EOF 返回错误。解析/认证失败或读取中途取消后应关闭连接，不能扫描
magic 尝试恢复。序号分配、超时、设备身份校验和连接生命周期由上层负责。

官方协议使用空 AAD，**帧头未被 GCM 认证**；body 认证成功不能证明发送者身份
或帧头未被篡改。长度限制在 body 分配前检查；认证失败不返回明文，并清除 payload。

实测：对 `03-kms-tablet-wlan.pcapng` 的 TCP 59705 会话按序号重组、核对重传重叠
字节后，Rust 成功解码 3,775 帧，其中 3,165 帧通过 GCM 认证；重新编码的全部
3,775 帧再由独立 Python AESGCM 实现认证/解码并核对正文、类型、序号和时间戳。
其中 3,747 帧与官方事件日志逐条一致，额外 28 帧为 PC 发出的心跳请求/响应。
此计数不同于 Windows 汇总中的 3,172 加密帧，统计差异尚未归因。

6 项永久回归测试覆盖独立密码学向量、nonce/密文/tag 篡改、错误密码、长度/标志/
版本校验、48-bit 序号边界、TCP 分片/粘包和截断 EOF。不提交抓包密码或进程 dump
中的密钥。离线验证不等同于 Linux 与平板真机互通。

#### KMS 连接控制与 IPC

通过现有私有 UDS HTTP 接口调用，均为 POST，JSON body：

| 路径 | 请求 |
| --- | --- |
| `/kms/start` | `{"device_id":"设备 PDID","width":3840,"height":2160,"direction":1}`；`direction` 范围 1..=4（1=Left, 2=Right, 3=Above, 4=Below） |
| `/kms/status` | `{"device_id":"设备 PDID"}` |
| `/kms/receive` | `{"device_id":"设备 PDID","connection_id":1}`；消费一次 Pad→电脑的跨回事件 |
| `/kms/stop` | `{"device_id":"设备 PDID"}` |

尺寸范围为 1..16384；`direction` 经逆向官方 `screenshare.exe`（`handleArrangePosition`）
证实对应平板相对电脑的方位（1=Left, 2=Right, 3=Above, 4=Below），并在 arrange 中同步发给平板。KMS 启动前
要求已认证的平板 owner；手机或未知设备不会绑定服务。Start 成功表示 listener 已绑定，
且确认的 unite session 已发出追加 `Oplus_KeyMouseLink` 的 topic sync 和
`kms_ability_sync/pc_device_info`，不表示平板已完成连接。响应只含设备、地址和状态，
不含 randomPwd/testData。状态为 listening/handshaking/connected/closed；没有运行的
KMS 服务时 status/local_addr 为 null。

服务绑定 Panta 连接的具体本地地址和动态端口，只接受同一 peer IP；INFO_SYNC
要求 deviceid 一致且 supportEncrypt=1。设备 ID 是明文一致性检查，**不是密码学
身份认证**。完成 resolution 后发送加密 arrange/resolution；心跳 type4/5 为
明文、sequence=0，type100 接受 alive。daemon 不注入本机输入；收到合法的
Pad→电脑 type-2 pointer cross-back 后只保留最新一次
跨回快照，由 `/kms/receive` 按 connection_id 消费。握手限时10秒、空闲90秒、
心跳间隔30秒、写入限时5秒；这些是本实现的保守超时策略，不是已定案的官方常量。
心跳 tick 不取消正在读取的半帧。

每个设备只有一个 Panta 控制 owner；有界队列串行处理启停，Start 等通告写完才
回复，取消的请求不会遗留服务。Stop 关闭 listener/连接并恢复原 topic 列表；
Panta EOF/任务取消会销毁 owner 和 KMS 服务。不虚构未确认的远端停止 opcode。

本轮在私有临时 UDS 与 loopback TCP 上跑了真实 Panta 认证/确认/加密 HELLO，
验证离线启动拒绝、端口通告及 testData、独立 Python 解密 arrange/resolution、
心跳 echo、重复启动拒绝、停止后的 EOF/端口关闭/topic 撤销、重启密码轮换和
Panta EOF 清理。此验证未连接真机，不能替代平板互通验收。

#### KMS 输入、释放与自由移入移出

先调用 `/kms/status`，仅在 connected 状态下读取非空 `connection_id`。
它标识一次成功的 KMS TCP 握手，不是 Panta session ID；每次重连或重启都改变。
POST `/kms/input` 请求：

```json
{"device_id":"设备 PDID","connection_id":1,"event":{"kind":"key","key_code":79,"modifier":0,"pressed":true}}
```

`event` 支持以下协议原生字段；不自动转换 Linux evdev/XKB 键码：

| kind | 字段 | 外层 event type / body type |
| --- | --- | --- |
| move | delta_x、delta_y（i32） | 3 / 0 |
| key | key_code（非零 Windows VK u8）、modifier（u32）、pressed | 3 / 3 |
| button | button_type（0左、1右、2中、6后退、8前进）、pressed | 3 / 1 |
| scroll | delta_x、delta_y（i32） | 3 / 5 |
| enter | pct_x、pct_y（有限浮点数，边缘穿越允许超过1） | 2 / 6 |

所有输入使用 platform=2 并加密发送。button 的 clickCount=0，支持 0(左)、1(右)、
2(中)、6(后退) 与 8(前进)；scroll 的 isContinuous=false、momentumPhase/scrollPhase=-1；
enter 的 left_key=0。这些值通过逆向官方 `screenshare.exe`（0x1400fabe0）确认。

POST `/kms/release` 使用 `{"device_id":"设备 PDID","connection_id":1}`，
为成功发出的、仍按下的键和按钮补发释放事件。重复 key-down 仅保留一份按下状态。
正常 Stop 也会尝试释放后关闭；掉线、进程退出/强制取消或写入失败时**不能保证远端
收到 key-up**，只清除本地状态并关闭连接，绝不在新连接补发旧输入。

输入经过有界队列，成功响应表示实际写入完成，不表示平板已经应用。旧 connection_id、
未连接、队列满或写失败返回错误；重连前残留输入被拒绝。输入发送和心跳不会取消
正在读取的半帧。按下状态采用固定大小存储，不记录输入历史。

GUI 默认从电脑左侧移出，设置页提供向左、向右两种方位。左侧移出发送 `pctX=1`，
右侧移出发送 `pctX=0`，`pctY` 保持电脑光标高度。Pad 回传的 pointer 坐标按 Pad
握手分辨率归一化，GUI 以对应边缘固定 7 像素内缩位置释放电脑光标，随后重新等待下一次
边缘激活。键盘事件（包括 Escape）在指针位于 Pad 时全部发送给 Pad，不会被本机拦截；
回移只由 Pad 的 cross-back 事件触发。

逆向官方 `screenshare.exe` 表明，键鼠共享下鼠标钩子仅激活了左右边缘（`setCrossEdge: 3`，
即 `1|2` 掩码）；官方 `Above/Below` 仅用于扩展屏（虚拟显示器）。实测表明向 Pad 发送垂直进入
坐标时，平板端驱动会因缺少垂直迟滞而在 20 毫秒内反弹回传，引发两端边缘高频震荡。因此键鼠共享
仅保留左右两个可靠方向。

历史隔离 smoke 覆盖真实 IPC 与独立 Python GCM 解码；此次新增真实 TCP 回归测试
覆盖跨回快照一次性消费、Enter 清除旧跨回和重连后拒绝旧 connection 标识。
初版平板真机已验证 KMS TCP 握手和设备类型识别；用户反馈光标间歇不出现，已修正
相对边缘坐标及 portal/EIS 激活排序。修订版的双向边缘连续操作仍待真机验收，
不将编译、单元测试或 TCP 已连接等同于 Pad 光标显示正常。

#### ExpandCast 平板扩展屏协议（2026-11-04）

平板扩展屏（PC 桌面推流至平板作为第二屏幕）由 `Oplus_ExpandCast` 承载，
运行逻辑与手机投屏相反（PC 作为视频源与发送端，平板作为全屏渲染与反控端）。

1. **Panta 话题协商**：
   - 话题：`Oplus_ExpandCast`，tag 为 0。
   - PC 启动扩展屏后，依次发送：
     - `pc_general_track`：声明扩展模式（`extMode: 1`）及显卡硬件；
     - `pc_device_info`：携带 `mode: 1`、`connection_type: 16` 及 PC 基础信息。
   - 平板收到后回复 `android_device_info`，携带物理分辨率（如 `{"width":2520,"height":1680}`）。
   - PC 收到平板物理尺寸后，启动 ODMF 监听（固定端口 7239），并回发 `pc_auth_ready`（携带 P-256 ECDH 协商公钥）。

2. **WebRTC 媒体与推流**：
   - 平板连接 PC 7239 端口完成基于 ECDH 的身份认证，建立 WebRTC PeerConnection。
   - PC 作为 Offerer，发送 Offer SDP（包含 H.264 PT100 视频轨道与 SCTP DataChannel 5000）；平板回复 Answer SDP。
   - Linux 端通过 PipeWire / Screencast Portal 捕获画面，使用 VA-API 硬件编码器（`vah264enc`）向平板实时推流，目标码率 10 Mbps。
   - 平板端的触控与手写笔操作通过 DataChannel 实时反传（采用与投屏反控相同的 24 字节帧格式）。

---

## 10. Windows harness

当前入口：

```text
oplus-connect-daemon/src/bin/msgid_inject.rs
```

典型命令：

```powershell
cargo build --bin msgid_inject
target\debug\msgid_inject.exe `
  --send "PC-HARNESS-CLIP-<time>" `
  --csp 10150 `
  --pdid e1c0dde99002 `
  --log-dir ..\analysis\harness-<time> `
  --wait-secs 1800
```

不传 `--session` 时每次连接随机分配；`--session N` 只保留给单变量实验，不能
把某次成功日志里的 N 变成生产常量。

它当前监听：

```text
SCP: 10150, 10152, 10155
DATA: 46888, 46999, 47888, 47999, 48999, 48888
```

它会在 SCP Sync 的 `custom_data` 中通告 `48999`，并处理手机发到控制端口的 HTTP SDP QUERY。

### 10.1 网络安全警告

官方 `O+Connect Service` 启动后会创建 `vgate0/utun8`。实测这些接口可能：

- 以 metric 5 的优先级出现；
- 打开 IP forwarding；
- 抢占 Windows 默认路由/DNS/NAT，导致整个系统没网。

另一个已实测的冲突来源是第三方代理 TUN（本轮为 `Sparkle` / Meta Tunnel）。
代理 TUN 开启时，手机仍持续向 `172.31.200.173:10150` 发 SDP QUERY，但
Windows 侧没有 ANNOUNCE 回复，官方客户端设备列表为空；关闭代理 TUN 后，
设备恢复可见并成功连接。该次成功运行时 Windows IPv4 `Forwarding` 仍为
`Enabled`，因此可排除“OPPO 依赖 Windows IP forwarding”的解释。

明确未知：代理 TUN 的具体干扰点是路由优先级、策略路由、WFP/filter 还是
Fake-IP，尚未逐项隔离。当前稳定操作是先关闭代理 TUN，再启用 OPPO 官方 TUN。

不要在未做网络保护的情况下自动启动官方服务。当前脚本 `windows-protocol-evidence/tools/run_harness_with_tunnel.ps1` 只适合实验，执行前必须先记录 WLAN/以太网默认路由，并准备恢复网络。正式实现应：

1. 只保留 172.30/172.31 特定路由；
2. 不使用虚拟接口承载公网默认路由；
3. 退出时恢复 metric、forwarding、DNS 和默认路由。

### 10.2 已确认的测试陷阱

- 官方服务不停止时会占用 10152/46999；当前 daemon/harness 在 ready 前绑定所有端口，任意绑定失败直接退出，避免只拿到部分端口后误判成功。
- 旧 harness 即使 `--wait-secs` 到期，SDP 阻塞线程也可能继续占 UDP 10150/10151；当前实现会取消并等待 SDP 线程退出。历史版本遗留进程仍需在实验前确认。
- 直接用官方 PDID 会被手机缓存成“已连接”，只发 QUERY 不重拨；测试应使用新 PDID。
- 只处理 UDP SDP 不够；手机可能通过 TCP 10150 发 `QUERY * HTTP/1.1`。
- 官方服务重建 `utun8` 时，Npcap 会报 `network adapter "utun8" is no longer attached` 并停止旧 capture；必须在重连后重新对新的 `utun8` 开抓。
- 实际连接可能走 WLAN 而不是 `utun8`：先看 `netstat` 的 established 本地/远端地址，再决定抓哪个接口。
- 只修 session 还不够：DBR 头 `9..17` 的 8 字节 sequence 必须在每个业务帧递增。
  复用值会让接收端按重复包静默丢弃后续 topic sync、能力数据和剪贴板。

---

## 11. 实现伪代码

### 11.1 发现

```text
listen UDP 10150 multicast 239.255.255.250
listen TCP 10150/10152/10155
on UDP QUERY:
    send ANNOUNCE from UDP 10151 to query source
on TCP HTTP QUERY:
    send ANNOUNCE on same TCP connection
```

### 11.2 Session

```text
local_session = random_choice(5..1023)
oshare_session = random_choice(5..1023, != local_session)

send 0x0004 sub=01:
    acceptor = peer_agent
    initiator = local_agent
    profile = "app:unite.oplus.onet;"
    info = (local_session, 104, 0, 1, 0)

on 0x0004 sub=02:
    if status == 0 and local_session in sessions:
        confirmed = true

on 0x0004 sub=01 from peer:
    peer_sessions += request.sessions
    send 0x0004 sub=05 bind
    send 0x0004 sub=02 confirm(status=0, request.sessions)

on_encrypted_business_frame(session):
    accept if session in peer_requested_sessions
           or session == local_session and local_session_confirmed
           or session == oshare_session and oshare_session_confirmed
    otherwise drop as unknown session

send_business(json):
    sequence = monotonically_increasing_u64()
    frame = transport(local_session)
          + dbr_header(sequence, route_head=0x52)
          + json

on_peer_registration(0x0008 sub=02):
    peer_agent = first_handle_before("app:unite.oplus.onet;")
    peer_oshare_agent = first_handle_before("app:oshare.win.agent;")
```

### 11.3 Clipboard

```text
on local Windows clipboard change:
    if session confirmed and clipboard enabled:
        send DBS topic Oplus_MacConnect
        payload key=clipboard_data
        source=windows

on incoming clipboard_data:
    strip transport + 61-byte DBR header
    parse outer JSON
    if dbs_msg_payload is a string:
        parse inner JSON
    write text from data.text.content
```

不要用固定 session 614、手机 session、agent handle 或 `0x52` 代替
`local_session`。本地 agent handle 可以固定并在注册帧中通告，但必须与后续
`sub=01 initiator` 完全一致；peer handle 必须从对端注册帧学习。

---

## 12. 明确未知

- session 分配器的具体算法。
- 所有 `status` 枚举值；`status=0` 已知接受，`status=1` 只知本次未接受。
- 两端同时选同一 session 的冲突处理。
- `1..4` reserved session 的完整语义。
- `OAFP_M...` endpoint id 在官方服务进程重启后是否变化。
- 官方 agent handle 的分配算法及服务进程重启后的稳定性。
- 手机 `AccessoryID` 与各 SDK `connectionID` 的完整映射公式。
- TCP 10150 SDP QUERY 的完整终止/复用行为。
- 文件传输和通知点击/清除的完整数据面。
- Wintun 路由的安全恢复策略。
- 第三方代理 TUN 干扰 OPPO TUN 的具体网络层机制。
- DBR `9..17` sequence 的官方初始 seed/位宽语义；目前只确认必须每帧递增。

---

## 13. 证据索引

| 目录/文件 | 内容 |
| --- | --- |
| `analysis/clip-success-20261031/` | 官方剪贴板双向成功日志、切片、notes |
| `analysis/official-final-20261031-172139/` | 2026-10-31 官方客户端最终双向复验、WLAN/utun8 pcap、手机日志、PC daemon 切片 |
| `analysis/official-reconnect-20261031-173856/` | 普通重连与删除配对重连的三阶段 pcap、逐帧解析、手机日志和动态参数证据 |
| `analysis/official-tablet-OPD2515-20261031-190419/` | OPD2515 平板官方连接、逐帧解析、双向剪贴板成功日志与 daemon 切片 |
| `analysis/msgid-live-20261030/` | session、0x0004、pcap、handshake 逐帧证据 |
| `analysis/harness-tablet-OPD2515-20261031-215354/` | Rust harness 在 OPD2515 上的最终双向剪贴板真机验收、sequence 修复后日志 |
| `analysis/harness-phone-PLZ110-20261031-221300/` | Rust harness 在一加 15T / PLZ110 上的双向剪贴板、完整 WLAN pcapng 与数据 stream |
| `analysis/harness-20261031-165232/` | 历史 Rust harness 连接、0x0004、status=1 证据 |
| `windows-protocol-evidence/` | 官方二进制分析、pcap、方向流、工具 |
| `windows-protocol-evidence/tools/` | 当前保留的分析工具和 Windows 网络/服务脚本 |

原始日志、pcapng、二进制和完整内存不放入本文；验证只保留布尔结果和 SHA-256 前缀。

---

## 14. 项目硬约束

**禁止任何 adb 兜底实现。** 投屏、文件管理、剪贴板、通知一律走官方私有协议（SCP + OAF/DBRouter 数据通道），不使用 `scrcpy`、`adb push/pull`、`adb shell` 读写剪贴板等替代方案。

这条约束对后续所有会话有效。

---

## 15. 重连与删除配对后的动态参数对比

### 15.1 采集条件

2026-10-31 17:38:56 开始保留同一台 Windows、同一台 PLZ110、同一个官方服务
进程，连续覆盖三个阶段：

| 时间 | 阶段 | 结果 |
| --- | --- | --- |
| 17:38:56 | 原有连接继续运行 | 正常心跳和业务数据 |
| 17:39:17 | 主动断开 | 原 TCP 会话结束 |
| 17:39:26 | 普通重新连接 | 建立后约 8 秒被 RST |
| 17:39:43 | 删除配对后重新连接 | 重新握手并稳定运行 |

证据：

| 文件 | 内容 |
| --- | --- |
| `analysis/official-reconnect-20261031-173856/reconnect-wlan.pcapng` | WLAN 全量三阶段抓包 |
| `analysis/official-reconnect-20261031-173856/stream13.follow.txt` | 普通重连的 TCP 46999 双向流 |
| `analysis/official-reconnect-20261031-173856/stream36.follow.txt` | 删除配对后重连的双向流 |
| `analysis/official-reconnect-20261031-173856/stream13.follow.frames.json` | 普通重连逐帧解析 |
| `analysis/official-reconnect-20261031-173856/stream36.follow.frames.json` | 删除配对重连逐帧解析 |
| `analysis/official-reconnect-20261031-173856/panta-slice.log` | Windows 官方 daemon 同期日志 |
| `analysis/official-reconnect-20261031-173856/phone.log` | 手机完整 logcat |

TCP stream 13 为普通重连；stream 36 为删除配对后重连。两段都从 EPI offer 开始，
因此表中不是从旧日志推测的值。

### 15.2 业务 session

| 阶段 | 本端 | Windows 侧 `localAgentID` | Windows `connectionID` | session | transport wire |
| --- | --- | --- | --- | ---: | --- |
| 普通重连 | PC unite | `0x54b0` / 21680 | `116618372875095` | `291` | `04 8c` |
| 普通重连 | 手机 unite（反向连接） | `0x18be` / 6334 | `116617367147509` | `323` | `05 0c` |
| 普通重连 | PC oshare | `0x6295` / 25237 | `116618605960506` | `552` | `08 a0` |
| 删除配对 | PC unite | `0x54b0` / 21680 | `116854596076375` | `862` | `0d 78` |
| 删除配对 | 手机 unite（反向连接） | `0x18be` / 6334 | `116853590348789` | `441` | `06 e4` |
| 删除配对 | PC oshare | `0x6295` / 25237 | `116854829161786` | `817` | `0c c4` |

关键日志行：

- 普通重连 PC session `291`：`panta-slice.log:2051-2064`；
  phone map：`phone.log:11271`。
- 普通重连手机 session `323`：`panta-slice.log:2352-2360`；
  phone map：`phone.log:11394`。
- 普通重连 oshare `552`：`panta-slice.log:4539-4551`。
- 删除配对 PC session `862`：`panta-slice.log:8111-8177`；
  phone map：`phone.log:33873`。
- 删除配对手机 session `441`：`panta-slice.log:8488-10440`。
- 删除配对 oshare `817`：`panta-slice.log:10445-10457`。

同一组 `0x0004 sub=01` 的线上字段为：

```text
普通重连 PC:
00 04 01 | 83 57 | 54 b0 | "app:unite.oplus.onet;" |
00 01 | 01 23 | 00 68 | 00 01 00

普通重连 phone:
00 04 01 | 18 be | 53 f5 | "app:unite.oplus.onet;" |
00 01 | 01 43 | 00 68 | 04 01 00

删除配对 PC:
00 04 01 | 83 57 | 54 b0 | "app:unite.oplus.onet;" |
00 01 | 03 5e | 00 68 | 00 01 00

删除配对 phone:
00 04 01 | 18 be | 53 f5 | "app:unite.oplus.onet;" |
00 01 | 01 b9 | 00 68 | 04 01 00
```

因此可确定：

1. `sessionId` 由各自 OAF 端在每次业务连接中独立选择。
2. 它不是 `localAgentID`、`connectionID` 或 channelId 的固定位段。
3. 它不要求两方向相同；相同只是独立随机/分配后的碰撞。
4. `sub=02 status=0` 确认后，该 session 进入整条连接的 session 表。两端都可能
   使用它发送，方向不是路由键。
5. 未确认或未知 session 仍被手机丢弃；此前注入 `777` 的
   `Session Details not present for 777` 结论不变。

### 15.3 agent handle

| 角色 | 旧抓包 PC | 本次 PC | 本次手机 |
| --- | ---: | ---: | ---: |
| unite 第一 handle | `7c88` | `54b0` | `8357` |
| unite 第二 handle | `18be` | `18be` | `53f5` |
| oshare handle | `1e98`（另有旧帧 `20ea`） | `6295` | `1d3a`、`cd17` |

本次普通重连和删除配对重连中：

- PC `54b0/18be/6295` 完全相同。
- 手机 `8357/53f5/1d3a/cd17` 完全相同。
- 删除配对没有重新分配 agent handle。
- 官方 `0x0004 sub=01` 始终是“对端 acceptor、本端 initiator”：
  PC 请求为 `8357 -> 54b0`，手机请求为 `18be -> 53f5`。

agent handle 的编号空间是本端 OAF 服务内部资源，不是 session。不同官方服务
进程可以给同一角色分配不同值，所以不能把抓包中的 peer handle 当成本地值。
Linux 端应：

```text
local_handles = 进程内固定的一组合法 u16
注册帧、sub=01.initiator、后续发送统一使用 local_handles
peer_handles = parse(0x0008 sub=02)
```

本轮 `0x0008 sub=02` 的 PC 注册含 `54b0/18be/6295`；手机注册含
`8357/53f5/1d3a/cd17`。`0x0010` 不携带 agent 或 session，只给出动态监听端口。

### 15.4 认证、配对与网络字段

| 字段 | 普通重连 | 删除配对后 | 结论 |
| --- | --- | --- | --- |
| PC endpoint id | `OAFP_M...E35E8EE8E09083E018` | 同左 | 服务进程内稳定 |
| 手机 endpoint id | `OAFP_BCE3A830D7B61CD21D236F1B0A92D3A` | 同左 | 手机服务进程内稳定 |
| 手机 6 B device id | `c77c702ec021` | 同左 | 稳定 |
| PC 数据帧 source device id | `e1c0dde98ace` | 同左 | 稳定 |
| 手机 auth alias | 旧值 | 新值 | 配对/认证级动态，值不写入文档 |
| 手机 KSC alias | 旧值 | 新值 | 删除配对后重新建立，值不写入文档 |
| phone `AccessoryID` | `15312` | `63131` | 每连接内部动态 |
| 主数据 TCP 端口 | `46999` | `46999` | 本机两次相同，仍应来自发现结果 |
| multichannel 端口 | `54670/54671` | `60384/60385` | 动态，必须使用响应值 |

日志对照：

- 普通重连 endpoint/alias：`phone.log:11124-11145`。
- 删除配对 endpoint/alias：`phone.log:33832-33840`。
- 普通重连 `AccessoryID=15312` 与 device id：`phone.log:11215`。
- 删除配对 `AccessoryID=63131` 与 device id：`phone.log:33897`。

删除配对后 alias 改变，说明 UKEY2/配对材料重新建立；本文不导出、比较或记录
KSC 明文，因此“KSC 字节是否必然不同”不作超出证据的断言。可验证事实是旧别名
不再使用，10/11/12 重新握手，后续业务 MAC/GCM 全部重新派生。

### 15.5 动态/静态矩阵

| 参数 | 生命周期 | 可否硬编码 | Linux 实现 |
| --- | --- | --- | --- |
| PDID / 两端 6 B device id | 设备身份 | 不可写成固定测试值 | 从本机持久身份和对端发现结果取得 |
| EPI endpoint id | 官方服务进程 | 不应按连接重新生成 | 本进程生成一次并复用 |
| transport session | 每业务连接 | 禁止 | 每次连接随机选 `5..1023` |
| `connectionID` | 每个 agent 连接 | 禁止 | 本地分配/保存，不当作 session |
| local agent handle | 本端服务进程 | 可以固定，但必须自洽 | 注册与 `sub=01` 使用同一组值 |
| peer agent handle | 对端服务进程 | 禁止 | 从 `0x0008 sub=02` 解析 |
| channelId `104` | profile 配置 | 当前可固定 | 以请求/注册中的值为准 |
| qos tuple | profile/agent 角色 | 可参考，不应跨角色复用 | initiate 用 `0,1,0`；保留 peer 值 |
| route head `0x52` | DBS 消息类型 | `clipboard_data` 当前可用 | 与 topic/消息类型绑定 |
| TCP 46999/10152/多通道端口 | 连接/启动级 | 禁止绝对硬编码 | 从发现或协商响应取得 |
| rc2/fc/server_time/MAC | 每次认证 | 禁止 | 每连接随机/当前时间/重新计算 |
| tagId、DBR time、序号 | 每帧 | 禁止 | 每帧生成 |

当前 Rust 已据此改为：

- 删除固定 `DEFAULT_APP_SESSION_ID=614`；
- 主业务与 oshare 默认各自随机分配 session；
- endpoint id 在进程内使用 `OnceLock` 复用；
- `app:oshare.win.agent` 的 peer handle 从手机注册帧中学习；
- oshare 本端 handle 统一为注册帧中的 `0x1e98`，不再把手机的 `0x1d3a`
  错当成本端 initiator；
- 接收过滤同时接受 peer session、已确认的 local unite session 和已确认的 local
  oshare session；
- `OPLUS_BUS_SESSION`、`OPLUS_OSHARE_SESSION` 仍保留为测试覆盖入口，生产不用。

### 15.6 明确未知

- 官方 session 分配器使用何种 PRNG、池或冲突重试算法。
- endpoint id 在官方服务真正重启、升级或换网卡后是否仍保持。
- 官方 agent handle 的服务进程内分配算法和完整生命周期。
- 删除配对时 KSC 明文是否必然改变；只验证了 alias 和认证材料重新生成。
- 手机 `AccessoryID`、名称为 `connectionId` 的 58-bit 内部值与其他 SDK
  `connectionID` 之间的完整映射公式。
- 两方向随机选到同一 session 时，session 表是否合并为同一条记录；目前只看到
  两端碰巧都选 614 时数据仍可工作。

### 15.7 可复现解析

```powershell
$tshark = 'C:\Users\YuKongA\Downloads\WiresharkPortable64\App\Wireshark\tshark.exe'
& $tshark -r analysis\official-reconnect-20261031-173856\reconnect-wlan.pcapng `
  -q -z follow,tcp,raw,13 > analysis\official-reconnect-20261031-173856\stream13.follow.txt
& $tshark -r analysis\official-reconnect-20261031-173856\reconnect-wlan.pcapng `
  -q -z follow,tcp,raw,36 > analysis\official-reconnect-20261031-173856\stream36.follow.txt
python windows-protocol-evidence\tools\reconnect_report.py `
  analysis\official-reconnect-20261031-173856\stream13.follow.txt `
  analysis\official-reconnect-20261031-173856\stream36.follow.txt
```

该工具只读 pcap/Follow 文本，不监听网络，不修改服务，也不会导出密钥或完整内存。

---

## 16. OPD2515 平板对照

### 16.1 采集与功能结果

2026-10-31 19:04:19 开始采集 OPD2515，19:05:00 建立官方客户端数据连接。
19:07:06 完成 PC→平板剪贴板，19:07:21 完成平板→PC 剪贴板，两个方向均成功。

| 字段 | 值 |
| --- | --- |
| 型号 / 产品名 | `OPD2515` / `OPPO Pad Mini` |
| 厂商 | `OPPO` |
| Android | `17` |
| build | `OPD2515_17.0.0.104(CN01)` |
| `com.oplus.padconnect` | `17.3.30` |
| 平板 IP | `192.168.2.111` |
| PC IP / 数据端口 | `192.168.2.233` / TCP `46999` |

证据：

- `tablet-reconnect.pcapng`：WLAN + utun8；
- `stream11.follow-final.txt` / `stream11.follow-final.frames.json`：TCP stream 11；
- `panta-slice.log`：官方 Windows daemon；
- `phone.log`：平板全量 logcat。

### 16.2 EPI 与设备身份差异

| 字段 | 一加 15T / PLZ110 | OPPO Pad Mini / OPD2515 | 结论 |
| --- | --- | --- | --- |
| EPI answer 长度 | `104` | `105` | 长度由 TLV 字符串长度决定，不能写死 |
| endpoint id | `OAFP_BCE3...2D3A` | `OAFP_B873...8B3A` | 设备/服务级身份，不同设备不同 |
| tag 6 产品名 | `PLZ110` | `OPD2515` | 设备型号 |
| tag 7 厂商 | `OnePlus` | `OPPO` | 设备厂商 |
| tag 8 friendly name | `一加 15T` | `OPPO Pad Mini` | EPI answer 长度差异的来源 |
| tag 10 | `0x08` | `0x0a` | 设备类别不同：手机 / 平板 |
| tag 12 | 本次为 `0x52` | `0x4e` | 取值不同；字段完整语义仍未定案 |
| 对端 6 B device id | `c77c702ec021` | `20108139a722` | 每台设备独立 |

两段 answer 的 TLV 数量均为 13。因此 `105 - 104 = 1` 来自字符串编码长度，
不是平板增加了新的 EPI 记录。`parse_answer` 已增加 OPD2515 的 105 字节回归测试。

### 16.3 session 与 agent handle

| 角色 | PLZ110 | OPD2515 | 结论 |
| --- | ---: | ---: | --- |
| PC 发起 unite session | `291` | `321` | 每连接重新分配 |
| 对端发起 unite session | `323` | `659` | 每连接重新分配 |
| PC 发起 oshare session | `552` | `462` | 每连接重新分配 |
| PC 本地 handles | `54b0/18be/6295` | 同左 | 同一 Windows 服务进程内稳定 |
| 对端 unite handles | `8357/53f5` | `9cc7/60a0` | 必须从 `0x0008 sub=02` 学习 |
| 对端 oshare handles | `1d3a/cd17` | `b358/6e67` | 必须从 `0x0008 sub=02` 学习 |

session 关键日志：

- PC unite `321`：`panta-slice.log:1036`；平板映射：`phone.log:17358`。
- 平板 unite `659`：`panta-slice.log:1144`；平板映射：`phone.log:17292`。
- PC oshare `462`：`panta-slice.log:3161`；平板映射：`phone.log:18213`。

平板侧 `AccessoryID` 为 `31705`；同一连接在 Windows daemon 中的本地
OAF accessory id 为 `11149`。两者位于不同端的本地编号空间，不能互相当作同一个值。
`connectionID` 也只在各端本地有效，不能跨端比较。

### 16.4 剪贴板实际走法

两个方向都使用：

```text
topic     = Oplus_MacConnect
DBR type  = 0x52 / DATA_u8MsgType=52
message   = msg_normal_topic_data
channelId = 104
```

PC→平板：

- PC daemon 使用 `localAgentID=21680`（`0x54b0`）、connectionID
  `47886011243719`，实际 transport session 为 PC 本地 `321`；
- `panta-slice.log:8861-8990` 显示发布、加密和 `channel(11149 321)` 发送；
- 平板 `phone.log:104511` 收到 `TABLET-PC-CLIP-20261031-1907`；
- 后续 `FileShareEngineService`、`ICDFSceneManager`、`ClipBoardServiceManager`
  写入系统剪贴板并触发 `onPrimaryClipChanged`；
- 本方向仍出现 `EngineProcessor ... pid: -2 ... can't found synergy client`，
  但最终粘贴成功，因此该行不能单独判定失败。

平板→PC：

- 平板在自己的 session `659` 上发送，`Oplus_MacConnect` payload 的
  `source` 为 `pad`；
- `phone.log:113780/113825` 显示 `publishClipboardData` 和
  `DataBusServerImpl publish`；
- PC daemon `panta-slice.log:9187` 在 session `659` 解密并识别
  `DATA_u8MsgType=52`，随后写入 Windows 剪贴板。

这说明发送方使用“自己的已确认 local session”，接收方同时接受双方已确认 session。
不能用对端 session 替换本端发送 session，也不能要求两端 session 同号。

本次平板 logcat 中 `Session Details not present` 出现次数为 `0`。

### 16.5 topic sync 差异

本轮官方 PC 首包 `msg_topic_sync`：

| 设备 | body 长度 | pub | sub |
| --- | ---: | --- | --- |
| PLZ110 | `588` | 2 项 | 2 项 |
| OPD2515 | `884` | 4 项 | 4 项 |

平板额外包含：

```text
Oplus_KeyMouseLink
Oplus_ExpandCast
```

两代设备都包含 `Oplus_MacConnect`。本轮平板剪贴板成功只依赖
`Oplus_MacConnect`，没有证据表明上述两个平板扩展 topic 是剪贴板前置条件；
它们更可能对应键鼠和扩展投屏能力。

剪贴板实验曾使用过 4 pub / 4 sub 的宽 topic 集合，且 OPD2515 官方帧确实是
`884` 字节；这组值不是本次 PLZ110 通知现场的官方值。当前实现使用
`OFFICIAL_WINDOWS_TOPIC_SYNC_JSON`，即上表 PLZ110 现场使用的最小 2 pub / 2 sub：

- `pub`: `Oplus_HeySynergy_PcConnect_SyncNotification_Subscriber`、
  `Oplus_MacConnect`；
- `sub`: `Oplus_HeySynergy_PcConnect_SyncNotification_Publisher`、
  `Oplus_MacConnect`。

该注册下手机在动态 unite session `531` 上正常回发通知。4 / 4 仍可作为此前
OPD2515 剪贴板实验的设备/agent 组合记录，不能再作为 PLZ110 通知实现的精确
官方现场值。

### 16.6 对 Linux 实现的直接结论

```text
on EPI answer:
    trust peer endpoint id / product / deviceCategory from the packet
    do not assume answer length == 104
    deviceCategory 8 = phone, 10 = observed pad

on 0x0008 sub=02:
    learn peer unite acceptor/initiator handles
    learn peer oshare handles
    never reuse PLZ110's 8357/53f5/1d3a/cd17 as constants

on 0x0004:
    allocate a fresh local unite session and local oshare session
    accept peer session only after parsing its sub=01
    accept local session only after sub=02 status=0 confirmation

send clipboard:
    use the sender's confirmed local unite session
    topic = Oplus_MacConnect
    route head = 0x52

receive clipboard:
    accept source=phone, source=pad, or source=windows
    only require data.text.content
```

### 16.7 明确未知

- tag 12 的准确语义，以及其值 `0x52` / `0x4e` 是否只跟设备类别有关。
- 所有平板是否都提供相同的 `9cc7/60a0/b358/6e67` 角色布局；当前只能确认
  OPD2515 这一台，且值本身不能硬编码。
- 平板端 `AccessoryID=31705` 与 Windows 端 `accessoryId=11149` 的完整映射规则。
- 4 pub / 4 sub 的 topic sync 是否足以在平板上真正启用键鼠、扩展投屏等非
  剪贴板能力；目前只验证了协商被接受，未验证对应功能。
- PLZ110 接受 4 / 4 topic sync 且剪贴板成功，但键鼠、扩展投屏等扩展 topic
  是否真正启用未验证；仍不能据此断言所有设备能力等价。

---

## 17. `msgId` / transport session 专项定案与真机验收

本章专门回答原 Windows 调研任务里的四个问题，并把 2026-10-31 的最终成功实验
补齐为可复核的证据链。原任务中的“每条业务帧开头 2 字节 `u16 msgId`”是观察
命名，不是协议字段名。

### 17.1 结论

| 问题 | 定案 |
| --- | --- |
| 开头 2 字节是什么 | OAF transport 头；业务帧 version=0、control=0 时，低 10 bit 是 sessionId |
| session 是否每连接分配 | 是；每个本地 OAF endpoint 在建业务会话时自行分配 |
| 是否由 agent handle/channelId 派生 | 否；它们是独立编号空间 |
| 是否由对端指定 | 否；发起端在自己的 `0x0004 sub=01` 中通告 |
| 两方向是否必须相同 | 否；各自独立分配，可以不同也可以偶然相同 |
| 对端如何得知 | 解析 `0x0004 sub=01` 的 session info list，并以 `sub=02 status=0` 确认 |
| 接收端是否过滤 | 是；未知 session 在 transport/session lookup 阶段直接丢弃 |
| 仅 session 正确是否足够 | 否；DBR `9..17` 的 8 字节 sequence 还必须每帧递增 |

### 17.2 字节解码与三种历史取值

普通业务帧：

```text
wire = u16 BE
session = (wire >> 2) & 0x03ff
control = wire & 0x0003
version = (wire >> 13) & 0x0007

encode:
wire = (session << 2) | control
```

| 观察值 | `wire >> 2` | session | 解释 |
| ---: | ---: | ---: | --- |
| `0x01fc` | `0x007f` | `127` | PC→手机那次连接中某一端分配的本地 session |
| `0x046c` | `0x011b` | `283` | 同一连接另一方向分配的本地 session |
| `0x0998` | `0x0266` | `614` | 某次实验中一端分配到的值 |

`0x0998` 不是 opcode，`0x0004` 也不是“session 0x0004”。`0x0004` 是
create-service 消息类型；session 位于 transport 头，两者处在不同层。

两方向同为 `0x0998` 只表示两端各自独立选中了 `614`。协议没有要求两端相同，
也没有禁止碰撞。生产实现不能把历史会话中的 `0x0998` 当成固定连接号。

### 17.3 赋值点与来源字段

官方 Windows 二进制中已经定位到：

| 位置 | 作用 |
| --- | --- |
| `pantaChannelService.exe+0x1400d33d0` | `parseTransportHdr` / `parseProtocolParams`，从业务帧头还原 session |
| `pantaChannelService.exe+0x1400cedb0` | 编码 transport 头；从 session 记录 `+0x38` 读取值并 `& 0x3ff` |
| `0x1400bbc70` | `parseCreateServiceRequest`，解析 `0x0004 sub=01` |
| `0x1400c2c90` | `procSrvConnectionRequest`，处理并登记请求里的 session |
| `0x1400bc160` | `parseCreateServiceReply`，解析 `0x0004 sub=02` |
| `0x1400c27d0` | `procSrvConnectionRequestResponse`，处理确认结果 |
| `0x1400c2530` | `procSrvConnectionRequestResponseAccept`，建立 session map |

因此发送路径的直接来源字段是“本地 OAF session 记录 `+0x38`”，不是 device id、
agent handle、`channelID=104` 或 `DATA_u8MsgType=52`。上游“哪一行生成初始
session 数字”的分配器函数尚未定位；已确认的是它由本地端点分配，使用低 10 bit，
每业务连接重新取值。

### 17.4 `0x0004` 如何传递 session

`0x0004 sub=01`：

```text
offset 0      3       body tag: 00 04 01
offset 3      2       acceptorId, u16 BE
offset 5      2       initiatorId, u16 BE
offset 7      n       profile ASCII, 以 ';' 结束
offset 7+n    2       infoCount, u16 BE
then, 每项 7 字节:
  +0           2       sessionId, u16 BE
  +2           2       channelId, u16 BE
  +4           1       qosType
  +5           1       qosClass
  +6           1       reserved
```

`initiatorId` 是发起建连的本端 agent，`acceptorId` 是被请求的对端 agent。
`sessionId` 是 initiator 的本地 transport session。

`0x0004 sub=02`：

```text
offset 0      3       body tag: 00 04 02
offset 3      2       acceptorId
offset 5      2       initiatorId
offset 7      n       profile ASCII, 以 ';' 结束
offset 7+n    1       status
offset 8+n    2       sessionCount, u16 BE
then              2 * count      被接受的 sessionId[]
```

只有 `status=0` 且 session 列表包含请求中刚通告的本地 session，发起端才能把
它标记为 usable。最终 OPD2515 harness 实验中：

```text
PC local unite session = 203
harness.out.txt:1746: local unite session=203 confirmed
phone.log:58221: updateRecord accessoryId:30088,sessionId:203
```

下面三个结构不负责传 transport session：

| 帧 | 实际内容 |
| --- | --- |
| `0x0008 sub=02` | profile/agent 注册；2 字节句柄紧跟并位于 profile 字符串之前 |
| `0x0004 sub=05` | agent/profile bind/ready；无 session list |
| `0x0010` | 多通道协商；携带 channel type/status/端口文本，不携带 session |

agent handle 与 session 可同时出现在 `0x0004 sub=01`，但位置不同：
`acceptorId/initiatorId` 在 offset `3..7`；session 在 `';'` 后的 info list。

### 17.5 接收端过滤的实测

“只改 session、不改 marker/route/JSON”的已知对照仍是最直接证据：

| 实验 | transport | 手机行为 |
| --- | --- | --- |
| known | session `614`，wire `09 98` | `onMessageReceived` → `handleNormalTopicData` → `CastDbsReceiver` 收到 marker |
| unknown | session `777`，wire `0c 24` | `Session Details not present for 777`；无 `onMessageReceived`，无 marker |

证据：

- `analysis/msgid-live-20261030/trace-614-777-phone.txt:168`：known session 入站；
- `analysis/msgid-live-20261030/trace-614-777-phone.txt:174`：业务 payload 被分发；
- `analysis/msgid-live-20261030/trace-614-777-phone.txt:318`：unknown session 被拒。

结论：手机不会对未知 transport session 的帧“照常按 JSON 分发”。过滤发生在
DBS/topic/JSON 之前。接收表键至少包含
`accessoryId + connectionId + channelId -> sessionId`。

### 17.6 最终双设备真机双向验收

实验目录：

```text
analysis/harness-tablet-OPD2515-20261031-215354/
```

连接与 marker：

| 字段 | 值 |
| --- | --- |
| 设备 | OPPO Pad Mini / OPD2515 |
| PDID | `E1C0DDE99002` |
| PC→平板 session | `203` |
| 平板→PC session | `701` |
| PC→平板 marker | `TABLET-HARNESS-PC-CLIP-20261031-215354` |
| 平板→PC marker | `TABLET-HARNESS-ANDROID-CLIP-20261031-215354` |
| PC→平板落地 | `True` |
| 平板→PC 落地 | `True` |

PC→平板证据链：

```text
phone.log:69028 CastDbsReceiver 收到 PC marker
phone.log:69077 FileShareEngineService: new clipboard pc to phone
phone.log:69089 processPureTextClipboardData: TABLET-HARNESS-PC-CLIP-...
phone.log:69223 onPrimaryClipChanged: TABLET-HARNESS-PC-CLIP-...
```

平板→PC 证据链：

```text
phone.log:77904 publishClipboardData: TABLET-HARNESS-ANDROID-CLIP-...
phone.log:77960 DataBusServerImpl 发布同一 marker
harness.out.txt:1981 kv Oplus_MacConnect.clipboard_data
harness.out.txt:1982 phone clipboard (43B): TABLET-HARNESS-ANDROID-CLIP-...
```

实验期间首次读取 `Get-Clipboard -Raw` 精确得到 Android marker，故平板→PC
为 `True`。采集停止后该剪贴板又被外部内容覆盖，不影响实验时刻的结果。

同一个连接中，PC 发包使用本地已确认的 `203`，手机回包使用它自己的 `701`：

```text
PC -> tablet: transport session 203
tablet -> PC: transport session 701
```

这同时证明两方向独立分配，以及接收端不能只承认“自己通告过的号”。

一加 15T / PLZ110 随后用以下独立参数复验：

| 字段 | 值 |
| --- | --- |
| build | `PLZ110_17.0.0.107(CN01)` |
| PDID | `E1C0DDE99004` |
| PC unite session | `211` |
| PC oshare session | `851` |
| 手机回发 session | `242` |
| PC→手机 marker | `PHONE-HARNESS-PC-CLIP-20261031-221300` |
| 手机→PC marker | `PHONE-HARNESS-Android-CLIP-20261031-221300` |
| PC→手机 / 手机→PC | `True / True` |

关键证据：

```text
harness.out.txt:94   local unite session=211 confirmed
harness.out.txt:121  local oshare session=851 confirmed
phone.log:27320      CastDbsReceiver 收到 PC marker
phone.log:27353      processPureTextClipboardData: PC marker
phone.log:41374      publishClipboardData: Android marker
phone.log:41479      DataBusServerImpl publish: Android marker
harness.out.txt:289-290  phone clipboard (42B): Android marker
```

本轮 WLAN pcapng 完整生成：`3553` 包、`180.02` 秒、数据 TCP stream `8`。
证据目录为 `analysis/harness-phone-PLZ110-20261031-221300/`。这证明最终实现
并非只适配 OPD2515。

### 17.7 DBR sequence：与 session 独立的第二个阻塞点

`build_data_bus` 的 61 字节头中，`9..17` 是一个 8 字节大端 delivery sequence。
它用于接收端重放去重，不属于 session，也不是原来观察到的 `u16 msgId`。

官方连续帧：

```text
000000006ae5b3e0
000000006ae5b3e4
000000006ae5b530
000000006ae5b531
```

修复前，harness 的 8 条握手帧全部复用：

```text
000000006ae5ef86
```

手机只接受第一条 link-info，topic sync、pcAbility 等后续帧被静默当重复包丢弃。
改为每帧递增后，手机首次出现 `handleTopicSync`：

```text
phone.log:58391 [ONET_DBS_TOPIC] handleTopicSync
```

因此排障顺序必须是：

```text
1. transport session 是否已在本次连接确认
2. DBR sequence 是否在本次连接内严格推进
3. topic sync 是否被接收
4. 再检查 route/JSON/上层服务
```

不能把“手机没打印 JSON 错误”误判为 JSON 完全正确；重复 sequence 会在更早的
DBR 入口静默丢弃。

### 17.8 Linux 端可实现结论

下面是单个 OAF 业务连接的推荐流程，不依赖任何历史会话值：

```text
type Conn:
    local_session: u16
    local_confirmed: bool
    peer_sessions: set<u16>
    next_sequence: u64

on_connect:
    local_session = random_non_reserved_10bit_session()
    local_confirmed = false
    peer_sessions = {}
    next_sequence = max(unix_seconds(), previous_sequence + 1)

send_create_service:
    body = 00 04 01
         + peer_agent_handle_be16
         + local_agent_handle_be16
         + "app:unite.oplus.onet;"
         + u16_be(1)
         + u16_be(local_session)
         + u16_be(104)
         + 00 01 00
    send_control(body)

on_create_service_confirm:
    if status == 0 and local_session in accepted_sessions:
        local_confirmed = true

on_peer_create_service:
    peer_sessions += request.sessions
    send_create_service_bind()
    send_create_service_confirm(status=0,
                                sessions=request.sessions)

accept_business_frame(transport):
    session = decode_session(transport.wire)
    known = session in peer_sessions
            or (session == local_session and local_confirmed)
    if not known:
        drop

send_topic_data(topic, key, value):
    require local_confirmed
    json = normalize_official_envelope(topic, key, value)
    seq = next_sequence
    next_sequence += 1
    frame = transport(u16_be((local_session << 2) | 0))
          + dbr_header(sequence=seq, route_head=0x52)
          + json
    send(frame)

on_peer_clipboard:
    session = lookup_session(transport)
    require session in peer_sessions
    body = strip_transport_and_61_byte_dbr_header(frame)
    outer = json_parse(body)
    inner = json_parse(outer.dbs_msg_payload) if string else outer
    text = json_parse(inner.value).data.text.content
    write_platform_clipboard(text)
```

最低要求：

1. 不硬编码 `0x0998`、`614`、`203`、`701` 或任何抓包中的 session。
2. 不在未收到 `sub=02 status=0` 时发送业务数据。
3. 接收端同时接受本端已确认 session 与对端 `sub=01` 通告的 session。
4. 每帧独立生成递增的 8 字节 DBR sequence。
5. 未知 session 直接丢；DBR sequence 重复也不能进入业务分发。

### 17.9 明确未知

- 官方 session 分配器的 PRNG/池/冲突重试算法没有定位；只确认本地分配和 10 bit
  transport 编码。
- `status=1` 的完整错误枚举未确认。
- reserved session `1..4` 的全部用途未穷举。
- 两端选到同一 session 时，官方 session 表是否合并记录未直接观测。
- DBR 8 字节 sequence 的精确初始化公式和回绕语义未定位；已确认一个有效实现是
  从接近 UNIX 秒的值起，每帧递增。
- `0x0004 sub=02` 是否允许一次确认多个 session，以及后续动态追加规则，尚无多值
  真机对照。
- PLZ110 与 OPD2515 都已接受 4 / 4 topic sync 并完成剪贴板双向，但这些
  topic 所对应的键鼠、扩展投屏等非剪贴板能力仍未逐项验证。
