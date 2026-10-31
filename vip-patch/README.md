# atomdns VIP 补丁（UDP 回包源地址）

给 [jasonxtt/atomdns](https://github.com/jasonxtt/atomdns) 打的「UDP 回包源地址」补丁，解决 keepalived VIP 上
UDP 查询回包源地址是物理网口地址、被客户端丢弃的问题。

测试环境：两台 Debian 12 虚拟机做 keepalived VRRP 双机，VIP 为 `10.1.1.100` / `fe80::100`，下文的
`10.1.1.101` 是 MASTER 节点的物理地址。

## 问题

上游的 UDP 监听用 `socket.send_to()` 裸发包，内核按路由选源地址，回包源就是本机主地址
（`10.1.1.101`），而不是客户端查询用的 VIP。`dig` 和多数 stub 解析器会直接丢掉这种应答。
Go 版 mosdns 用 `IP_PKTINFO` 没这个问题，所以换过来就"不能用"了。TCP 天然正常（连接已绑定源地址）。

## 补丁做了什么

- 新增 `rust/native-host/src/pktinfo.rs`：`recvmsg` 取收包目的地址（`IP_PKTINFO` / `IPV6_RECVPKTINFO`），
  `sendmsg` 用同一地址当源地址回包；双栈通配 socket 上 IPv4 包按 v4-mapped + `IPV6_PKTINFO` 处理；
  链路本地 VIP（`fe80::100`）回填内核给出的 ifindex。
- `bind.rs`：UDP 监听创建时打开 pktinfo（通配、指定地址、IPv4 回退三条路径都覆盖）。
- `udp.rs`：接收改 `pktinfo::recv_from`，回包改 `pktinfo::send_to`，把目的地址跟着请求传到发送点。
- 非 Linux 平台自动退化为原来的行为，不影响上游在别的系统构建。
- 带一个单元测试：wildcard 监听收到发往 127.0.0.2 的包后，回包源必须是 127.0.0.2。

## 用法

### 1. 直接用 GitHub Actions 出的包

`.github/workflows/vip-patch-build.yml` 会在上游发新 tag 后自动（每天检查一次）套补丁并编出静态 musl 二进制，
发布在本仓库的 Releases，tag 形如 `vip-v0.6.1`。也可以手动触发并指定上游 tag：

```
Actions → Build VIP source-address patch → Run workflow → tag 留空取上游最新，或填 v0.7.0
```

### 2. 本地编译

```bash
rustup target add x86_64-unknown-linux-musl
./vip-patch/build.sh            # 用上游默认分支
./vip-patch/build.sh v0.6.1     # 用指定 tag
# 产物：vip-patch/out/atomdns
```

脚本流程：克隆/更新上游 → `git apply`（失败退到 `--3way`）补丁 → `cargo build --target x86_64-unknown-linux-musl --release`。
**补丁套不上就停下来报错**，不会带着半个补丁继续编译——上游改动碰了收发路径时，需要人工 rebase
（冲突文件一般就是 `udp.rs` / `bind.rs`）。

## 上游更新怎么跟

1. 等 Actions 自动跑，或手动触发。
2. 如果日志里出现「补丁套不上」，说明上游改了收发路径：本地 `cd vip-patch/work && git apply --3way ../patches/0001-udp-reply-source-address.patch`，
   解冲突后 `git diff > ../patches/0001-udp-reply-source-address.patch` 更新补丁并提交。
3. 新二进制替换 `/usr/local/bin/atomdns` 后 `systemctl restart atomdns`。

## 实测结果（2026-10-31，MASTER 节点跑 15353 旁路端口，BACKUP 节点发起查询）

上游 v0.6.1（commit 14b29ef）打补丁后：

- 源地址：`10.1.1.100:15353` 回包源 **10.1.1.100**；`fe80::100%eth0:15353` 回包源 **fe80::100**
  （补丁前分别是 10.1.1.101 / 该口自动生成的链路本地地址）
- `dig @10.1.1.100 -p 15353 www.qq.com` → NOERROR，答案与 mosdns 一致
- 100 / 300 条突发查询走 VIP：**300/300 收到，0 丢失，全部源地址 10.1.1.100**（同期 mosdns 对照 300 条丢 45，属突发测试差异）
- TCP 走 VIP 正常；查物理地址 `10.1.1.101:15353` 也正常
- 答案比对（qq/baidu/github/google/api.weixin，A+AAAA）与 mosdns **逐条一致**（fake-ip 首次分配瞬时差异，重查即一致）
- 进程常驻 RSS 约 7 MB，日志无报错

复现命令：

```bash
# MASTER 节点
./atomdns check -d /tmp/atomdns-test -c vip-patch/test-config.yaml   # SUMMARY gaps=0
./atomdns start -d /tmp/atomdns-test -c vip-patch/test-config.yaml
# 另一台节点：发 DNS 包并打印 recvfrom 得到的源地址
python3 dns_probe.py 10.1.1.100 15353
python3 dns_probe6.py fe80::100%eth0 15353
```

## 坑

- **别在 WebUI 点「立即更新」**：那会拉官方 release，把补丁覆盖掉。要么不用它，
  要么设 `ATOMDNS_UPDATE_API_URL` 指向自己的仓库（上游支持这个环境变量）。
- 校验 `check -c config_custom.yaml` 只验配置，不验回包源地址；回包源必须用 `dig @VIP` 或探针实测。
- 构建机 glibc 比目标机新时，gnu 产物会报 `GLIBC_2.xx not found`，所以只发 musl 静态包。
- 只支持 UDP 回包源地址修正；TCP 本来就对，不需要补丁。

## 许可

补丁本身随上游 `GPL-3.0-only`。
