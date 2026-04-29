<p align="center">
  <img src="icons/icon_128.png" width="128">
</p>

<h1 align="center">LogcatX</h1>

LogcatX 是一个面向 Windows 发布的桌面 GUI 工具，用来并行采集多台 Android 设备的 `adb logcat` 日志。

## 文档入口

- [English README](./README.en.md)
- [更新日志](./CHANGELOG.md)
- [MIT License](./LICENSE)
- [配置示例](./config.example.json)
- [Windows 打包脚本](./scripts/package_windows_release.sh)

## 当前发布形态

- 平台：**Windows**
- 分发方式：**绿色版 zip + 单 exe**
- 当前里程碑：**v0.4.0**

## 核心能力

- 主界面展示当前 ADB 已连接设备
- 双击设备行即可开始采集对应设备的 logcat
- 支持按设备手动停止采集
- 支持多设备并行采集
- 支持设备别名、置顶和最近连接记录
- 支持直接输入 `IP:端口` 快捷连接设备
- 支持 USB 设备变化后的自动刷新
- 支持更清晰的设备状态展示（可用 / 离线 / 未授权 / 已断开）
- 支持按设备别名生成日志目录与日志文件名前缀
- 支持配置 `adb` 可执行文件路径和设备日志保存目录
- 支持刷新设备列表与历史日志占用空间
- 支持清理历史设备日志，同时保护正在写入的日志
- 支持独立的应用运行日志，便于排障
- 内置简体中文与英文界面，首次启动按系统语言选择默认值
- 底层公共能力已拆分到 [DeskFoundry](https://github.com/Shawlaw/DeskFoundry) 统一维护

## 便携模式说明

Windows 发布包会优先读取 exe 同目录下的 `config.json`；如果 exe 目录不可写，则自动回退到 AppData 目录。

这样既适合直接解压即用的绿色版场景，也能兼容受限目录下的运行环境。

## 日志说明

应用会写入两类日志：

1. **设备日志**：来自 Android 设备的 `adb logcat` 输出
2. **应用日志**：LogcatX 自身的启动与运行诊断信息

应用日志与设备日志相互独立，便于定位问题。

## 首次启动

首次启动时，应用会要求确认：

- `adb` 可执行文件路径
- 设备日志保存目录
- 界面语言

设置窗口同时支持：

- 直接打开配置目录
- 直接打开应用日志
- 查看当前是便携模式还是 AppData 模式

## 构建

```bash
cargo xwin build --target x86_64-pc-windows-msvc --release
```

## Windows 打包

```bash
./scripts/package_windows_release.sh
```

打包后会生成：

- `dist/LogcatX.exe`
- `dist/LogcatX-v0.4.0-win64.zip`

## GitHub Release CI

仓库内置了基于 GitHub Actions 的发布流程：当你推送形如 `v0.4.0` 的 tag 到远端时，会自动：

1. 校验 tag 与 `Cargo.toml` 中的版本号一致
2. 构建 Windows 发布版
3. 生成 GitHub Release
4. 上传 `LogcatX.exe` 和对应 zip 包

## 排障建议

- 正常发布版启动时不显示控制台窗口，这属于预期行为。
- 如果需要排查启动问题，可启用 `console` feature 或使用 `--console`。
- 如果设备采集失败，请优先检查已配置的 `adb` 路径以及应用运行日志。
