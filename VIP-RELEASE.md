# VIP 版本的自动发布与 WebUI 在线更新

上游 atomdns 自带「检查更新 / 立即更新」：拉一份 release 列表 → 下载清单和包 → 校验 sha256 →
把候选程序当子进程试跑 → 健康检查通过才落地，不通过自动回滚。本仓库只是给它换了一个更新源
（我们自己的 release），不改一行 Rust 代码。

## 发版

GitHub → Actions → **Publish VIP release (WebUI-updatable)** → Run workflow → 填版本号。

版本号硬性要求（updater 里写死的规则）：

- 只能是 `vX.Y.Z` 纯数字三段，`atomdns-vip-*` 这类 tag 会被直接忽略；
- 必须**严格大于**目标机当前版本，否则 WebUI 不提示更新（同一版本重发只能靠接口里的 `force`）；
- 编译时把 `ATOMDNS_BUILD_VERSION` 钉成这个 tag，否则候选程序自报版本对不上，健康检查失败 → 自动回滚。

发出来的资产（名字一个都不能改）：

| 文件 | 作用 |
| --- | --- |
| `atomdns-linux-amd64-musl.tar.gz` | 真正的二进制包（目标机是 musl 构建，优先装这个） |
| `atomdns-linux-amd64.tar.gz` | 必需资产名，内容同上（静态包在 glibc 机器也能跑） |
| `atomdns-webui.tar.gz` | 目标机有 `ui/main` 目录，缺了它整个更新会被拒 |
| `atomdns-update-manifest.json` | sha256 清单 |

发完自动把这一版写进分支根目录的 `releases.json`（GitHub release 列表同构的数组），
目标机就用它查更新，不经过 `api.github.com`，没有匿名限速。

## 清单里为什么写 `required_config_schema: 0`

上游写 6 表示「这次更新要配套的配置包」。配置包会**覆盖** `config_custom.yaml` 和
`sub_config/*.yaml`，那是手改的本机配置，不能给线上 DNS 换掉。写 0 等于声明「纯二进制更新，
配置我自己管」，updater 就会跳过配置包，健康检查也不校验 schema。

## 目标机上要设的东西

1. 更新源（systemd unit，`/etc/systemd/system/atomdns.service` 的 `[Service]` 段）：

   ```
   Environment=ATOMDNS_UPDATE_API_URL=https://raw.githubusercontent.com/zza774886285/atomdns/vip-pktinfo/releases.json
   ```

   改完 `systemctl daemon-reload && systemctl restart atomdns`。

2. 代理（WebUI 设置里的 SOCKS5，字段 `main_proxy`）：`10.1.1.100:7891`。
   GitHub 的 release 下载走它；国内直连 `github.com` 和 `raw.githubusercontent.com` 都不稳。

## 失败了怎么办

更新是事务式的：旧程序、旧配置、旧 UI 存在 `/cus/atomdns/update/txn-*/`，候选不健康自动回滚，
原因写在 `GET /api/v1/system/health` 的 `update.last_error`。

手工救：

```bash
systemctl stop atomdns
# 从 release 重新解包覆盖，或者从 txn 目录取回 previous
systemctl start atomdns
dig @10.1.1.100 www.qq.com      # 回包源必须是 VIP
```

## 注意

- 健康检查只验 HTTP `ready`、版本号、`ui/main` 挂载，**不验 UDP 回包源地址**。装完自己 `dig @VIP` 一次。
- 跟上游同步后（merge 到 `vip-pktinfo`）要重新发一个更大的版本号，不能改旧 release 的包。

## 版本号规则：跟上游 release tag 同名同号

2026-11-01 为了验证 WebUI 更新通道，曾用 `v0.6.2` 这个 tag 发过「上游 0.6.1 代码 + 补丁」的包，
142 / 140 当时装的就是它（自报版本 v0.6.2）。这次提前占用留下的教训：

- 发版 tag 一旦被安装过就不能重发同名（updater 要求严格大于已装版本，`vip-release.yml` 也拒绝已存在 tag）；
- 所以「有没有跟上上游」不能看 release tag，只看 `rust/native-host/Cargo.toml` 的 version，
  它必须等于「已合并的上游 release tag」。

现状：上游 v0.6.2（11-02）、v0.6.3（11-03）连续发布，本仓库直接跟到上游 v0.6.3，
发同名 tag **v0.6.3**，这版起版本号与上游一致（上游 v0.6.4 → 我们 v0.6.4）。

中间曾误发过一个 v0.6.3（内容是上游 0.6.2 代码 + 补丁），因为没有任何机器安装过，
发现上游随后发布真 v0.6.3 后已删除该 tag / release，重新用同号发布对齐版本。
