# scenario-engine — luya-z 情景引擎的 OpenWrt 移植

把 [luya-z-manager](https://github.com/faying/luya-z-manager) 的情景引擎移植到 MU300/U30Air 的 OpenWrt（mu300_openwrt 安装的系统）上：
**扫描到指定的"家"SSID 就自动关掉本机 Wi-Fi 热点，扫不到就自动开回来**——回到家时手机漫游进家里网络，随身设备不再重复广播。

保留原引擎的防抖与 fail-open 设计，并修复移植验收发现的问题：

- 判定核心是 `vendor/luyaz-policy`（纯决策、零 I/O）：保留 `enter_hits`/`exit_misses` 防抖、信号门限、闪断保护和邻居对照；扫描失败时依据真实 beacon 状态恢复 AP，不能因为软件已是 away 就停止恢复。
- 引擎 `vendor/luyaz-scenario` 的移植：路径 `/data/scenario` → `/etc/scenario`（takeover 标记 → `/tmp/scenario.took-over`）、剥离 web 层（`http_*` 路由换成 `Engine::put_config` / `set_enabled`）、`set_abroad` 落地为纯文件标志（去掉 luyaz-api 依赖）。停用标记优先于 home 重启保留，保证逃生口恢复 AP。
- 扫描连败 3 次尝试 fail-open、开机 bootsafe 修复、动作失败回滚、`touch /etc/scenario/disabled` 逃生口。开 AP 失败后仍会在后续扫描周期重试，即使扫描本身已恢复成功；回滚后的 Wi-Fi 写入重新锁存，保留之后的用户覆盖。
- **开机管理窗口**：服务启动后，在设备 uptime 未满 600 秒期间强制 AP 开启并暂停检测/固定情景，持续更新心跳。窗口结束后恢复正常判断；在已开机 10 分钟以上的设备上重启服务或升级，不会重新计时。
- guard 的强制开 AP 操作不会被误认成用户手动覆盖；接管释放后会重新锁存 Wi-Fi 状态，允许正常回到 home 并关闭 AP。
- **目标硬件事实**：AP vdev 扫描为空，managed vdev 会破坏客户端数据面。引擎保留 **P2P discovery device**（`__p2pdev`）的固件上下文，跨扫描、AP 开关和 procd 重启复用；暂停检测或停止服务才释放。逐次 stop/delete 会在 home/AP-off 每分钟冷启动 WCN，保存的两次 RF 断言均出现在冷启动 CMD_OPEN 后、扫描命令之前。
- **持续流量必须覆盖扫描周期**：独立 MAC 不足以保证已连接客户端持续联网。现场对照为停止扫描时 HTTP 持续成功，启用 managed 扫描接口后持续超时，删除接口也不恢复；discovery 启动、扫描和销毁期间 HTTP 均成功。损坏的 AP 状态需 `wifi down radio0; wifi up radio0` 重建；`pgrep hostapd` 不能当 beacon 证据，应查 `ubus call hostapd.wlan0 get_status` / `iw dev`。
- discovery 所有权保存在 `/tmp/scenario.scan-wdev`，只复用/清理登记且仍属于 phy0 discovery 类型的 wdev，不接管其他服务的设备。home/AP-off 时 WCN 上下文不再逐轮掉电，待机功耗不能沿用旧的逐轮断电假设。
- 普通开 AP 校验失败后，`radio-recover.sh` 在既有 Wi-Fi 锁预算内协调系统 WCN 恢复锁，只做 radio0 的普通 down/up，并以真实 hostapd ENABLED 验证；扫描资源清理失败不会跳过这次恢复。脚本不再写 `rebootwcn`、`rebootmarlin` 或 `poweroff_wcn`，也不重启整机、USB 或蜂窝。
- 自动固件恢复依赖配套 BSP：只有 `/sys/module/wcn_bsp/parameters/reset_dump_safe` 为 `Y`/`1` 时，扫描会话才借用 sysfs `reset_dump` 模式。驱动必须在电源锁内检查上电并建立 carddump 保护，拒绝已断电状态的 runtime MMIO 关闭，并在复位时清除 loopcheck 失败门槛。旧 BSP 只记录一次能力缺失并保留普通扫描/重开，不能声称它能恢复 card-dump 锁死。
- `/tmp/scenario.reset-dump-owner` 保存借用前模式并跨进程重启复用。暂停、停止或卸载只在模式仍属于引擎时恢复原值；检测到操作员改值即交还所有权。内核 `recovery_reboot` 和心跳 guard 的 900 秒合同不变。`reset_dump` 先写驱动内存环形缓冲再复位，并不保证每次转储完整持久化到磁盘；不要通过人为固件断言验收。

## 布局

```
src/                    OpenWrtIo 宿主 + main（心跳看门狗退出给 procd 重启）
vendor/luyaz-policy     判定核心（OpenWrt Wi-Fi key 与真实 AP fail-open）
vendor/luyaz-scenario   情景引擎运行时
vendor/luyaz-wifi       仅复用 WifiLock/lock_at（flock 锁，设备无关）
packaging/              apk post-install / post-upgrade / pre-deinstall 脚本
packaging/rootfs/       OpenWrt 静态载荷：init、guard、恢复脚本、默认配置和 NOTICE
fixtures/               Wi-Fi 扫描解析和电池估算测试夹具
tools/                  apk-tools 下载构建脚本、root-owner 补丁及 GPL 许可
```

根目录 `Cargo.toml` 同时定义主程序和 workspace。打包时在临时目录创建 `/usr/bin/scenario-engine` 并复制编译产物，无需在源码树预留空目录。`docs/` 仅保留本地交接与现场记录，整个目录由 Git 忽略，不包含在发布快照中；测试数据统一放在 `fixtures/`。

## 构建 / 安装

构建主机需要 Rust 1.99 或更新版本、`aarch64-unknown-linux-musl` 标准库、`cargo-zigbuild`、Zig、C 编译器、Meson、Ninja、pkg-config，以及 apk-tools 所需的 OpenSSL、zlib、zstd 开发库。下载和补丁流程使用 `curl`、`tar`、`patch` 和 `shasum`（macOS 自带）。macOS 还需安装 Xcode Command Line Tools；系统依赖可用 Homebrew 安装：

```sh
brew install zig meson ninja pkgconf openssl@3 zlib zstd
cargo install cargo-zigbuild --locked
rustup target add aarch64-unknown-linux-musl
```

克隆本仓库后构建，无需同级的 luya-z-manager 工作区。移植的 Rust 依赖源码保存在 `vendor/`；首次构建需联网下载 Cargo.lock 中的 crates，首次打包还会下载固定版本的 apk-tools 源码：

```sh
git clone https://github.com/InsulateJustf/scenario-engine.git
cd scenario-engine
cargo test --workspace --locked -- --skip lock_takes_both_layers_and_releases_both
```

上面的测试命令适用于缺少外部 `flock` 命令的 macOS；提供兼容 `flock` 命令的 Linux 主机可去掉 `-- --skip ...` 运行全部测试。

```sh
./build-package.sh        # → dist/ 下的 apk 安装包（arch: aarch64_generic）
# 将 /path/to/package.apk 替换为刚构建的安装包路径
ssh mu300 'cat > /tmp/sc.apk' < /path/to/package.apk
ssh mu300 'apk add --allow-untrusted /tmp/sc.apk && /etc/init.d/scenario enabled && ubus call service list '\''{"name":"scenario"}'\'''
```

post-install 会启用开机启动并启动两个 procd 实例；post-upgrade 则重启它们，确保新二进制生效：`engine`（本体，respawn 5 s）和 `guard`（wifiguard：引擎心跳超 900 s 且 AP 关着时强制开 AP 并写 takeover 标记，引擎稳定运行 10 分钟后接管回来）。pre-deinstall 停止服务、取消开机启动并强制 AP 开，清除过时运行状态；即使 init 文件缺失，也会通过 procd 停止实例。用户配置保留在 `/etc/scenario/`，升级和卸载不会覆盖。

构建脚本检查包体积和 ADB 文件/生命周期脚本清单，禁用 macOS 扩展属性；本地 mkpkg 的 `--root-owner` 补丁统一所有文件和目录为 root:root，避免把构建机用户名带到设备。检查通过后才将临时包发布到 `dist/`。

apk-tools 仅用于主机构建，不随 apk 安装到设备，也不将其源码纳入本仓库。`tools/build-apk-tools.sh` 从 [上游 v3.0.5](https://github.com/alpinelinux/apk-tools/tree/v3.0.5) 下载源码压缩包，验证脚本中固定的 SHA-256 后，应用 [root-owner 补丁](tools/apk-tools-root-owner.patch) 并用 Meson/Ninja 构建。下载或校验失败会终止打包。

源码和构建缓存保存在 `target/host-tools/`，目录由上游版本和补丁 SHA-256 决定；后续构建复用缓存，不重复下载。补丁变化会自动使用新目录；需要强制重建时，删除 `target/host-tools/` 中对应的缓存目录。引擎与打包工具均由 `build-package.sh` 自动构建，不需要手动安装 apk-tools。

构建脚本可从任意工作目录调用，使用 `Cargo.lock` 锁定 Rust 依赖；打包在临时目录完成，退出时自动清理。源码缓存与编译产物只写入 `target/` 和 `dist/`，不提交到 Git。apk 内附引擎上游 `NOTICE` 和完整 `LICENSE`；[tools/LICENSE.apk-tools](tools/LICENSE.apk-tools) 单独适用于 apk-tools 及本地补丁。

## 配置

先备份，再在设备上编辑完整的 `/etc/scenario/scenarios.json` 中 `id: "home"` 的 `detect.entries`（下面只是该情景的片段，不要用它替换整个文件）。引擎每轮 tick 按 mtime+大小热重载，无需重启；默认 away 扫描间隔 300 秒、home 60 秒，进入需连续命中 2 次，退出需连续未命中 2 次，强信号闪断保护可能延后退出。

```json
{
  "id": "home", "name": "在家",
  "detect": {
    "type": "ssid",
    "entries": [
      { "ssid": "HOMESSID1" },
      { "ssid": "HOMESSID2" },
      { "ssid": "HOMESSID3" }
    ]
  }
}
```

- 只写 `ssid`：信号 ≥ `min_rssi_dbm`(-70) 才算命中；加 `bssid` 则无视信号强度。
- 同一个 home 情景内的多个 SSID 是“任意一个命中即可”，不需要同时出现；配置项是周围路由器的 SSID，不是设备自己的热点名。上面的三个 SSID 是示例占位名称，使用时替换为自己的家庭网络名称；包内出厂配置仍为空。
- 出厂配置 home 的 `entries` 为空时仍会定期扫描，但不会因检测到家庭网络而关闭 AP。
- 手段速查：`/etc/scenario/pin` 写情景 id 固定之；`touch /etc/scenario/disabled` 逃生（恢复 away 后停用）；`echo $(cut -d. -f1 /proc/uptime) > /tmp/scenario.took-over` 触发看门狗接管分支。

开机管理窗口内可以连接设备热点，禁用自动检测，让 AP 之后也保持开启：

```sh
ssh mu300 'touch /etc/scenario/disabled'
```

这个标记保存在 `/etc/scenario/`，重启后仍有效；开机窗口结束不会替你删除它。恢复检测：

```sh
ssh mu300 'rm -f /etc/scenario/disabled'
```

保护窗口结束且检测未禁用时，附近任意一个 HOME SSID 达到命中条件，设备自己的 AP 就会关闭；手机应使用家庭 Wi-Fi。设备的蜂窝连接不由本引擎开关。

备份（含配置和恢复快照）：

```sh
ssh mu300 'tar -C /etc -czf - scenario' > scenario-backup.tar.gz
```

首次配置建议通过 USB tether 管理；命中家庭 SSID 后设备自己的 AP 会关闭。需要立即恢复时，通过 USB 执行：

```sh
ssh mu300 'touch /etc/scenario/disabled; /etc/init.d/scenario stop; uci set wireless.ap0.disabled=0; uci commit wireless; ubus call network reload; /etc/init.d/scenario start'
```

确认 `ubus call hostapd.wlan0 get_status` 的 `status` 为 `ENABLED`。修改好配置后 `rm /etc/scenario/disabled` 恢复检测。要恢复出厂惰性配置，将 `/usr/share/scenario/scenarios.json.default` 复制回 `/etc/scenario/scenarios.json`，等待引擎切回 away 并确认 AP 开。

## 验证

- `cargo test --workspace --locked -- --skip lock_takes_both_layers_and_releases_both`：219 项通过；仅跳过依赖外部 `flock` 命令的锁测试。运行时使用 Rust `flock(2)`。
- 从仓库之外调用 `build-package.sh`：首次下载、校验、应用补丁并编译 apk-tools 后，成功生成 aarch64 musl 的 apk 安装包；移除旧内置源码后，缓存复用打包也通过。两条路径均通过文件清单、许可、生命周期脚本及 root:root 所有权检查。
- 使用临时下载替身验证：损坏的源码压缩包因 SHA-256 不符而在解压前被拒绝，临时目录清理完成；禁止下载时，已缓存的 apk-tools 仍能完成完整打包。
- 本次仓库整理未重新部署设备；硬件相关行为仍需在适配的 OpenWrt/BSP 上验证。

## 许可

代码来自 [faying/luya-z-manager](https://github.com/faying/luya-z-manager)，采用 [PolyForm-Noncommercial-1.0.0](LICENSE)。许可覆盖非商业用途；再分发须保留 Required Notice，并提供许可全文或其链接。apk-tools 和本地 root-owner 补丁采用 GPL-2.0，许可全文见 [tools/LICENSE.apk-tools](tools/LICENSE.apk-tools)。
