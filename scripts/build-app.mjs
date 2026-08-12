import { execSync } from "node:child_process";
import { join } from "node:path";
import { mkdirSync, copyFileSync } from "node:fs";

const root = join(import.meta.dirname, "..");
const helperSrc = join(root, "src-tauri/helpers/CodexFF.m");
const helperBin = join(root, "src-tauri/target/release/helper/CodexFF");
const bundleApp = join(
  root,
  "src-tauri/target/release/bundle/macos/CodexFF.app",
);

// 编译管理员授权助手 (CodexFF), 打包进 App 的 Resources/helpers
mkdirSync(join(helperBin, ".."), { recursive: true });
execSync(
  `clang -framework Foundation -fobjc-arc -O2 -o "${helperBin}" "${helperSrc}"`,
  { stdio: "inherit" },
);
try {
  execSync(`codesign --force --sign - "${helperBin}"`, { stdio: "inherit" });
} catch {
  // 已签名/签名失败不阻塞编译
}

// 构建前先把 version.txt 同步到 package/tauri/cargo
execSync("npm run sync-version", { stdio: "inherit" });

try {
  execSync("npm run tauri build -- --bundles app", { stdio: "inherit" });
} catch {
  process.exit(1);
}

// 把授权助手放进 bundle 并重新签名 (tauri 打包完成后追加文件会破坏签名)
try {
  const helperDir = join(bundleApp, "Contents/Resources/helpers");
  mkdirSync(helperDir, { recursive: true });
  copyFileSync(helperBin, join(helperDir, "CodexFF"));
  // 先显式用我们的 ad-hoc 签名签主程序 (替换 tauri 打包器签名, 使后续重签字节稳定)
  const exePath = join(bundleApp, "Contents/MacOS/codexff");
  execSync(`codesign --force --sign - "${exePath}"`, { stdio: "inherit" });
  execSync(`codesign --force --sign - "${bundleApp}"`, { stdio: "inherit" });
  console.log("admin helper bundled -> CodexFF.app/Contents/Resources/helpers/CodexFF");
} catch (e) {
  console.error("admin helper bundling failed (non-fatal):", e.message);
}
