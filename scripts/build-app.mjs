import { execSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { mkdirSync, copyFileSync } from "node:fs";
import { createHash } from "node:crypto";

const root = join(import.meta.dirname, "..");
const versionFile = join(root, "version.txt");
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

// 构建成功后 patch +1, 下一次构建自动使用新版本
const current = readFileSync(versionFile, "utf8").trim();
const [major, minor, patch] = current.split(".").map((n) => Number(n) || 0);
const next = `${major}.${minor}.${patch + 1}`;
writeFileSync(versionFile, `${next}\n`);
execSync("npm run sync-version", { stdio: "inherit" });
console.log(`next build version -> ${next}`);

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

// 防篡改: 把主程序 SHA-256 写入 Resources/tamper-check.json 并重新签名密封
try {
  const exePath = join(bundleApp, "Contents/MacOS/codexff");
  const exeBuf = readFileSync(exePath);
  const codeHash = codeSha256(exeBuf);
  writeFileSync(
    join(bundleApp, "Contents/Resources/tamper-check.json"),
    JSON.stringify({ code_sha256: codeHash }),
  );
  execSync(`codesign --force --sign - "${bundleApp}"`, { stdio: "inherit" });
  console.log(`tamper-check bundled -> ${codeHash.slice(0, 12)}…`);
} catch (e) {
  console.error("tamper-check bundling failed (non-fatal):", e.message);
}

/** Mach-O 代码区 SHA-256 (排除 LC_CODE_SIGNATURE blob, 重签不影响代码哈希) */
function codeSha256(buf) {
  if (buf.length < 32) throw new Error("not mach-o");
  const le = buf.readUInt32LE(0);
  const be = buf.readUInt32BE(0);
  let is64, bigEndian;
  if (le === 0xfeedfacf) { is64 = true; bigEndian = false; }
  else if (be === 0xfeedfacf) { is64 = true; bigEndian = true; }
  else if (le === 0xfeedface) { is64 = false; bigEndian = false; }
  else if (be === 0xfeedface) { is64 = false; bigEndian = true; }
  else throw new Error("not mach-o");
  const rd = (o) => (bigEndian ? buf.readUInt32BE(o) : buf.readUInt32LE(o));
  const ncmds = rd(16);
  let off = is64 ? 32 : 28;
  for (let i = 0; i < ncmds; i++) {
    if (off + 8 > buf.length) throw new Error("truncated mach-o");
    const cmd = rd(off);
    const size = rd(off + 4);
    if (cmd === 0x1d) {
      const dataoff = rd(off + 8);
      const datasize = rd(off + 12);
      return createHash("sha256")
        .update(buf.subarray(0, dataoff))
        .update(buf.subarray(dataoff + datasize))
        .digest("hex");
    }
    off += size;
  }
  throw new Error("no LC_CODE_SIGNATURE");
}
