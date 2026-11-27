# 浏览器接入

Picoli 通过 WebAssembly 在本地处理图片。构建步骤见项目 [README](../README.md#webassembly)。生成的模块位于 `web/pkg`，需要通过 HTTP 服务加载，而不是直接打开本地 HTML 文件。

## 运行 Demo

先按项目 README 构建 WASM 和绑定，然后在仓库根目录执行：

```sh
cargo fetch --locked
node scripts/build-site.mjs
python3 -I -m http.server 8080 --bind 127.0.0.1 --directory dist
```

访问 `http://127.0.0.1:8080`。页面支持拖放与批量选择、强度调整、原图／结果分界对比、原尺寸查看和单张／ZIP 下载。图片仅在本地 Worker 中处理，不发送到服务器。

页面的内存预算默认为 128 MiB，可手动选择 256、512 或 1024 MiB。文件体积小不代表解码后的工作集小；大尺寸图片可能需要更高预算。高预算模式会增加内存占用，手机或内存不足的设备慎用。预算用于工作集估算，不是浏览器进程的硬上限，也不会改变图片尺寸或压缩强度。

部署由 `.github/workflows/pages.yml` 管理。每次 `main` 更新后，Actions 从同一提交构建核心和页面，将静态产物写入 `gh-pages`，再通过官方 Pages Action 部署。不要手工修改 `gh-pages`；页面底部的提交号指向实际构建版本。

## 压缩接口

在 `web` 目录的 JavaScript 模块中加载绑定：

```js
import init, { compress_experimental } from './pkg/picoli_wasm.js';

await init();

export async function compress(file) {
  const input = new Uint8Array(await file.arrayBuffer());
  return compress_experimental(input);
}
```

`compress_experimental(input, strength_percent?, memory_mib?)` 返回 `Uint8Array`：

- `input`：完整 JPG 或 PNG 文件字节。
- `strength_percent`：可选整数 0–100，默认 50；0 原样返回。
- `memory_mib`：可选整数 16–1024，默认 128。

输出保留输入格式和尺寸。内存不足或格式不支持时，接口抛出错误。内存预算用于估算工作集，不是浏览器进程的硬上限。

## Web Worker

压缩接口是同步的，建议放入 Dedicated Web Worker，避免阻塞页面。当前没有进度回调或协作取消接口；需要取消时终止 Worker。

Demo 使用 `engine-client.js` 和 `compress-worker.js` 调用完整压缩接口，通过可转移的 `ArrayBuffer` 传递输入和输出。文件依次处理，每张完成后释放 Worker；停止操作会直接终止当前 Worker，清空队列也会释放预览 URL。页面显示真实的文件级状态，不显示虚构的百分比进度。

`worker.js` 保留为旧的基线编码协议示例，支持 `capabilities` 和 `baseline-encode`，不是 Demo 使用的压缩入口。

## Node 验证

`smoke.mjs` 执行 Web 目标的 WASM 绑定，将基线编码结果与原生 `baseline` 报告比较：

```sh
node web/smoke.mjs input.jpg native-baseline/report.json wasm-check.json
```

输出文件必须不存在。这个脚本是 Node 中的 WASM 验证，不是浏览器测试。
