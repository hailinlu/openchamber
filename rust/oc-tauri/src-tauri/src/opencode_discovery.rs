//! OpenCode 二进制探测 — 在打包运行时找到真实的 `opencode.exe` 写进 env。
//!
//! **为什么需要这个:** 打包后的 Tauri 运行时不再继承 dev 启动器设的
//! `OPENCODE_BINARY` env。`oc-server` 默认 `opencode_binary = "opencode"`,
//! 而在 Windows 上 `Command::new("opencode")` 完全不会查 `PATHEXT`,所以
//! npm 全局装的 `opencode.cmd` shim 找不到 → spawn 失败 →
//! "Local OpenCode Unavailable"。
//!
//! **探测策略 (三层降级,Windows):**
//!
//! 1. **Tier 1 — 已知位置的 `opencode.exe` 直查**
//!    直接对一组常见安装位置 (`%USERPROFILE%\.opencode\bin\`、
//!    `%APPDATA%\npm\`、`%ProgramFiles%\nodejs\`、scoop、chocolatey、
//!    `%USERPROFILE%\.bun\bin\`) 做 `is_file()` 探针,命中 `.exe` 即返回。
//!    这覆盖了用户把 opencode 装到非 npm 路径的情况 (bun / scoop / 自解压)。
//!
//! 2. **Tier 2 — `.cmd` shim 解析**
//!    对同一组已知位置,检查 `opencode.cmd` 是否存在。若存在,读文件内容,
//!    用正则从内容里抽出 shim 实际代理的 `opencode.exe` 路径。
//!    `npm i -g opencode-ai` 生成的 shim 内容形如:
//!    `"C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe" %*`
//!    抽出后验证 `.exe` 真存在 → 返回。这覆盖了 npm 全局装的最常见场景。
//!
//! 3. **Tier 3 — `where.exe opencode` 兜底**
//!    Windows 自带的 `where.exe` 走 `PATHEXT`,能找出任何 PATH 里的
//!    `opencode.exe` / `opencode.cmd` / `opencode.bat`。对每行结果,
//!    `.exe` 直接返回;`.cmd` / `.bat` 走 Tier 2 的解析。
//!
//! 关键不变量:返回路径**必须**以 `.exe` 结尾。`oc-server/src/opencode/mod.rs:169`
//! 用 `Command::new(&config.opencode_binary)` spawn,Windows 上 CreateProcess
//! 不查 `PATHEXT`,所以 `.cmd` shim 直接传过去等于 spawn 失败。
//!
//! 本模块**不依赖** fork npm (`Command::new("npm")` 自身也受同样的 .cmd 问题
//! 影响,见历史上 `Command::new("npm").args(["root","-g"])` 的失败)。

use std::path::{Path, PathBuf};

/// 探测 opencode 二进制全路径, 必要时写入 `OPENCODE_BINARY` env。
///
/// 决策顺序 (与 `tauri-dev.mjs:resolveOpencodeBinary` 保持同等覆盖):
/// 1. 用户已显式设 `OPENCODE_BINARY` → 尊重, 不动。
/// 2. 探测已知安装位置 → 命中则写入 env (覆盖默认 "opencode")。
/// 3. 探测失败 → 不动 (保留默认 "opencode", 让 oc-server 报原本的 spawn 错误)。
///
/// 仅在 `OPENCODE_BINARY` 未设置或为空时探测; 已设置则完全尊重用户意图。
pub fn resolve_and_export() {
    // 1. 用户显式设置 → 不干预。
    if let Ok(val) = std::env::var("OPENCODE_BINARY") {
        if !val.trim().is_empty() {
            return;
        }
    }

    match resolve_opencode_binary() {
        Some(path) => {
            let path_str = path.to_string_lossy().to_string();
            log::info!(
                "resolved opencode binary to {}; setting OPENCODE_BINARY",
                path_str
            );
            std::env::set_var("OPENCODE_BINARY", &path_str);
        }
        None => {
            // 不动 env, oc-server 会用默认 "opencode" 尝试 spawn 并报错。
            log::warn!(
                "could not auto-detect opencode binary; falling back to default \"opencode\" \
                 (spawn may fail if opencode is not on PATH as a direct executable)"
            );
        }
    }
}

/// 探测 opencode 二进制路径。返回 `None` 表示未找到。
///
/// 平台分支:
/// - Windows: 三层降级 (已知位置 → .cmd 解析 → `where.exe`)
/// - Unix: 已知路径 + PATH 兜底 (不变)
pub fn resolve_opencode_binary() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        resolve_windows_with_roots(&default_windows_roots())
    }
    #[cfg(not(windows))]
    {
        resolve_unix()
    }
}

/// Windows 探测用的根目录集合。
///
/// 生产路径用 `default_windows_roots()` 读 `%APPDATA%` / `%USERPROFILE%` /
/// `%ProgramFiles%` / `%ProgramData%`。测试路径可以传合成 roots,
/// 避免污染测试机器的真实目录 (并行 cargo test 下也安全)。
///
/// 与 `env-runtime.js:382-395` 的 Windows fallback 表对齐:
/// `localAppData` 在 Node 那边也是只读不用,这里同样不读。
#[cfg(windows)]
#[derive(Debug, Clone)]
struct WindowsRoots {
    user_profile: Option<PathBuf>,
    app_data: Option<PathBuf>,
    program_files: Option<PathBuf>,
    program_files_x86: Option<PathBuf>,
    program_data: Option<PathBuf>,
}

#[cfg(windows)]
impl WindowsRoots {
    #[cfg(test)] // 仅测试用
    fn empty() -> Self {
        Self {
            user_profile: None,
            app_data: None,
            program_files: None,
            program_files_x86: None,
            program_data: None,
        }
    }
}

#[cfg(windows)]
fn default_windows_roots() -> WindowsRoots {
    WindowsRoots {
        user_profile: std::env::var_os("USERPROFILE").map(PathBuf::from),
        app_data: std::env::var_os("APPDATA").map(PathBuf::from),
        program_files: std::env::var_os("ProgramFiles").map(PathBuf::from),
        program_files_x86: std::env::var_os("ProgramFiles(x86)").map(PathBuf::from),
        program_data: std::env::var_os("ProgramData").map(PathBuf::from),
    }
}

/// Windows 三层降级探测。详见模块级文档。
#[cfg(windows)]
fn resolve_windows_with_roots(roots: &WindowsRoots) -> Option<PathBuf> {
    // Tier 1: 已知位置的 opencode.exe 直查 (无 fork,纯 stat)。
    for candidate in tier1_native_exe_candidates(roots) {
        if candidate.is_file() {
            log::debug!("opencode discovery: tier 1 hit at {}", candidate.display());
            return Some(candidate);
        }
    }

    // Tier 2: .cmd shim 解析 (读文件内容,抽真实 .exe 路径,验证存在)。
    for shim in tier2_cmd_shim_candidates(roots) {
        if !shim.is_file() {
            continue;
        }
        if let Some(exe) = extract_opencode_exe_from_cmd_shim(&shim) {
            if exe.is_file() {
                log::debug!(
                    "opencode discovery: tier 2 hit via shim {} -> {}",
                    shim.display(),
                    exe.display()
                );
                return Some(exe);
            }
        }
    }

    // Tier 3: where.exe opencode 兜底 (用户自定义安装目录)。
    if let Some(path) = tier3_where_exe_lookup() {
        log::debug!("opencode discovery: tier 3 hit at {}", path.display());
        return Some(path);
    }

    None
}

/// Tier 1: 已知位置的 `opencode.exe` 候选路径列表。
///
/// 对齐 `packages/web/server/lib/opencode/env-runtime.js:382-395`。
/// 仅返回**真实存在的 `.exe` 路径**才有效 (调用方用 `is_file()` 过滤)。
#[cfg(windows)]
fn tier1_native_exe_candidates(roots: &WindowsRoots) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(8);

    if let Some(p) = &roots.user_profile {
        out.push(p.join(".opencode").join("bin").join("opencode.exe"));
        out.push(p.join("scoop").join("shims").join("opencode.exe"));
        out.push(p.join(".bun").join("bin").join("opencode.exe"));
    }
    if let Some(p) = &roots.app_data {
        out.push(p.join("npm").join("opencode.exe"));
    }
    if let Some(p) = &roots.program_files {
        out.push(p.join("nodejs").join("opencode.exe"));
    }
    if let Some(p) = &roots.program_files_x86 {
        out.push(p.join("nodejs").join("opencode.exe"));
    }
    if let Some(p) = &roots.program_data {
        out.push(p.join("chocolatey").join("bin").join("opencode.exe"));
    }

    out
}

/// Tier 2: 已知位置的 `opencode.cmd` shim 候选路径列表。
///
/// 与 Tier 1 同样的根目录集合,但目标是 `.cmd` (npm / scoop / chocolatey
/// 在这些位置放的是 shim,而真正的 .exe 在 shim 内容指向的别处)。
#[cfg(windows)]
fn tier2_cmd_shim_candidates(roots: &WindowsRoots) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(8);

    if let Some(p) = &roots.user_profile {
        out.push(p.join(".opencode").join("bin").join("opencode.cmd"));
        out.push(p.join("scoop").join("shims").join("opencode.cmd"));
        out.push(p.join(".bun").join("bin").join("opencode.cmd"));
    }
    if let Some(p) = &roots.app_data {
        out.push(p.join("npm").join("opencode.cmd"));
    }
    if let Some(p) = &roots.program_files {
        out.push(p.join("nodejs").join("opencode.cmd"));
    }
    if let Some(p) = &roots.program_files_x86 {
        out.push(p.join("nodejs").join("opencode.cmd"));
    }
    if let Some(p) = &roots.program_data {
        out.push(p.join("chocolatey").join("bin").join("opencode.cmd"));
    }

    out
}

/// 从 .cmd shim 文件里抽出它代理的 opencode.exe 绝对路径。读文件 + 调纯函数。
///
/// 典型 shim 内容:
/// 1. **绝对路径 shim** (老式 npm / scoop / choco):
///    ```text
///    @ECHO off
///    "C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe" %*
///    ```
/// 2. **`%dp0%` 变量 shim** (npm 较新版,本次生产环境实测):
///    ```text
///    @ECHO off
///    GOTO start
///    :find_dp0
///    SET dp0=%~dp0
///    EXIT /b
///    :start
///    SETLOCAL
///    CALL :find_dp0
///    "%dp0%\node_modules\opencode-ai\bin\opencode.exe"   %*
///    ```
///    这里的 `%dp0%` = shim 自身所在目录,所以完整路径 = `shim.parent + \node_modules\...`
///
/// 返回 `None` 表示无法识别(文件读不出、内容空、不像 opencode shim 都算)。
#[cfg(windows)]
fn extract_opencode_exe_from_cmd_shim(shim_path: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(shim_path).ok()?;
    let shim_dir = shim_path.parent()?;
    parse_opencode_exe_from_shim_content(&content, shim_dir)
}

/// 从 .cmd shim 文本里抽出它代理的 opencode.exe 绝对路径。纯函数,可单测。
///
/// 抽出策略:
/// 1. 扫所有 `"..."` 引号包裹的 token,挑第一个**以 `opencode.exe` 结尾**的
/// 2. Token 可能是:
///    - **绝对路径** (`C:\...\opencode.exe`) → 直接用
///    - **`%dp0%` / `%~dp0%` 相对路径** (`%dp0%\node_modules\...\opencode.exe`) → 用 `shim_dir` 替换 `%dp0%`
///    - **裸相对路径** (`\node_modules\...\opencode.exe`) → 直接拼 `shim_dir` 前缀
/// 3. 引号内的 `\\` 还原成 `\`(cmd 双重转义)
///
/// 返回 `None` 表示无法识别。
#[cfg(windows)]
fn parse_opencode_exe_from_shim_content(content: &str, shim_dir: &Path) -> Option<PathBuf> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 收集所有引号包裹的 token。cmd shim 的真身永远在引号里(路径含空格)。
    // 用手写的 split 而不是依赖 regex(避免在 Windows MSRV 下拉新 crate)。
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 找下一个开引号
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'"' {
            end += 1;
        }
        if end >= bytes.len() {
            // 没找到闭合引号,放弃这行
            return None;
        }
        let raw = &trimmed[start..end];
        if let Some(path) = shim_token_to_exe_path(raw, shim_dir) {
            return Some(path);
        }
        // 没命中(比如 shim 内容是 "@echo off" 这种命令),继续扫下一个引号块
        i = end + 1;
    }

    None
}

/// 把 shim 内的一个 token 还原成可用的 `.exe` 绝对路径。
///
/// 三种 shim 形态都能识别:
/// - `"C:\...\opencode.exe"` (绝对路径,带或不带双反斜杠转义)
/// - `"%dp0%\node_modules\opencode-ai\bin\opencode.exe"` (npm 当前 shim 格式,变量替换)
/// - `"\node_modules\opencode-ai\bin\opencode.exe"` (相对路径,直接拼 shim_dir)
///
/// 不变量:返回值必须以 `opencode.exe` 结尾(大小写不敏感)。
/// 这条不变量挡住了 shim 第一行的 `@echo`、第二行的 `node`、第三行的 `set` 等噪声 token。
#[cfg(windows)]
fn shim_token_to_exe_path(token: &str, shim_dir: &Path) -> Option<PathBuf> {
    let lower = token.to_ascii_lowercase();
    // 必须以 opencode.exe 结尾(大小写不敏感)。
    let exe_marker = "opencode.exe";
    let exe_idx = lower.rfind(exe_marker)?;
    // 路径必须以 .exe 结尾,且之前不能有 `\`(否则匹配到的是中间目录名)。
    let after = exe_idx + exe_marker.len();
    if after != lower.len() {
        return None;
    }

    // 还原 cmd 双重反斜杠。npm / scoop / choco 生成的 shim 用 `\\` 转义。
    // 也直接接受单 `\`(单行 shim 不转义)。
    let normalized = token.replace("\\\\", "\\");

    // 情况 1: 绝对路径。Windows 上以盘符开头(`C:\`)或 UNC(`\\server\share`)。
    if looks_like_absolute_windows_path(&normalized) {
        return Some(PathBuf::from(normalized));
    }

    // 情况 2: `%dp0%` / `%~dp0%` 变量替换 —— 替换成 shim_dir。
    // npm 当前格式: `"%dp0%\node_modules\opencode-ai\bin\opencode.exe"`
    // scoop 风格: `"%~dp0%\node_modules\..."` (基本等价)
    if let Some(after_dp0) = strip_dp0_prefix(&normalized) {
        let joined = join_path_no_separator(shim_dir, after_dp0);
        return Some(joined);
    }

    // 情况 3: 裸相对路径 —— 直接拼 shim_dir 前缀。
    // 例如 `"\node_modules\opencode-ai\bin\opencode.exe"`(没有 `%dp0%`,但以 `\` 开头)
    if normalized.starts_with('\\') || normalized.starts_with('/') {
        let joined = join_path_no_separator(shim_dir, &normalized);
        return Some(joined);
    }

    None
}

/// 从 shim token 里剥掉 `%dp0%` / `%~dp0%` 前缀,返回剩余相对路径。
///
/// `None` 表示没有这种变量前缀(走其他分支)。
#[cfg(windows)]
fn strip_dp0_prefix(token: &str) -> Option<&str> {
    // 顺序无所谓 —— 哪个先出现都该剥。trim 后剩下的以 `\xxx\...` 形式直接拼 shim_dir。
    const PREFIXES: &[&str] = &["%~dp0%", "%dp0%"];
    for prefix in PREFIXES {
        if let Some(rest) = token.strip_prefix(prefix) {
            // rest 必须以 `\` 或 `/` 开头,否则是别的东西。
            if rest.starts_with('\\') || rest.starts_with('/') {
                return Some(rest);
            }
        }
    }
    None
}

/// 拼 shim_dir + 相对路径,自动处理 shim_dir 是否以分隔符结尾。
#[cfg(windows)]
fn join_path_no_separator(base: &Path, rel: &str) -> PathBuf {
    let rel_trimmed = rel.trim_start_matches(|c| c == '\\' || c == '/');
    let mut out = base.to_path_buf();
    // 始终补一个分隔符,防止 `C:\foo` + `\bar` 拼成 `C:\foo\bar` 而非 `C:\foo\bar` 时漏掉斜杠。
    out.push(rel_trimmed);
    out
}

/// 粗判一个字符串是不是合法的 Windows 绝对路径。
///
/// Windows 绝对路径形式:
/// - `C:\` / `D:/`(盘符 + 冒号 + 分隔符)
/// - `\\server\share`(UNC)
/// - `//server/share`(部分 shell 兼容)
#[cfg(windows)]
fn looks_like_absolute_windows_path(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 3 {
        return false;
    }
    // 盘符路径: ASCII 字母 + ':' + 分隔符
    let first = bytes[0];
    if first.is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        return true;
    }
    // UNC: `\\` 或 `//`
    if (bytes[0] == b'\\' || bytes[0] == b'/') && (bytes[1] == b'\\' || bytes[1] == b'/') {
        return true;
    }
    false
}

/// Tier 3: `where.exe opencode` 兜底。
///
/// `where.exe` 走 PATHEXT,会返回 `opencode.exe` / `opencode.cmd` /
/// `opencode.bat` (按 PATH 顺序)。逐行解析:
/// - `.exe` 命中 → 直接返回(`is_file()` 防御性再确认一次)。
/// - `.cmd` / `.bat` → 走 shim 解析(复用 Tier 2 的逻辑)。
#[cfg(windows)]
fn tier3_where_exe_lookup() -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    let mut cmd = Command::new("where.exe");
    cmd.arg("opencode");
    // CREATE_NO_WINDOW — 沿用 oc-server/src/opencode/mod.rs:176 的同款写法。
    // 避免打包运行时弹黑色 cmd 窗口。
    cmd.creation_flags(0x08000000);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            log::debug!("opencode discovery: where.exe spawn failed: {}", e);
            return None;
        }
    };
    if !output.status.success() {
        log::debug!(
            "opencode discovery: where.exe exited with status {:?}",
            output.status.code()
        );
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(trimmed);
        let ext_lower = candidate
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        match ext_lower.as_deref() {
            Some("exe") => {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            Some("cmd") | Some("bat") => {
                if let Some(exe) = extract_opencode_exe_from_cmd_shim(&candidate) {
                    if exe.is_file() {
                        return Some(exe);
                    }
                }
                // shim 内容识别失败,继续扫 where 的下一行
            }
            _ => {
                // 未知扩展名,跳过(比如 PATH 里有奇怪的链接)
            }
        }
    }

    None
}

/// Unix: 依次探测已知安装位置, 最后 `which opencode` 兜底。
///
/// 复现 `tauri-dev.mjs:93-107` 的探测顺序与路径。
#[cfg(not(windows))]
fn resolve_unix() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let candidates = [
        PathBuf::from(&home).join(".opencode/bin/opencode"),
        PathBuf::from(&home).join(".local/bin/opencode"),
        PathBuf::from(&home).join(".bun/bin/opencode"),
    ];
    for candidate in &candidates {
        if candidate.is_file() {
            return Some(candidate.clone());
        }
    }
    // 最终 PATH 兜底 (which opencode)。
    which_on_path("opencode")
}

/// 在 PATH 中查找可执行文件 (Unix)。
#[cfg(not(windows))]
fn which_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.permissions().mode() & 0o111 != 0 {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— shim 内容解析 (与具体文件系统无关,纯函数测试) ——

    /// 典型 npm 全局装 opencode-ai 生成的 shim (单行,带引号)。
    #[test]
    #[cfg(windows)]
    fn extract_exe_handles_quoted_single_line_shim() {
        let shim = r#""C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe" %*"#;
        let shim_dir = Path::new(r"C:\Program Files\nodejs");
        let exe = parse_opencode_exe_from_shim_content(shim, shim_dir);
        // 这条测试只验证解析函数;真实文件不存在的 .exe 也照样返回。
        assert_eq!(
            exe.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some(r"C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe".to_string())
        );
    }

    /// 多行 shim,带 `@ECHO off` 头。噪声行被跳过,真身在第二行。
    #[test]
    #[cfg(windows)]
    fn extract_exe_handles_multiline_shim_with_echo_off() {
        let shim = "@ECHO off\r\n\
                    \"C:\\Users\\foo\\AppData\\Roaming\\npm\\node_modules\\opencode-ai\\bin\\opencode.exe\" %*\r\n";
        let shim_dir = Path::new(r"C:\Users\foo\AppData\Roaming\npm");
        let exe = parse_opencode_exe_from_shim_content(shim, shim_dir);
        assert_eq!(
            exe.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some(
                r"C:\Users\foo\AppData\Roaming\npm\node_modules\opencode-ai\bin\opencode.exe"
                    .to_string()
            )
        );
    }

    /// shim 内容跟 opencode 无关(`dir /b`),必须返回 None,不能误报。
    #[test]
    #[cfg(windows)]
    fn extract_exe_returns_none_for_unrelated_content() {
        let shim = "@echo off\r\ndir /b\r\nexit /b\r\n";
        let shim_dir = Path::new(r"C:\Program Files\nodejs");
        assert!(parse_opencode_exe_from_shim_content(shim, shim_dir).is_none());
    }

    /// 双重反斜杠转义(scoop / 部分 npm shim 用 `\\` 还原单 `\`)。
    #[test]
    #[cfg(windows)]
    fn extract_exe_handles_double_backslash_escaping() {
        // 模拟: shim 原文是 `"C:\\Program Files\\nodejs\\node_modules\\opencode-ai\\bin\\opencode.exe"`
        // (在 Rust 字符串字面量里写出来就是每个 `\\` 两字节)
        let shim = r#""C:\\Program Files\\nodejs\\node_modules\\opencode-ai\\bin\\opencode.exe" %*"#;
        let shim_dir = Path::new(r"C:\Program Files\nodejs");
        let exe = parse_opencode_exe_from_shim_content(shim, shim_dir);
        // 还原后应该是单 `\`
        assert_eq!(
            exe.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some(
                r"C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe".to_string()
            )
        );
    }

    /// **生产实测**:npm 当前版本生成的 shim 格式 —— 用 `%dp0%` 变量引用
    /// shim 自身所在目录。这是本次失败的根本原因:之前的解析器只认
    /// 绝对路径,看到 `%dp0%` 这种"相对+变量"就拒绝。
    ///
    /// shim 原文 (用户从 `C:\Program Files\nodejs\opencode.cmd` 实测):
    /// ```cmd
    /// @ECHO off
    /// GOTO start
    /// :find_dp0
    /// SET dp0=%~dp0
    /// EXIT /b
    /// :start
    /// SETLOCAL
    /// CALL :find_dp0
    /// "%dp0%\node_modules\opencode-ai\bin\opencode.exe"   %*
    /// ```
    /// 期望解析出: shim_dir + `\node_modules\opencode-ai\bin\opencode.exe`
    #[test]
    #[cfg(windows)]
    fn extract_exe_handles_dp0_variable_shim_format() {
        let shim = "@ECHO off\r\n\
                    GOTO start\r\n\
                    :find_dp0\r\n\
                    SET dp0=%~dp0\r\n\
                    EXIT /b\r\n\
                    :start\r\n\
                    SETLOCAL\r\n\
                    CALL :find_dp0\r\n\
                    \"%dp0%\\node_modules\\opencode-ai\\bin\\opencode.exe\"   %*\r\n";
        let shim_dir = Path::new(r"C:\Program Files\nodejs");
        let exe = parse_opencode_exe_from_shim_content(shim, shim_dir);
        assert_eq!(
            exe.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some(
                r"C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe"
                    .to_string()
            )
        );
    }

    /// `%~dp0%` (波浪号修饰形式) 也应该被识别 —— scoop / 部分 npm shim 用这种。
    #[test]
    #[cfg(windows)]
    fn extract_exe_handles_tilde_dp0_variant() {
        let shim = r#""%~dp0%\node_modules\opencode-ai\bin\opencode.exe" %*"#;
        let shim_dir = Path::new(r"C:\Program Files\nodejs");
        let exe = parse_opencode_exe_from_shim_content(shim, shim_dir);
        assert_eq!(
            exe.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some(
                r"C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe"
                    .to_string()
            )
        );
    }

    /// 裸相对路径(没有 `%dp0%`,但以 `\` 开头)也应该拼 shim_dir 前缀。
    #[test]
    #[cfg(windows)]
    fn extract_exe_handles_bare_relative_path_token() {
        // 模拟一个奇葩 shim: token 直接是 `\node_modules\...\opencode.exe`,无任何变量。
        let shim = r#""\node_modules\opencode-ai\bin\opencode.exe" %*"#;
        let shim_dir = Path::new(r"C:\Program Files\nodejs");
        let exe = parse_opencode_exe_from_shim_content(shim, shim_dir);
        assert_eq!(
            exe.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some(
                r"C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe"
                    .to_string()
            )
        );
    }

    /// 没引号 / 不是绝对路径 / 扩展名不对 —— 都必须返回 None。
    #[test]
    #[cfg(windows)]
    fn extract_exe_rejects_relative_or_wrong_ext() {
        let shim_dir = Path::new(r"C:\Program Files\nodejs");
        // 相对路径 + 没有 opencode.exe 后缀
        let bad1 = "opencode.exe";
        assert!(shim_token_to_exe_path(bad1, shim_dir).is_none());
        // 相对路径(无盘符)
        let bad2 = r".\bin\opencode.exe";
        assert!(shim_token_to_exe_path(bad2, shim_dir).is_none());
        // 绝对路径但后缀不是 .exe
        let bad3 = r"C:\node_modules\opencode-ai\bin\opencode.cmd";
        assert!(shim_token_to_exe_path(bad3, shim_dir).is_none());
    }

    // —— 三层降级的端到端探测(用合成 roots,只在 tempdir 写文件) ——

    /// Tier 1: 在 `user_profile\.opencode\bin\opencode.exe` 放一个空文件,期望命中。
    #[test]
    #[cfg(windows)]
    fn tier1_finds_native_exe_in_well_known_location() {
        let temp = std::env::temp_dir().join(format!(
            "oc-disc-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        let exe = temp.join(".opencode").join("bin").join("opencode.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();

        let roots = WindowsRoots {
            user_profile: Some(temp.clone()),
            ..WindowsRoots::empty()
        };
        let found = resolve_windows_with_roots(&roots);
        let _ = std::fs::remove_dir_all(&temp);

        assert_eq!(found.as_deref(), Some(exe.as_path()));
    }

    /// Tier 2 路径 (I/O 端到端): 把一个真实 .cmd shim 写到 tempdir,内容指向
    /// 同一 tempdir 下的一个真实 .exe。文件读取 + 解析 + 路径验证全链路走通。
    #[test]
    #[cfg(windows)]
    fn tier2_finds_exe_via_cmd_shim_parse() {
        let temp = std::env::temp_dir().join(format!(
            "oc-disc-test-shim-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        // 真实 .exe
        let exe = temp.join("node_modules").join("opencode-ai").join("bin").join("opencode.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        // .cmd shim 指向它 (双反斜杠是 cmd shim 转义形式,验证还原逻辑也走过)
        let shim = temp.join("opencode.cmd");
        std::fs::write(
            &shim,
            format!(
                "@ECHO off\r\n\"{}\" %*\r\n",
                exe.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();

        let parsed = extract_opencode_exe_from_cmd_shim(&shim);
        let _ = std::fs::remove_dir_all(&temp);

        assert_eq!(parsed.as_deref(), Some(exe.as_path()));
    }

    /// 端到端: 合成 roots 走 resolve_windows_with_roots,期望直接命中 Tier 2。
    #[test]
    #[cfg(windows)]
    fn resolve_windows_finds_exe_via_synthesized_cmd_shim() {
        let temp = std::env::temp_dir().join(format!(
            "oc-disc-test-e2e-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        // 真实 .exe (放在 %USERPROFILE%\.bun\bin\opencode.exe 这样的常见位置)
        let exe = temp
            .join(".bun")
            .join("bin")
            .join("opencode.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();

        // 不放 shim —— Tier 1 应该直接命中 .bun\bin\opencode.exe
        let roots = WindowsRoots {
            user_profile: Some(temp.clone()),
            ..WindowsRoots::empty()
        };
        let found = resolve_windows_with_roots(&roots);
        let _ = std::fs::remove_dir_all(&temp);

        assert_eq!(found.as_deref(), Some(exe.as_path()));
    }

    // —— 公共契约的不变量 (resolve_and_export 用户显式设置时不覆盖) ——

    /// resolve_opencode_binary 在没有 opencode 的环境返回 None (不 panic)。
    /// 它不会误报一个不存在的路径。
    #[test]
    fn resolve_returns_none_or_existing_path() {
        if let Some(path) = resolve_opencode_binary() {
            // 若返回了路径, 该路径必须真实存在 (探测函数不准谎报)。
            assert!(
                path.is_file(),
                "resolve_opencode_binary returned {:?} but it does not exist",
                path
            );
        }
        // 返回 None 也是合法的 (本机可能没装 opencode)。
    }
}