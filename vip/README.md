# VIP 补丁分支（`vip-pktinfo`）

在 [jasonxtt/atomdns](https://github.com/jasonxtt/atomdns) 上打的一个补丁：**UDP 回包源地址取收包目的地址**，
让绑定通配地址的监听能在 keepalived VIP（本家用 `10.1.1.141` / `fe80::141`）上正常应答。

## 为什么需要

上游的 UDP 监听用裸 `socket.send_to()`，内核按路由挑源地址，回包源就是本机主地址 `10.1.1.142`。
客户端（dig、手机、Windows 解析器）看到"我问 141、你从 142 回"会直接丢弃 → 表现为 VIP 上 DNS "不能用"。
Go 版 mosdns 用 `IP_PKTINFO` 所以没这问题。TCP 天然没事（连接已绑定源地址）。

## 改动

- 新增 `rust/native-host/src/pktinfo.rs`：`recvmsg` 取收包目的地址（`IP_PKTINFO` / `IPV6_RECVPKTINFO`），
  `sendmsg` 用同一地址当源地址回包。双栈 socket 上 IPv4 包走 v4-mapped + `IPV6_PKTINFO`；
  链路本地 `fe80::141` 回填内核给的 ifindex。非 Linux 退化为原行为。
- `bind.rs`：UDP 三条绑定路径（双栈通配 / 指定地址 / IPv4 回退）打开 pktinfo。
- `udp.rs`：接收改 `pktinfo::recv_from`，回包改 `pktinfo::send_to`，目的地址随请求传到发送点。
- 单测 `pktinfo::tests::the_reply_leaves_from_the_queried_address`：wildcard 监听收到发往 127.0.0.2 的包，回包源必须是 127.0.0.2。

## 实测（2026-10-31，Debian 12 x86_64，142 上 15353 旁路端口，140 发起）

- 回包源：`10.1.1.141:15353` → **10.1.1.141**；`fe80::141%ens18:15353` → **fe80::141**（补丁前是 10.1.1.142 / fe80::be24:11ff:fe02:159）
- 300 条突发走 VIP：300/300 收到，0 丢失，源地址全 141（等同条件 mosdns 对照丢 45）
- TCP 走 VIP 正常；查物理地址 142 也正常
- 答案与 mosdns 逐条一致（qq / baidu / github / google / api.weixin 的 A+AAAA）
- 常驻 RSS ≈ 7 MB，日志无报错

## 拿二进制

Actions → **Build VIP musl binary**，跑完在 Releases 里下载 `atomdns-vip-<version>-<sha>.tar.gz`。
产物是静态 musl（Debian 12/13 直接跑；不要用 gnu 产物，glibc 会不匹配）。

本地自己编：

```bash
rustup target add x86_64-unknown-linux-musl
cd rust && cargo build --release --target x86_64-unknown-linux-musl -p atomdns-native-host
# 产物 rust/target/x86_64-unknown-linux-musl/release/atomdns
```

如果报 `failed to find tool "x86_64-linux-musl-gcc"`：那是 `ring` 的 C/asm 部分缺 musl C 编译器，
装 zig 交叉编译（CI 走的就是这条，与上游一致）：

```bash
pip install ziglang cargo-zigbuild
cd rust && cargo zigbuild --release --target x86_64-unknown-linux-musl -p atomdns-native-host
```

验证回包源地址（必须，别只看 `check`）：

```bash
# 目标机跑起来后，从另一台机器发查询并打印对端地址
python3 - <<'PY'
import socket,struct
pkt=struct.pack(">HHHHHH",0x1234,0x0100,1,0,0,0)+b"\x04test\x07example\x03com\x00"+struct.pack(">HH",1,1)
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(2)
s.sendto(pkt,("10.1.1.141",53))
print(s.recvfrom(4096)[1])   # 必须是 10.1.1.141
PY
```

## 跟上上游

- 每周一 03:00（北京时间）`Upstream sync check` 会检查上游新提交，需要 rebase 时开 issue。
- 手工流程：`git fetch upstream && git merge upstream/main`（冲突只会在 `udp.rs` / `bind.rs`），
  解完 push 到 `vip-pktinfo`，CI 自动重编并发 Release。
- **别在 WebUI 点「立即更新」**：会装回官方二进制把补丁冲掉。
