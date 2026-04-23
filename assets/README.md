# assets/ — UI 资源源文件

此目录用于存放图标和发布品牌资源的源文件，不直接打包进程序。

## 当前文件

| 文件 | 用途 |
|------|------|
| `icon_source.png` | 应用图标原始设计稿，所有发布图标从它导出 |

## 如何更新图标

1. 替换 `assets/icon_source.png`
2. 在项目根目录执行：

```bash
python3 - <<'PY'
from PIL import Image
img = Image.open('assets/icon_source.png').convert('RGBA')
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
