# v0.2.0 实施计划

> 目标：把当前可用原型打磨成一个可对外发布的 **Windows 绿色版**，具备无黑框启动、可落盘运行日志、版本信息、便携配置、发布文档和可重复构建能力。

---

## 背景

当前 `LogcatX` 已具备核心功能：

- 展示 ADB 已连接设备
- 双击或点击按钮启动/停止设备日志采集
- 支持多设备并行采集
- 支持首次配置 ADB 路径与日志目录
- 支持刷新设备列表、刷新日志空间、清理历史日志

但与 `clipImg` 这类可发布小工具相比，还存在明显差距：

1. Windows 启动有黑框，发布体验不够完整
2. 没有独立的应用运行日志，也没有 panic 落盘
3. 没有版本号展示、图标、EXE 文件版本信息
4. 配置只走 AppData 路径，不适合绿色版 zip 分发
5. 缺少 README、CHANGELOG、配置示例、构建说明
6. 运行时反馈偏原型化，排障成本高

---

## 发布目标

### 发布形态

- 平台：**Windows**
- 形式：**绿色版 zip + 单 exe**
- 配置路径：**优先 exe 同目录，其次 AppData**
- 构建方式：**`cargo xwin build --target x86_64-pc-windows-msvc --release`**

### 版本定位

- `v0.1.0`：内部原型
- `v0.2.0`：第一版可对外分享的 Windows 发布版

---

## Feature 1：发布基础设施（P0）

### 目标

建立与 `clipImg` 一致的发布骨架，解决便携配置、应用日志、启动诊断、版本信息等基础问题。

### 核心改动

1. **配置路径策略**
   - 若 `exe` 同目录存在 `config.json`，优先使用
   - 若不存在，则回退到 AppData 配置目录
   - 首次运行默认写入 exe 同目录，符合绿色版使用习惯

2. **应用运行日志**
   - 新增独立 logger 模块
   - 日志文件与设备 logcat 输出分离
   - 支持：
     - 文件落盘
     - 可选控制台输出
     - 大小轮转
     - panic hook

3. **启动诊断**
   - 启动时记录：
     - 应用版本
     - 配置路径
     - 运行日志路径
     - ADB 路径
     - 日志保存目录
   - 启动失败提供明确错误弹窗，避免闪退无提示

4. **版本显示**
   - 在 UI 中显示 `vX.Y.Z`
   - 构建时附带 build/version 信息

### 涉及文件

- `src/main.rs`
- `src/config.rs`
- `src/logger.rs`（新增）
- `src/app.rs`
- `src/models.rs`

---

## Feature 2：Windows 启动体验与 EXE 身份（P0）

### 目标

解决“黑框”“没有图标”“无版本信息”的发布观感问题。

### 核心改动

1. **去黑框**
   - 默认启用 Windows GUI subsystem
   - 保留 `console` feature 或 `--console` 作为调试模式

2. **EXE 图标与文件信息**
   - 新增 `build.rs`
   - 新增 `icons/icon.ico`
   - 嵌入：
     - ProductName
     - FileDescription
     - FileVersion
     - ProductVersion
     - OriginalFilename

3. **资源目录结构**
   - `assets/`：图标源文件和设计说明
   - `icons/`：构建所需 ICO 和派生 PNG

### 涉及文件

- `Cargo.toml`
- `build.rs`（新增）
- `icons/icon.ico`（新增）
- `assets/README.md`（新增）
- `src/main.rs`

---

## Feature 3：首启和主界面 UX 硬化（P1）

### 目标

让用户第一次打开就能理解“这个工具怎么配、日志去哪了、出错怎么看”。

### 核心改动

1. **首启配置增强**
   - 更清楚地解释：
     - ADB 可执行文件路径
     - 设备日志保存目录
     - 应用运行日志位置
   - 提供更好的校验反馈

2. **主界面增强**
   - 显示：
     - 版本号
     - 当前已连接设备数
     - 当前采集中设备数
   - 增加快捷操作：
     - 打开日志目录
     - 打开应用日志
     - 打开配置文件目录

3. **状态反馈增强**
   - 持久显示最近错误
   - 清晰区分：
     - 启动失败
     - ADB 校验失败
     - 设备断开
     - 采集异常退出
     - 清理失败

### 涉及文件

- `src/app.rs`
- `src/config.rs`
- `src/fs_utils.rs`

---

## Feature 4：采集稳定性与运行细节打磨（P1）

### 目标

让日志采集过程更可解释、更稳、更接近一个公开工具应有的行为。

### 核心改动

1. **运行态补全**
   - 记录每台设备：
     - started_at
     - output_path
     - bytes_written（可后续扩展）
     - 最后退出状态

2. **停止与异常退出**
   - 明确区分：
     - 用户手动停止
     - 设备断开导致退出
     - ADB 进程启动失败
     - ADB 进程异常返回码

3. **历史清理保护**
   - 清理历史日志时排除：
     - 正在写入的设备日志
     - 应用运行日志

---

## Feature 5：发布文档与产物（P0）

### 目标

让项目具备可交付、可理解、可复现构建的基本材料。

### 核心改动

1. 新增 `README.md`
   - 工具介绍
   - 使用方式
   - 首次运行说明
   - ADB 配置说明
   - 设备日志与应用日志说明
   - Windows 构建方式

2. 新增 `CHANGELOG.md`
   - 从 `v0.2.0` 开始维护

3. 新增 `config.example.json`
   - 作为发布包参考配置

4. 新增 `.cargo/config.toml`
   - 说明 `cargo xwin` 构建方式

5. 发布输出规范
   - `dist/LogcatX.exe`
   - `dist/LogcatX-v0.2.0-win64.zip`

---

## 实施顺序

### Step 1：发布基础设施
- 配置路径改为“便携优先”
- 增加 logger 和 panic hook
- 启动日志与错误弹窗
- UI 中加入版本信息

### Step 2：Windows EXE 身份
- 增加 `build.rs`
- 增加 `icons/` 与 `assets/`
- 嵌入 EXE 图标和版本元数据
- 去掉默认黑框

### Step 3：UI/UX 硬化
- 打磨首启窗口
- 增加打开目录/日志按钮
- 优化状态栏和错误反馈

### Step 4：文档与打包
- README
- CHANGELOG
- config example
- `.cargo/config.toml`
- release zip

### Step 5：验证
- `cargo fmt`
- `cargo test`
- `cargo check`
- `cargo xwin build --target x86_64-pc-windows-msvc --release`
- Windows 实机验证：
  - 正常启动无黑框
  - 运行日志落盘
  - 首启配置正常
  - 图标和版本信息正确

---

## 预期交付物

- 面向 Windows 的正式绿色版 exe
- 带版本信息和图标的 EXE
- 应用运行日志 + panic 日志
- 更稳定的首启和运行反馈
- README / CHANGELOG / 配置示例 / 构建说明
- 可直接发布的 zip 产物
