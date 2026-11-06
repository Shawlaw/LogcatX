# 录屏、文本输入与剪贴板

本文档说明 0.9.3 新增的三组设备快捷能力，以及它们背后的 Android 限制。

## 录屏

设备「更多」菜单 →「开始录屏…」可选择：

| 方式 | 实现 | 时长 | 依赖 |
| --- | --- | --- | --- |
| 快速录制 | `adb shell screenrecord` | Android 14+ 最长 30 分钟；Android 13 及以下 180 秒 | 无 |
| 长录屏 | `scrcpy --no-playback` | 手动停止，无 3 分钟限制 | 已配置 scrcpy 2.0+ |

两种方式都会在开始前选择本地保存路径；默认目录是设置页的「录屏保存目录」
（默认 `exe 目录\recordings`），每次开始仍可另选文件。

### 快速录制

- 记录在设备 `/data/local/tmp/logcatx-rec-*.mp4`，停止后自动 `adb pull` 回电脑，
  然后删除设备临时文件。
- 停止通过 `pkill -INT screenrecord`（或 `killall` 回退），让 `screenrecord` 正常
  写完 MP4 尾部的 `moov`；到设定时长时也会自动停止。
- 打开录屏对话框时会执行 `adb shell screenrecord --help` 探测能力：
  - Android 13 及以下：帮助文本为 "Default / maximum is 180"，只提供 30/60/120/180 秒。
  - Android 14+：帮助文本为 "Set to 0 to remove the time limit"，提供 30 秒到
    30 分钟（1800 秒）的选项。Android Studio 的 30 分钟 Logcat 录屏走的就是
    这条能力（AOSP 在 Android 14 移除了 180 秒 clamp；`--time-limit 0` 可无限录制）。
  - 探测失败或输出无法识别时按 180 秒处理，避免向旧设备发送会被拒绝的参数。
- 本应用把快速录制上限设为 30 分钟；30 分钟视频会占用较大的设备临时空间和回传时间，
  更长或需要跨 Android 版本一致体验时，使用长录屏（scrcpy 直接写电脑）。
- **不录制声音**；安全窗口/DRM 画面可能被录成黑色。
- 停止时向该设备上名为 `screenrecord` 的进程发送 `SIGINT`；请避免在同一设备上同时手动运行
  `screenrecord`，否则也会被一起停止。

### 长录屏

- 由 scrcpy 直接把画面编码写入本地 MP4，不经过设备临时文件。
- 启动时使用 `--no-playback`，并把 scrcpy 的 SDL 窗口隐藏；停止时向该窗口发送
  `WM_CLOSE`，scrcpy 会正常收尾 MP4。若 10 秒内无法关闭，才会强制结束进程并
  在状态栏提示文件可能不完整。
- 需要 scrcpy 2.0 或更新版本；设置页可配置 scrcpy 路径。
- 退出 LogcatX 时会先停止并保存仍在进行的录屏。

## 文本输入

设备「更多」菜单 →「输入文本…」打开多行输入框；也可以直接使用
「将电脑剪贴板文本输入到设备」。

- 每个换行发送 `KEYCODE_ENTER`，制表符发送 `KEYCODE_TAB`。
- ASCII 文本走 `adb shell input text "..."`，与手工命令一致。
- AOSP 的 `input text` 只支持 ASCII；遇到中文/emoji 时 `KeyCharacterMap` 返回
  null，命令会抛 `NullPointerException` 且不会输入任何字符。LogcatX 会先按你的
  习惯尝试 `input text`，失败后把该段文本写入设备剪贴板并发送
  `KEYCODE_PASTE`（279）。这需要设备剪贴板可写——即本应用内置的辅助程序。
- 回退会覆盖设备剪贴板（保留最后一段非 ASCII 文本），这是 Android 平台限制；
  不使用回退时，非 ASCII 文本会返回明确错误。
- 目标 App 的输入框必须获得焦点，并且允许粘贴。

## 双向文本剪贴板（手动）

设备「更多」菜单 →「剪贴板」：

- **将电脑剪贴板写入设备剪贴板**：读取电脑文本，写入设备剪贴板。
- **读取设备剪贴板到电脑**：读取设备当前文本剪贴板，放进电脑剪贴板。
- **将电脑剪贴板文本输入到设备**：不写剪贴板，直接把电脑文本输入当前焦点输入框。

### 为什么需要内置辅助程序

Android 10+ 不允许普通后台 App 读取剪贴板，但 **shell UID** 仍被允许访问
剪贴板服务（测试用途）。scrcpy 的剪贴板同步正是利用这一点。

LogcatX 内置一个约 6 KB 的 dex（`assets/clipboard-helper.dex`，Java 源码在
`assets/clipboard-helper/`），仅在用户点击上述菜单时：

1. `adb push` 到 `/data/local/tmp/logcatx-clipboard.dex`；
2. 以 `shell` 身份运行
   `app_process / com.logcatx.clipboard.ClipboardBridge get|set <base64>`
   （较大文本用 `set-stdin` 从 stdin 读取 base64，避开 Windows 命令行长度限制）；
3. 解析一行结果（`CLIP <base64>` / `EMPTY` / `ACK` / `ERROR`）。

它**不会安装 APK、不会常驻后台、不会修改默认输入法**，也不会自动同步；每次
操作结束后进程立即退出。这个方案与 scrcpy 的 shell UID + `app_process` 思路
一致，但不依赖用户安装 scrcpy，也不打开镜像窗口。

### 限制

- 只同步文本；图片/文件剪贴板不支持。
- 设备端读取不到文本时会显示「剪贴板没有文本」。
- 需要设备允许 `/data/local/tmp` 执行 dex；绝大多数 Android 7+ 设备可以。
  部分深度定制 ROM 可能限制 `app_process`，此时状态栏会显示 helper 的错误输出。
- 当前按「手动双向」实现，不轮询剪贴板；需要持续自动同步时可在后续版本增加
  常驻监听（scrcpy 的 device→PC 自动同步就是监听 `OnPrimaryClipChangedListener`）。

## 安全说明

- 辅助程序源码可审阅，构建脚本为 `scripts/build_clipboard_helper.ps1`；提交的
  dex 与源码一起入库，发布机不需要 Android SDK。
- 只有用户点击菜单才会推送/执行；没有开机自启、没有后台服务、没有网络访问。
- 文本内容不会写入应用日志；状态栏只显示字符数/文件名，不显示剪贴板正文。
- 「写入设备剪贴板」会把电脑剪贴板内容暴露给设备上的 App；请在粘贴敏感内容
  （密码等）前确认。
