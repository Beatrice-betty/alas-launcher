//! 开机自启模块：查询与设置启动器随系统自动启动的状态。
//!
//! 仅 Windows 提供完整实现——通过写入当前用户注册表的
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 键实现开机自启，
//! 并附加 `--start-minimized` 参数使启动后直接最小化到托盘。
//! 其他平台返回"不支持"状态，调用方据此在界面上隐藏相关选项。

use anyhow::{anyhow, Result};
use serde::Serialize;

/// 开机自启状态的查询结果。
///
/// 序列化后经 Tauri 命令返回给前端展示。
#[derive(Debug, Serialize)]
pub struct AutostartStatus {
    /// 当前自启项是否指向本启动器（即自启已生效）。
    pub enabled: bool,
    /// 当前平台是否支持开机自启（仅 Windows 为 true）。
    pub supported: bool,
    /// 注册表 Run 键中当前记录的原始命令行，未设置时为 `None`。
    pub value: Option<String>,
}

/// 注册表 Run 键中本启动器对应的值名。
#[cfg(windows)]
const RUN_VALUE_NAME: &str = "AzurPilot";

/// 当前用户的开机自启注册表键路径。
#[cfg(windows)]
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 自启命令附加的参数，使开机启动后直接最小化，不抢焦点。
#[cfg(windows)]
const START_MINIMIZED_ARG: &str = "--start-minimized";

/// 查询当前平台的开机自启状态。
///
/// # Errors
/// Windows 上读取注册表命令行失败（如无法定位当前可执行文件）时返回 Err；
/// 非 Windows 平台始终返回"不支持"的固定状态。
pub fn query() -> Result<AutostartStatus> {
    query_platform()
}

/// 启用或禁用开机自启，并返回设置后的最新状态。
///
/// # Errors
/// Windows 上创建注册表键或写入/删除值失败时返回 Err；
/// 非 Windows 平台一律返回 Err（不支持该功能）。
pub fn set_enabled(enabled: bool) -> Result<AutostartStatus> {
    set_enabled_platform(enabled)
}

/// Windows 实现：读取 Run 键并判断其是否等于当前启动器的自启命令。
///
/// 只有当注册表中的命令与"当前可执行文件 + `--start-minimized`"完全一致时
/// 才认为自启已启用，避免残留的旧路径或用户手动添加的条目造成误判。
#[cfg(windows)]
fn query_platform() -> Result<AutostartStatus> {
    let value = read_run_value()?;
    Ok(AutostartStatus {
        enabled: value
            .as_deref()
            .map(is_current_launcher_command)
            .unwrap_or(false),
        supported: true,
        value,
    })
}

/// Windows 实现：按需写入或删除注册表 Run 键，然后回查最新状态。
///
/// 启用时把"当前可执行文件 + `--start-minimized`"写入 Run 键；
/// 禁用时删除对应值（值不存在时静默忽略，保持幂等）。
#[cfg(windows)]
fn set_enabled_platform(enabled: bool) -> Result<AutostartStatus> {
    // create 会在键不存在时自动创建，保证写入路径总是可用
    let key = windows_registry::CURRENT_USER
        .create(RUN_KEY_PATH)
        .map_err(|e| anyhow!("{e:?}"))?;

    if enabled {
        let command = current_launcher_command()?;
        key.set_string(RUN_VALUE_NAME, &command)
            .map_err(|e| anyhow!("{e:?}"))?;
    } else {
        // 目标值已不存在时 remove_value 会报错，但禁用的目标状态已达成，忽略即可
        let _ = key.remove_value(RUN_VALUE_NAME);
    }

    // 统一走查询路径返回，确保调用方拿到的就是注册表的真实落盘结果
    query_platform()
}

/// 读取注册表 Run 键中本启动器的命令行值。
///
/// 键或值不存在时返回 `Ok(None)` 而非错误——"未设置自启"是正常状态，
/// 不应让调用方按异常处理。
#[cfg(windows)]
fn read_run_value() -> Result<Option<String>> {
    let key = match windows_registry::CURRENT_USER.open(RUN_KEY_PATH) {
        Ok(key) => key,
        Err(_) => return Ok(None),
    };
    match key.get_string(RUN_VALUE_NAME) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Ok(None),
    }
}

/// 判断注册表中的命令行是否等于当前启动器应写入的自启命令。
///
/// 比较前对两侧做归一化（见 [`normalize_command`]），以容忍
/// 路径分隔符方向、大小写和首尾空白等无意义的差异。
#[cfg(windows)]
fn is_current_launcher_command(value: &str) -> bool {
    let command = match current_launcher_command() {
        Ok(command) => command,
        Err(_) => return false,
    };
    normalize_command(value) == normalize_command(&command)
}

/// 构造当前启动器的自启命令行：带引号的完整路径 + 最小化启动参数。
///
/// 路径中的双引号会被转义，防止含空格或特殊字符的安装路径
/// 破坏命令行的引号结构。
#[cfg(windows)]
fn current_launcher_command() -> Result<String> {
    let exe = std::env::current_exe()?;
    let path = exe.to_string_lossy().replace('"', r#"\""#);
    Ok(format!(r#""{path}" {START_MINIMIZED_ARG}"#))
}

/// 归一化命令行字符串用于比较：去首尾空白、统一斜杠方向、转小写。
///
/// 注册表中的值可能由旧版本写入（如使用 `/` 分隔符或不同大小写），
/// 归一化后比较可避免对同一路径的重复写入。
#[cfg(windows)]
fn normalize_command(value: &str) -> String {
    value.trim().replace('/', "\\").to_ascii_lowercase()
}

/// 非 Windows 平台：固定返回"不支持"状态，便于前端隐藏自启开关。
#[cfg(not(windows))]
fn query_platform() -> Result<AutostartStatus> {
    Ok(AutostartStatus {
        enabled: false,
        supported: false,
        value: None,
    })
}

/// 非 Windows 平台：不支持设置开机自启，直接返回错误。
#[cfg(not(windows))]
fn set_enabled_platform(_enabled: bool) -> Result<AutostartStatus> {
    Err(anyhow!("Autostart is only supported on Windows"))
}
