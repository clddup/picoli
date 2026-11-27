# 浏览器接入

Picoli 通过 WebAssembly 在本地处理图片。构建步骤见项目 [README](../README.md#webassembly)。生成的模块位于 `web/pkg`，需要通过 HTTP 服务加载，而不是直接打开本地 HTML 文件。

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

`worker.js` 是基线编码的消息协议示例，支持 `capabilities` 和 `baseline-encode`，尚未接入完整压缩接口。使用完整压缩时，在自己的 Worker 中调用 `compress_experimental`，并通过可转移的 `ArrayBuffer` 传递输入和输出。

## Node 验证

`smoke.mjs` 执行 Web 目标的 WASM 绑定，将基线编码结果与原生 `baseline` 报告比较：

```sh
node web/smoke.mjs input.jpg native-baseline/report.json wasm-check.json
```

输出文件必须不存在。这个脚本是 Node 中的 WASM 验证，不是浏览器测试。
