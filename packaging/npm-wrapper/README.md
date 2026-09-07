# npm 平台二进制 wrapper（M1.7 脚手架）

Stage 1 验收通过后发布。发布流程：

1. **打 tag** `obelisk-v<version>` → `.github/workflows/release-rust.yml` 构建五平台
   artifact（linux x86_64/aarch64、macos x86_64/aarch64、windows x86_64）+ `SHA256SUMS`，
   生成 draft release。
2. **生成平台包**：每个平台一个 npm 包 `@obelisk-apps/cli-<os>-<arch>`，包内只含
   解包后的 `obelisk` 二进制（无 bin 入口、无依赖），版本与 wrapper 一致。
   `packaging/platform-package.mjs`（待验收后编写）从 release artifacts 组装。
3. **发布顺序**：先平台包（全部 5 个），再 wrapper `@obelisk-apps/cli-bin`。
4. **主位切换**：`@obelisk-apps/cli`（TS）保持主位直至 Stage 1 验收；切换时其
   package.json 的 `bin.obelisk` 指向本 wrapper 或 deprecate 并 README 指引。

安装路径（F9：无需 Node 的路径存在）：

- `install.sh --binary`：从 GitHub Releases 直装二进制（curl + sha256 校验）。
- `npm i -g @obelisk-apps/cli-bin`：wrapper + optionalDependencies 平台包。
- 既有 `install.sh`（npm 路径）不变。
