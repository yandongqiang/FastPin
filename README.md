<div align="center">

<img src="assets/logo/fastpin.png" width="120" alt="FastPin Logo">

# FastPin

**常驻托盘的 Windows 截图工具**

标注编辑 · 本地 OCR · API 翻译 · 长截图 · 贴图钉住

</div>

## 关于

**本项目完全由 vibe coding（AI 辅助编程）完成。**

实现上参考了两个优秀的开源项目：

- **[snow-shot](https://github.com/mg-chao/snow-shot)**（GPL-3.0-or-later）—— 图像拼接、UI 元素检测、贴图、切图等模块直接来自它，或按它的实现移植
- **Zshot** —— FastPin 最初基于它二次开发而来

在参考项目的基础上，FastPin 新增的功能是标注工具里的**云线**。

## 功能

- **区域截图**：冻结帧框选，窗口 / 元素自动吸附，可自定义选区边框（实线 / 虚线、粗细、颜色）
- **标注编辑**：矩形、椭圆、箭头、画笔、文字、马赛克、序号、直线（实线 / 虚线 / **云线**），支持撤销 / 重做
- **本地 OCR**：PP-OCRv6 本地推理，截图不上传
- **翻译**：发往你自己配置的 OpenAI 兼容 API，密钥存 Windows 凭据管理器
- **长截图**：自动滚动 + 特征匹配拼接，可切图、拼图、导出 PDF
- **贴图钉住**：无边框置顶、拼接成组、组内标注
- **托盘常驻**：无主窗口，设置独立打开

## 快速上手

装好后程序静默进托盘，直接按热键：

| 热键 | 动作 |
| --- | --- |
| `Ctrl` + `Alt` + `A` | 区域截图 |
| `Ctrl` + `Alt` + `W` | 区域截图，只进剪贴板 |
| `PrtSc` | 全屏截图 |

热键可在「设置 → 快捷键」里重新录制。托盘左键单击 = 区域截图，右键菜单里有设置和退出。

框选后覆盖层不关闭，可直接在选区上标注 / 取字 / 翻译 / 长截图，`Enter` 或「完成」结束。

## 从源码构建

需要 Rust stable（MSVC）和 Windows 10/11 SDK。OCR 模型已随仓库提供（`models/v6/`），无需联网下载。

```bash
cargo build --release                              # 产出 target/release/FastPin.exe
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

设置存 `%LOCALAPPDATA%\FastPin\settings.json`。

## 许可证

GPL-3.0-or-later，见 [LICENSE](LICENSE) 与 [NOTICE](NOTICE)。

FastPin 包含来自 snow-shot（GPL-3.0-or-later）的代码并参考其实现，因此整体按 GPL-3.0-or-later 发布。
