# 第三方代码与许可说明

Picoli 的项目代码采用 [GPL-3.0-only](LICENSE)。下列第三方代码保留各自的许可证和版权声明。

## 直接包含并修改的源码

### jpeg-encoder 0.7.1

- 上游：[vstroebel/jpeg-encoder](https://github.com/vstroebel/jpeg-encoder)。
- 本地源码：[vendor/jpeg-encoder](vendor/jpeg-encoder)。
- 用途：JPEG 编码，包括 DCT、量化、采样和码流生成等上游实现。
- 本地修改：渐进式 EOBRUN、按扫描统计的 Huffman 表、可选 AC 系数率失真优化、系数访问接口及回归修复。
- 许可证：`(MIT OR Apache-2.0) AND IJG`。MIT 与 Apache-2.0 可二选一，涉及 IJG 的部分还需遵守 IJG 条款。
- 许可全文：[MIT](vendor/jpeg-encoder/LICENSE-MIT)、[Apache-2.0](vendor/jpeg-encoder/LICENSE-APACHE)、[IJG](vendor/jpeg-encoder/LICENSE-IJG)。
- 修改说明：[PATCHES.md](vendor/jpeg-encoder/PATCHES.md)。

其中部分 DCT 实现由上游从 MozJPEG 等项目移植，并保留了 IJG、libjpeg-turbo 和 MozJPEG 的来源声明，不是 Picoli 从零编写的实现。

### zune-jpeg 0.5.15

- 上游：[etemesi254/zune-image](https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg)。
- 本地源码：[vendor/zune-jpeg](vendor/zune-jpeg)。
- 用途：JPEG 解码，包括熵解码、IDCT、重采样和颜色转换等上游实现。
- 本地修改：截断扫描的严格校验、量化系数访问、亮度及色度块局部重建接口。
- 许可证：`MIT OR Apache-2.0 OR Zlib`，可选择其中一种。
- 许可全文：[MIT](vendor/zune-jpeg/LICENSE-MIT)、[Apache-2.0](vendor/zune-jpeg/LICENSE-APACHE)、[Zlib](vendor/zune-jpeg/LICENSE-ZLIB)。
- 修改说明：[PATCHES.md](vendor/zune-jpeg/PATCHES.md)。

### libdeflater 1.25.2

- 上游：[libdeflater/libdeflater](https://github.com/libdeflater/libdeflater)。
- 本地源码：[vendor/libdeflater](vendor/libdeflater)。
- 用途：为 C 库 libdeflate 提供 Rust 调用接口。
- 本地修改：可失败的压缩器构造函数、WASM 分配回调的溢出与空指针检查，以及内存对齐处理。
- 许可证：`Apache-2.0`。
- 许可全文：[LICENSE](vendor/libdeflater/LICENSE)。
- 修改说明：[PATCHES-PICOLI.md](vendor/libdeflater/PATCHES-PICOLI.md)。

C 编码器 [libdeflate](https://github.com/ebiggers/libdeflate) 通过 `libdeflate-sys` 在构建时引入，用于 PNG 的无损重压缩。DEFLATE 匹配、分块、Huffman 和码流编码来自 libdeflate，并非 Picoli 重写。Rust FFI 包装采用 Apache-2.0，C 库采用 MIT 许可证；C 库许可全文见 [libdeflate-COPYING](vendor/licenses/libdeflate-COPYING)。

## 其他主要编解码依赖

以下依赖通过 Cargo 引入，而不是将其源码复制进 Picoli 的业务代码：

| 项目 | 版本 | 用途 | 许可证 |
| --- | --- | --- | --- |
| [image](https://github.com/image-rs/image) | 0.25.10 | 图像类型、格式读写和基线编码 | MIT OR Apache-2.0 |
| [png](https://github.com/image-rs/image-png) | 0.18.1 | PNG 编解码 | MIT OR Apache-2.0 |
| [miniz_oxide](https://github.com/Frommi/miniz_oxide) | 0.9.1；传递依赖另含 0.8.9 | DEFLATE 编解码 | MIT OR Zlib OR Apache-2.0 |
| [moxcms](https://github.com/awxkee/moxcms) | 0.8.1 | JPEG ICC 配置解析 | BSD-3-Clause OR Apache-2.0 |

完整依赖和锁定版本见 [Cargo.lock](Cargo.lock)，上表不是全部传递依赖的清单。

## IJG 致谢

This software is based in part on the work of the Independent JPEG Group.

本软件部分基于 Independent JPEG Group（独立 JPEG 工作组）的成果。该声明对应 `jpeg-encoder` 中源自 IJG 的代码。
