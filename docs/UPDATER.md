# 自动更新（tauri-plugin-updater）

Buddy Tauri 版的自动更新基于 **tauri-plugin-updater**，替代 Electron 版的 electron-updater。事件流与原版对齐：check → available / not-available → download（自动开始）→ progress → downloaded → install 重启。

## 一、客户端配置

`src-tauri/tauri.conf.json`：

```json
"plugins": {
  "updater": {
    "active": false,
    "dialog": false,
    "pubkey": "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDcxRUI4QzA4NkNDOTU0NTQKUldSVVZNbHNDSXpyY2VDRWVKTlNBaGJUYjBJcjY4eWtkZkJpOUdnK1hwWjdFUFRTRjdNeXdJdTYK",
    "endpoints": [
      "https://github.com/zhouchang1988/buddy-tauri/releases/latest/download/latest.json"
    ]
  }
}
```

- `active: false` — **当前已禁用自动更新**。原因：`zhouchang1988/buddy-tauri` 是 private 仓库，GitHub 对匿名请求一律返回 404，且仓库尚未发布任何 release（`latest.json` 不存在），每次检查都会以 'Could not fetch a valid release JSON from the remote' 失败。注意：tauri-plugin-updater 2.x 的插件 `Config` 结构并不识别 `active` 字段，该开关由 `src-tauri/src/updater.rs` 自行读取——禁用后不再启动周期检查，菜单中的「检查更新…」入口也会随之隐藏（`menu.rs`），若通过其他途径触发检查命令则返回明确的禁用提示而非网络错误。重新启用：把仓库改为 public（或自建公开更新清单托管并改 `endpoints`）、按 §三 发布首个 release，然后把 `active` 改回 `true`。
- `dialog: false` — 更新 UI 由应用内实现（与 Electron 版一致），不用插件自带对话框
- `pubkey` — minisign 公钥，用于校验更新包签名
- `endpoints` — 指向 GitHub Releases 的 `latest.json`，已配置为 `zhouchang1988/buddy-tauri`。注意：该仓库为 **private**，tauri-plugin-updater 默认无法匿名拉取私有仓库的 release 资产，启用自动更新前需改为 public 或自建更新服务

Rust 端实现见 `src-tauri/src/updater.rs`：启动 5 秒后首次检查，之后每 30 分钟检查一次；发现更新后自动下载（对齐 electron-updater 的 `autoDownload = true`）；所有状态经 `updater:event` 推送给前端 `useUpdater` hook。前端通过 `window.api.checkForUpdates() / downloadUpdate() / installUpdate()` 触发对应 command。

## 二、签名密钥对

密钥对已生成：

| 文件 | 位置 | 说明 |
|------|------|------|
| 私钥 | `~/.tauri/buddy-tauri.key` | **不入库、不提交**。用于发布时签名 |
| 公钥 | `~/.tauri/buddy-tauri.key.pub` | 内容已写入 `tauri.conf.json` 的 `pubkey` |

重新生成（仅在私钥丢失时）：

```bash
pnpm tauri signer generate -w ~/.tauri/buddy-tauri.key
```

⚠️ **私钥丢失无法找回**：必须重新生成密钥对，并把新公钥替换进 `tauri.conf.json` 的 `pubkey` 后发布一个新版本。旧版本客户端只认旧公钥，换钥后旧客户端将无法校验后续更新——这是必须保住私钥的原因。

## 三、发布流程

### 3.1 构建并签名

```bash
export TAURI_SIGNING_PRIVATE_KEY=$(cat ~/.tauri/buddy-tauri.key)
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""   # 生成时若设了密码则填密码

pnpm dist:arm64   # aarch64 (Apple Silicon) 包
pnpm dist:intel   # x86_64 (Intel) 包
```

发布要求同时支持 Apple Silicon 和 Intel Mac，分两个架构各打一个包（通用包体积大、跨架构编译/链接耗时且失败率高，因此默认分架构打包）。前置条件：`rustup target add aarch64-apple-darwin x86_64-apple-darwin`。

设置 `TAURI_SIGNING_PRIVATE_KEY` 后，tauri-cli 会在产出 `.app` / `.dmg` 的同时生成签名（`.sig` 文件）。产物目录按架构区分（arm64 为本机原生构建，落在默认 target 目录；intel 为跨架构构建，落在对应 triple 目录）：

```
src-tauri/target/release/bundle/
├── dmg/Buddy_x.y.z_aarch64.dmg        # arm64 安装包
└── macos/Buddy.app.tar.gz             # arm64 更新包 + .sig

src-tauri/target/x86_64-apple-darwin/release/bundle/
├── dmg/Buddy_x.y.z_x64.dmg            # x86_64 安装包（tauri 命名为 x64）
└── macos/Buddy.app.tar.gz             # x86_64 更新包 + .sig
```

> 如仍需单份通用包（两种架构同一 DMG），可改用 `pnpm dist:universal`（`tauri build --target universal-apple-darwin`），此时产物位于 `src-tauri/target/universal-apple-darwin/release/bundle/`，`latest.json` 两平台条目指向同一 URL。默认不推荐。

### 3.2 生成 latest.json

`latest.json` 是 updater 的元数据清单，指向更新包与其签名。tauri-cli 的 bundle 阶段可自动生成（`bundle > createUpdaterArtifacts`），也可手工按如下格式生成：

```json
{
  "version": "1.2.9",
  "pub_date": "2026-08-05T00:00:00Z",
  "platforms": {
    "darwin-aarch64": {
      "signature": "<Buddy.app.tar.gz.sig 的内容>",
      "url": "https://github.com/<owner>/<repo>/releases/download/v1.2.9/Buddy.app.tar.gz"
    },
    "darwin-x86_64": {
      "signature": "<Buddy.app.tar.gz.sig 的内容>",
      "url": "https://github.com/<owner>/<repo>/releases/download/v1.2.9/Buddy.app.tar.gz"
    }
  }
}
```

分架构打包时，两个平台各上传一份更新包，`latest.json` 中 `darwin-aarch64` 与 `darwin-x86_64` 分别指向各自的 tarball 与签名：

```json
{
  "version": "1.2.9",
  "pub_date": "2026-08-05T00:00:00Z",
  "platforms": {
    "darwin-aarch64": {
      "signature": "<arm64 Buddy.app.tar.gz.sig 的内容>",
      "url": "https://github.com/<owner>/<repo>/releases/download/v1.2.9/Buddy.app.aarch64.tar.gz"
    },
    "darwin-x86_64": {
      "signature": "<x86_64 Buddy.app.tar.gz.sig 的内容>",
      "url": "https://github.com/<owner>/<repo>/releases/download/v1.2.9/Buddy.app.x86_64.tar.gz"
    }
  }
}
```

> 若采用单份通用包，则两平台条目指向同一 URL、同一签名即可（见 3.1）。

`url` 必须与实际上传到 GitHub Release 的资产地址一致（上传时对两架构 tarball 重命名以区分，如 `Buddy.app.aarch64.tar.gz` / `Buddy.app.x86_64.tar.gz`）。

### 3.3 上传 GitHub Releases

1. 打 tag（如 `v1.2.9`）并创建 GitHub Release
2. 上传资产：两架构各上传 `Buddy_x.y.z_aarch64.dmg` 与 `Buddy_x.y.z_x64.dmg`（安装包）、各自 `Buddy.app.tar.gz` + `.sig`（更新包，重命名为 `Buddy.app.aarch64.tar.gz` / `Buddy.app.x86_64.tar.gz` 以区分）、`latest.json`
3. 客户端下次检查时请求 endpoint 的 `latest.json`，按本机架构命中对应条目，比对版本、下载更新包、用 `pubkey` 校验签名后提示安装

也可用 `gh release upload <tag> <files...>` 脚本化上传。

## 四、与 Electron 版的对照

| 项 | Electron 版 | Tauri 版 |
|----|-------------|----------|
| 更新器 | electron-updater | tauri-plugin-updater |
| 元数据 | `latest-mac.yml` | `latest.json` |
| 签名 | 依赖 Apple 代码签名 | minisign 密钥对（独立于 Apple 签名） |
| 更新包 | `*-mac.zip` | `Buddy.app.tar.gz` + `.sig` |
| 事件 | `updater:event`（同名） | `updater:event`（payload 形状保持一致） |

注意：tauri 的签名校验**不替代** Apple 代码签名/公证；DMG 分发时的 Gatekeeper 信任仍需 Developer ID 签名与公证（本仓库当前为无签名内部构建）。
