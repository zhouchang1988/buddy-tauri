# 本地签名（解决 macOS 反复弹"想访问文稿文件夹"）

## 问题

Buddy 会对用户选中的仓库目录执行 `git status` 等操作；仓库在 `~/Documents` 下时，
macOS 的 TCC 隐私机制会弹窗「"Buddy"想访问"文稿"文件夹中的文件」。

TCC 把授权绑定在**应用的代码签名**上：

- 正式签名（Developer ID）→ 授权一次，永久有效（Electron 原版即是如此）
- **ad-hoc 签名（本地开发构建的默认行为）→ 每次重新编译哈希都变，macOS 视为新应用，每次都重新弹窗**

## 解决方案

用稳定的**自签名证书**签名，身份在多次构建间保持不变 → 授权一次即可。

本机已配置好：

- 证书：`Buddy Local Dev`（自签名，EKU=Code Signing，有效期 10 年）
- 存放：`~/Library/Keychains/buddy-dev.keychain-db`（密码 `buddy-dev`，已加入用户钥匙串搜索列表）
- p12 备份：`~/.tauri/buddy-local-codesign.p12`（密码 `buddy-local`）

`tauri.conf.json` 已配置 `bundle.macOS.signingIdentity: "Buddy Local Dev"`，
`pnpm tauri build` 会自动签名。如自动签名失败，构建后手动执行：

```bash
sh scripts/sign-local.sh
```

## 在新机器上重新创建（参考）

```bash
# 1. 生成自签名代码签名证书
openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 3650 -nodes \
  -subj "/CN=Buddy Local Dev" \
  -addext "extendedKeyUsage=codeSigning" -addext "keyUsage=digitalSignature"

# 2. 打包 p12（必须用旧算法，否则 macOS 导入报 MAC verification failed）
openssl pkcs12 -export -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES -macalg sha1 \
  -out buddy-local.p12 -inkey key.pem -in cert.pem -passout pass:buddy-local

# 3. 建独立钥匙串并导入（不要直接导入 login 钥匙串——cert/key 跨钥匙串重复会导致
#    codesign errSecInternalComponent；若已污染需按公钥哈希清理 login 里的副本）
security create-keychain -p buddy-dev buddy-dev.keychain-db
security unlock-keychain -p buddy-dev buddy-dev.keychain-db
security set-keychain-settings buddy-dev.keychain-db   # 不自动锁定
security import buddy-local.p12 -k buddy-dev.keychain-db -P buddy-local -A
security list-keychains -d user -s login.keychain-db buddy-dev.keychain-db
security set-key-partition-list -S apple-tool:,apple:,codesign: -k buddy-dev buddy-dev.keychain-db

# 4. 签名
codesign --force --deep --keychain ~/Library/Keychains/buddy-dev.keychain-db \
  --sign "Buddy Local Dev" path/to/Buddy.app
```

注意：`security find-identity -v` 可能仍显示 0（该命令对该自签名的政策校验偏严格），
但以 `codesign --keychain ... --sign "Buddy Local Dev"` 实际可签为准。

## 分发场景

自签名只解决本机 TCC 持久化，**不能**过 Gatekeeper。对外分发仍需 Apple Developer ID
签名 + 公证（此时 TCC 问题自然消失）。
