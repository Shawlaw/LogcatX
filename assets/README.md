# assets/ — UI 资源源文件

此目录用于存放图标和发布品牌资源的源文件，不直接打包进程序。

## 当前文件

| 文件 | 用途 |
|------|------|
| `icon_source.png` | 应用图标原始设计稿（1024x1024） |
| `icon_raw.png` | 图标设计过程中的原始草稿 / 备用版本 |
| `icon_source_corner_256.png` | 已加圆角的正式应用图标源图，程序图标和 README 展示优先使用它 |
| `ui_redesign_reference_20260430.png` | 主界面重构参考图（已缩小后纳入项目，供后续 UI 重构对照） |

## 如何更新图标

1. 更新 `assets/icon_source_corner_256.png`
2. 在项目根目录执行：

```bash
python3 - <<'PY'
from PIL import Image
img = Image.open('assets/icon_source_corner_256.png').convert('RGBA')
sizes = [(16,16),(32,32),(48,48),(64,64),(128,128),(256,256)]
img.resize((256,256), Image.LANCZOS).save('icons/icon.ico', format='ICO', sizes=sizes)
for s,_ in sizes:
    img.resize((s,s), Image.LANCZOS).save(f'icons/icon_{s}.png')
PY
```

3. 重新构建 Windows 版本：

```bash
cargo xwin build --target x86_64-pc-windows-msvc --release
```

## 图标在程序中的使用

| 用途 | 文件 | 说明 |
|------|------|------|
| EXE 图标 + 属性面板 | `icons/icon.ico` | 通过 `build.rs` 嵌入 Windows 资源 |
| 程序窗口图标 | `icons/icon_256.png` | `src/main.rs` 运行时加载 |
| README 展示 | `icons/icon_128.png` | Markdown 中引用 |
