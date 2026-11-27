# Picoli

本地运行的 JPG 和 PNG 压缩工具。使用 Rust 编写，同一套核心支持原生 CLI 和浏览器 WebAssembly。

**[在线体验 Picoli](https://clddup.github.io/picoli/)**

- 默认保留图片格式和尺寸。
- 透明 PNG 逐像素保留 Alpha，包括完全透明像素的隐藏 RGB。
- 原生 CLI 支持多线程，并根据内存预算调整并发。
- 压缩过程不需要网络，不上传图片。

目前仍在开发中。Web Demo 支持批量选择、压缩前后对比和 ZIP 下载。压缩可能改变颜色和细节，并非所有图片都能进一步减小。

## 构建

需要 Rust stable 和 C 编译器。

```sh
git clone https://github.com/clddup/picoli.git
cd picoli
cargo build --release -p picoli-cli
```

## 使用

```sh
./target/release/picoli optimize photo.jpg result
```

输出目录必须不存在。目录内包含 `selected.jpg` 或 `selected.png`、候选文件和 `report.json`，原图不会被覆盖。没有更小且满足当前误差限制的候选时，保留原文件。

调整压缩强度、线程数和内存预算：

```sh
./target/release/picoli optimize photo.png result \
  --strength 50 --jobs 4 --memory-mib 512
```

| 参数 | 说明 |
| --- | --- |
| `--strength` | 整数 0–100，默认 50。数值越高，允许的失真越大；0 原样返回。 |
| `--jobs` | 请求的工作线程数，默认不超过 4，实际并发受内存预算限制。 |
| `--memory-mib` | 内存预算，范围 64–4096 MiB；省略时根据系统可用内存自动选择。 |

强度不是画质保留百分比，也不表示文件缩小比例。内存预算用于工作集估算，不是进程内存的硬上限。

## WebAssembly

安装 `wasm32-unknown-unknown` 目标和与项目匹配的 `wasm-bindgen-cli`：

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.117 --locked
```

还需要支持 WebAssembly 的 Clang 和 LLVM ar。确认它们已在 PATH 中，然后构建：

```sh
CC_wasm32_unknown_unknown=clang AR_wasm32_unknown_unknown=llvm-ar \
  CFLAGS_wasm32_unknown_unknown="-O3 -msimd128" \
  RUSTFLAGS="-C target-feature=+simd128" \
  cargo build -p picoli-wasm --target wasm32-unknown-unknown --profile wasm-release

wasm-bindgen target/wasm32-unknown-unknown/wasm-release/picoli_wasm.wasm \
  --target web --out-dir web/pkg

wasm-opt -O3 --enable-simd --enable-bulk-memory --enable-nontrapping-float-to-int \
  --enable-sign-ext --enable-mutable-globals --enable-multivalue --enable-reference-types \
  web/pkg/picoli_wasm_bg.wasm -o web/pkg/picoli_wasm_bg.wasm
```

`compress_experimental(input, strength_percent?, memory_mib?)` 接收图片字节并返回压缩后的字节，默认强度为 50；省略 `memory_mib` 时按图片自适应工作集预算。浏览器端建议在 Web Worker 中调用。详见 [浏览器接入](web/README.md)。

## 支持范围

支持 8 位静态 JPG 和 PNG。CMYK、16 位、动画图片及部分色彩或方向元数据暂不支持。JPEG 会保留支持的 EXIF 元数据；如需移除位置信息，请另外处理。

## 开发

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- `crates/picoli-core`：图片分析、候选生成、评分和选择。
- `crates/picoli-cli`：文件读写、并行调度和结果报告。
- `crates/picoli-wasm`：WebAssembly 接口。
- `vendor`：带本地补丁的第三方依赖及其许可证。

## 许可证与第三方代码

Picoli 采用 [GNU GPL v3.0](LICENSE)（`GPL-3.0-only`）。

本项目直接包含以下开源项目的源码，并进行了本地修改：

- [jpeg-encoder](https://github.com/vstroebel/jpeg-encoder)：JPEG 编码，许可证为 `(MIT OR Apache-2.0) AND IJG`。
- [zune-jpeg](https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg)：JPEG 解码，许可证为 `MIT OR Apache-2.0 OR Zlib`。
- [libdeflater](https://github.com/libdeflater/libdeflater)：libdeflate 的 Rust 包装，许可证为 `Apache-2.0`。

PNG 编码与 DEFLATE 压缩还使用 `png`、`miniz_oxide` 和 C 库 `libdeflate`。这些底层编解码实现来自上游；Picoli 在此基础上增加图片分析、量化与候选搜索、误差约束和结果选择。

第三方代码保留各自的许可证。来源、版本、本地修改和 IJG 致谢见 [第三方代码与许可说明](THIRD-PARTY-NOTICES.md)。
