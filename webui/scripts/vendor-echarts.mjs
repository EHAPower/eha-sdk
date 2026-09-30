// Copyright The eha_controller Contributors
// 将 npm 安装的离线图表资源及其许可证复制到静态发布目录。

import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const webuiRoot = resolve(scriptDirectory, "..");
const vendorRoot = resolve(webuiRoot, "static", "vendor");
const files = [
  ["echarts/dist/echarts.min.js", "echarts.min.js"],
  ["echarts/LICENSE", "echarts.LICENSE.txt"],
  ["echarts/NOTICE", "echarts.NOTICE.txt"],
  ["echarts/licenses/LICENSE-d3", "echarts.LICENSE-d3.txt"],
  ["zrender/LICENSE", "zrender.LICENSE.txt"],
  ["tslib/LICENSE.txt", "tslib.LICENSE.txt"],
  ["tslib/CopyrightNotice.txt", "tslib.CopyrightNotice.txt"],
];

await mkdir(vendorRoot, { recursive: true });
for (const [source, destination] of files) {
  const sourcePath = resolve(webuiRoot, "node_modules", source);
  const destinationPath = resolve(vendorRoot, destination);
  if (destination.endsWith(".txt")) {
    const text = await readFile(sourcePath, "utf8");
    await writeFile(destinationPath, `${text.replace(/\r\n/g, "\n").trimEnd()}\n`);
  } else {
    await copyFile(sourcePath, destinationPath);
  }
}
