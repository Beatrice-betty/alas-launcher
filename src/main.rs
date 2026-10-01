//! AzurPilot Launcher——AzurLaneAutoScript（碧蓝航线自动化脚本）的跨平台
//! （Windows/macOS/Linux）桌面启动器，Tauri 2 + Rust。本文件是应用入口，
//! 承担窗口管理（splash 启动画面 + 主窗口）、系统托盘、启动器自更新、
//! 时间炸弹与后端 ManagedBackend 的生命周期衔接。
//!
//! 启动流程概览：初始化日志（log/{日期}_launcher.txt）→ splash 窗口 →
//! 后台线程依次完成启动器自更新检查、setup_alas_repo 仓库环境准备、
//! ManagedBackend 启动 gui.py → SSE 通知流与反向控制流就绪后销毁 splash
//! 并把主窗口导航到 WebUI（默认端口 22267）。
//!
//! 关键机制：
//! - 自定义 URI 协议：alas-splash:// 提供启动画面页面，alas-error:// 提供
//!   后端连接失败的错误页面（Windows 上以 http://*.localhost 假域名实现）；
//! - 标题栏 JS 注入（page_load_injector）：注入红绿灯窗口按钮与拖拽区、
//!   覆盖 window.saveAs 改走 Tauri save_as 命令、阻止浏览器后退；
//! - 时间炸弹：运行时从内嵌 Cargo.toml 解析过期配置，经 HTTP Date 头获取
//!   网络时间比对，过期则弹窗并阻止启动；
//! - 三平台关闭行为：Windows 弹出窗口内"退出/最小化到托盘"选择菜单，
//!   macOS 最小化到托盘并隐藏 Dock 图标，Linux 直接隐藏窗口。

// 在 Windows 上不创建默认控制台窗口
#![windows_subsystem = "windows"]

/// 开机自启动管理。
mod autostart;
/// gui.py 后端子进程的托管（启动、就绪探测、终止等）。
mod backend;
/// 多语言（i18n）初始化与本地化支持。
mod i18n;
/// 启动器反向控制流：接收后端经 SSE 下发的退出等指令。
mod launcher_control;
/// Windows 运行时 Node.js 的检测与安装引导。
mod nodejs;
/// SSE 通知流：接收后端推送的系统通知与点击回调。
mod notify;
/// 首次启动环境准备（deploy.yaml、仓库克隆/更新、依赖同步）。
mod setup;
/// 窗口工具（控制台附着状态等平台辅助）。
mod window_util;

#[macro_use]
extern crate rust_i18n;
i18n!("locales", fallback = "en");

use std::{
    cell::Cell,
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread::{self},
    time::{Duration, Instant},
};

use crate::{
    backend::{is_backend_startup_timeout, ManagedBackend, WebuiLaunchConfig},
    launcher_control::start_launcher_control_stream,
    notify::{start_notify_stream, NotificationClickHandler},
    setup::{
        get_deploy_config, rebuild_venv_and_sync_dependencies, reset_venv_for_rebuild,
        setup_alas_repo, setup_environment, SplashUpdate,
    },
};
use anyhow::{anyhow, bail, Context, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use chrono::{DateTime, FixedOffset, Local, Utc};
use reqwest::{
    blocking::Client,
    header::{
        HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, CONTENT_RANGE, DATE, RANGE, USER_AGENT,
    },
    StatusCode,
};
use rust_i18n::t;
use serde::Deserialize;
use serde_json::to_string;
use sha2::{Digest, Sha256};
use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    webview::{PageLoadEvent, PageLoadPayload},
    Manager, State, Url, WebviewWindow,
};
use tauri_plugin_dialog::{DialogExt, FilePath};
#[cfg(windows)]
use tauri_plugin_dialog::{MessageDialogButtons, MessageDialogKind};
use tempfile::Builder as TempDirBuilder;
use tracing::{debug, error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

/// macOS 托盘图标（2x 分辨率 PNG）：Retina 屏幕菜单栏使用的高清版本。
#[cfg(target_os = "macos")]
const MENUBAR_ICON_2X: &[u8] = include_bytes!("../icons/menubar@2x.png");
/// macOS 托盘图标（1x 分辨率 PNG）：2x 图标解码失败时的降级备选。
#[cfg(target_os = "macos")]
const MENUBAR_ICON_1X: &[u8] = include_bytes!("../icons/menubar.png");
/// Windows 系统托盘图标（PNG）。使用内嵌字节而非文件路径，保证打包后
/// 即使资源缺失托盘图标也能正常加载。
#[cfg(windows)]
const WINDOWS_TRAY_ICON: &[u8] = include_bytes!("../icons/icon.png");
/// splash 启动画面背景视频（MP4），base64 内嵌进 alas-splash:// 页面。
const SPLASH_BG_VIDEO: &[u8] = include_bytes!("../bg/bg.mp4");
/// 进度条上的"进度头"装饰图（WebP），随进度百分比横向移动。
const SPLASH_PROGRESS_HEAD: &[u8] = include_bytes!("../bg/loading.webp");
/// 内嵌 MiSans 启动器字体（TTF），供 splash 与错误页面内联 @font-face 使用。
const MI_SANS_FONT: &[u8] = include_bytes!("../fonts/MiSansLauncher.ttf");
/// 后端端口探测的单次 TCP 连接超时：只做快速可达性判断，不宜过长。
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// 主窗口导航前等待后端就绪的总时限，超时后转入错误页面。
const BACKEND_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(10);
/// 错误页面地址基路径。Windows/Android 的 WebView 通过
/// http://<协议>.localhost 假域名实现自定义协议，故走 http 变体。
#[cfg(any(windows, target_os = "android"))]
const BACKEND_ERROR_URL_BASE: &str = "http://alas-error.localhost/backend";
/// 错误页面地址基路径的通用变体：直接使用自定义 alas-error:// 协议。
#[cfg(not(any(windows, target_os = "android")))]
const BACKEND_ERROR_URL_BASE: &str = "alas-error://localhost/backend";
/// splash 页面地址基路径（Windows/Android 走 http 假域名变体，原因同上）。
#[cfg(any(windows, target_os = "android"))]
const SPLASH_URL: &str = "http://alas-splash.localhost/";
/// splash 页面地址基路径的通用变体：自定义 alas-splash:// 协议。
#[cfg(not(any(windows, target_os = "android")))]
const SPLASH_URL: &str = "alas-splash://localhost/";
/// 时间炸弹配置源：编译期内嵌的 Cargo.toml 文本，运行时从中解析
/// package.metadata.alas-launcher.time-bomb 段，免于新增独立配置文件。
const TIME_BOMB_CONFIG_SOURCE: &str = include_str!("../Cargo.toml");
/// 测试专用的 tauri.conf.json 内嵌文本，用于校验 WebView 拖拽相关配置。
#[cfg(test)]
const TAURI_CONFIG_SOURCE: &str = include_str!("../tauri.conf.json");
/// 启动器更新清单主 URL：编译期由构建脚本经环境变量注入。
const LAUNCHER_UPDATE_URL: &str = env!("LAUNCHER_UPDATE_URL");
/// 主 URL 不可用时的更新清单回退 URL。
const LAUNCHER_UPDATE_FALLBACK_URL: &str =
    "https://ap.launcher-update.nanoda.work/updata/stable.json";
/// 存在该环境变量时跳过启动器更新检查（自更新重启后的新一轮启动会带上）。
const LAUNCHER_UPDATE_SKIP_ENV: &str = "AZURPILOT_SKIP_LAUNCHER_UPDATE";
/// 迷你版启动器的版本号：迷你版即使已是"最新"也必须升级到正式版。
const MINI_LAUNCHER_VERSION: &str = "0.0.1";
/// 下载更新载荷所需的 mTLS 客户端身份（PEM），构建脚本生成后内嵌；
/// 空内容表示未配置 mTLS。
const LAUNCHER_UPDATE_MTLS_IDENTITY: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/launcher_mtls_identity.pem"));
/// 更新请求使用的浏览器 User-Agent，附带 AZURPILOT_LAUNCHER_UPDATE 标识
/// 便于服务端识别流量来源。
const LAUNCHER_UPDATE_BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 AZURPILOT_LAUNCHER_UPDATE/2.0.4";
/// 并行下载更新载荷的最大连接数。
const LAUNCHER_UPDATE_MAX_CONNECTIONS: usize = 8;
/// 并行分片的最小字节数：载荷不足单片时退回单连接顺序下载。
const LAUNCHER_UPDATE_MIN_CHUNK_BYTES: u64 = 1024 * 1024;
/// 更新下载占用 splash 进度条的起始百分比（8 之前留给更新前的检查）。
const LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_START: u8 = 8;
/// 更新下载占用 splash 进度条的结束百分比（88 之后留给校验与重启）。
const LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_END: u8 = 88;
/// Windows 专用：存在该环境变量表示当前进程由更新助手拉起，启动时不
/// 附着父进程控制台（读取后立即删除，避免传递给子进程）。
#[cfg(windows)]
const LAUNCHER_UPDATE_NO_CONSOLE_ENV: &str = "AZURPILOT_NO_ATTACH_CONSOLE";
/// Windows 专用：更新助手模式的命令行标记，后跟目标 exe 路径与载荷路径。
#[cfg(windows)]
const LAUNCHER_UPDATE_APPLY_ARG: &str = "--apply-launcher-update";
/// 预览"跳过更新检查"模式的命令行参数：横线与斜杠两种风格都接受，
/// 便于测试时模拟"无更新可用"的启动路径。
const PREVIEW_NO_UPDATE_ARGS: &[&str] = &[
    "--preview-no-update",
    "--skip-update",
    "--no-update",
    "--disable-update",
    "/preview-no-update",
    "/skip-update",
    "/no-update",
];
/// 预览"启动即报错"模式的命令行参数，用于测试 splash 错误态的展示效果。
const PREVIEW_CRASH_ARGS: &[&str] = &[
    "--preview-crash",
    "--preview-error",
    "--crash-preview",
    "--error-preview",
    "/preview-crash",
    "/preview-error",
];
/// 请求"启动后最小化到托盘"的命令行参数。
const START_MINIMIZED_ARGS: &[&str] = &["--start-minimized", "/start-minimized"];

// ---------------------------------------------------------------------------
// 启动器信任免密登录
//
// 启动器为每次会话生成一个随机信任密钥，经环境变量 TRUST_SECRET_ENV 注入到
// gui.py 子进程。WebUI 启动后会据当前 --key / deploy.yaml Password 登记该
// 密钥；启动器窗口导航前先向后端换发一次性令牌，再进入 /launcher-login 页面
// 预置登录态实现免密。信任密钥与会话密钥解耦，其它浏览器仍走原密码门禁。
// 手动 gui.py 启动时密钥未注入，WebUI 端整体关闭该通道。
// ---------------------------------------------------------------------------
/// 注入 gui.py 子进程的免密信任密钥所用的环境变量名。
pub(crate) const TRUST_SECRET_ENV: &str = "ALAS_WEBUI_TRUST_SECRET";
/// 信任密钥的原始随机字节数（base64 编码后约 32 字符）。
const TRUST_SECRET_LENGTH: usize = 24;
/// 换发免密令牌的请求超时：失败即回退普通登录页，不能拖慢主窗口展示。
const TRUST_LOGIN_TIMEOUT: Duration = Duration::from_secs(3);

/// 本次会话的信任密钥缓存：首次调用时生成并全程复用，进程结束即失效。
static LAUNCHER_TRUST_SECRET: OnceLock<String> = OnceLock::new();

/// 生成（首次）并返回本次会话的启动器信任密钥。
pub(crate) fn launcher_trust_secret() -> &'static str {
    LAUNCHER_TRUST_SECRET.get_or_init(|| {
        use rand::RngCore;
        let mut bytes = [0u8; TRUST_SECRET_LENGTH];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        BASE64_STANDARD.encode(bytes)
    })
}

/// 计算主窗口应导航到的后端地址：若后端支持启动器免密（本机回环 + 密钥匹配），
/// 则返回带一次性令牌的 /launcher-login 页面；否则回退普通后端首页，维持原有
/// 登录行为。本函数不抛错，任何失败都静默回退。
fn webui_navigate_url(port: u16) -> String {
    let fallback = || backend_url(port);
    let secret = launcher_trust_secret();
    let client = match Client::builder()
        .timeout(TRUST_LOGIN_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(_) => return fallback(),
    };
    let response = client
        .post(format!("http://127.0.0.1:{port}/api/launcher/trusted-login"))
        .header("X-Webui-Launcher-Secret", secret)
        .send();
    let response = match response {
        Ok(response) if response.status().is_success() => response,
        _ => return fallback(),
    };
    // reqwest 未启用 json feature，手动解析 body。
    let body_text = match response.text() {
        Ok(body_text) => body_text,
        Err(_) => return fallback(),
    };
    let body: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(body) => body,
        Err(_) => return fallback(),
    };
    let Some(token) = body.get("token").and_then(|token| token.as_str()) else {
        return fallback();
    };
    if token.is_empty() {
        return fallback();
    }
    // 令牌为 URL-safe 随机串（secrets.token_urlsafe），可直接置于 query。
    format!("http://127.0.0.1:{port}/launcher-login?token={token}")
}

/// URL 的日志安全形式：只保留 scheme/host/port/path，剔除 query，避免
/// /launcher-login 的一次性令牌经日志落盘。
fn redacted_url_log(url: &Url) -> String {
    let Some(host) = url.host_str() else {
        return url.to_string();
    };
    let mut out = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out.push_str(url.path());
    out
}

/// Tauri 托管状态：包装"允许退出"标志，供 window_exit_application 命令置位，
/// 使 RunEvent::ExitRequested 分支放行真正的进程退出。
struct ExitControl(Arc<AtomicBool>);

/// 时间炸弹配置：到期时间、网络时间来源 URL 与过期提示文案。
///
/// 由 Cargo.toml 的 package.metadata.alas-launcher.time-bomb 段解析而来；
/// 未启用时间炸弹时整体缺省（None）。
#[derive(Clone, Debug)]
struct TimeBombConfig {
    /// 到期时间：RFC 3339 解析为带固定时区的时间，网络时间不早于该值即过期。
    expires_at: DateTime<FixedOffset>,
    /// 用于获取"可信当前时间"的 URL，取其 HTTP Date 响应头判断。
    network_time_url: String,
    /// 过期后弹窗展示的提示文案。
    message: String,
}

/// 启动器更新清单：最新版本号与各平台的下载载荷描述。
#[derive(Debug, Deserialize)]
struct LauncherUpdateManifest {
    /// 清单声明的最新启动器版本。
    version: String,
    /// 平台标识（如 windows-x86_64）到对应载荷的映射。
    platforms: HashMap<String, LauncherUpdatePlatform>,
}

/// 单个平台的更新载荷信息。
#[derive(Debug, Deserialize)]
struct LauncherUpdatePlatform {
    /// 载荷下载 URL（清单公开，载荷本身需 mTLS）。
    url: String,
    /// 载荷的 SHA-256 十六进制摘要，下载后用于完整性校验。
    sha256: String,
}

/// 并行下载的一个字节区间（闭区间 [start, end]）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LauncherUpdateByteRange {
    /// 区间起始字节偏移（含）。
    start: u64,
    /// 区间结束字节偏移（含）。
    end: u64,
}

/// 加载 macOS 托盘图标：优先 2x 高清版，失败则降级 1x。
///
/// # Panics
/// 两个内嵌图标字节均无法解码时 panic——没有图标时托盘无法创建。
#[cfg(target_os = "macos")]
fn tray_icon_for_platform() -> Image<'static> {
    info!("Loading macOS tray icon from embedded bytes...");
    let result = Image::from_bytes(MENUBAR_ICON_2X)
        .or_else(|_| {
            info!("2x icon failed, trying 1x...");
            Image::from_bytes(MENUBAR_ICON_1X)
        })
        .unwrap_or_else(|err| {
            error!(
                ?err,
                "Failed to load tray icon from embedded menubar icon bytes (2x and 1x)."
            );
            panic!("Failed to load tray icon from embedded menubar icon bytes: {err}");
        });
    info!("Tray icon loaded successfully");
    result
}

/// 加载 Windows 托盘图标（从内嵌 PNG 字节解码）。
///
/// # Panics
/// 图标字节解码失败时 panic——没有图标时托盘无法创建。
#[cfg(windows)]
fn tray_icon_for_platform() -> Image<'static> {
    Image::from_bytes(WINDOWS_TRAY_ICON).unwrap_or_else(|err| {
        error!(?err, "Failed to load tray icon from embedded icon bytes.");
        panic!("Failed to load tray icon from embedded icon bytes: {err}");
    })
}

/// 启动中断后的清理入口：提示用户、等待 setup 线程停止后重置 venv 并退出。
///
/// 多个退出路径（ExitRequested、splash 关闭）都可能触发，通过
/// startup_cleanup_started 原子标志保证只执行一次。清理成功后置 allow_exit
/// 并以退出码 0 结束应用；失败则在 splash 上显示错误并复位标志，允许重试。
fn begin_startup_cleanup(
    app_handle: tauri::AppHandle,
    allow_exit: Arc<AtomicBool>,
    setup_cancel_requested: Arc<AtomicBool>,
    setup_running: Arc<AtomicBool>,
    startup_cleanup_started: Arc<AtomicBool>,
) {
    // 幂等保护：清理只允许触发一次，后续并发的退出请求直接忽略。
    if startup_cleanup_started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    // 通知 setup 线程尽早取消，避免清理与初始化并发改写环境。
    setup_cancel_requested.store(true, Ordering::SeqCst);
    if let Some(splash) = app_handle.get_webview_window("splash") {
        update_splash(
            &splash,
            &SplashUpdate::loading(
                t!("dialog.cleaning_env"),
                t!("dialog.cleaning_env_detail"),
                99,
            )
            .with_subtitle(t!("dialog.cleaning_wait")),
        );
    }

    // 弹出说明对话框告知用户"正在清理环境"，此弹窗不阻塞主流程。
    app_handle
        .dialog()
        .message(t!("dialog.cleaning_message"))
        .title(t!("dialog.cleaning_env"))
        .show(|_| {});

    // 后台线程执行清理：先等 setup 线程退出（至多 30 秒，100ms 轮询）。
    thread::spawn(move || {
        let started_at = Instant::now();
        while setup_running.load(Ordering::SeqCst) && started_at.elapsed() < Duration::from_secs(30)
        {
            thread::sleep(Duration::from_millis(100));
        }

        // 超时仍未退出则记录告警，但继续清理，尽快让进程结束。
        if setup_running.load(Ordering::SeqCst) {
            warn!("Setup thread did not stop before startup cleanup timeout");
        }

        // 重置 venv：下次启动时重新同步依赖，修复可能损坏的运行环境。
        match reset_venv_for_rebuild() {
            Ok(()) => {
                info!("Startup cleanup finished; runtime will be rebuilt on next launch");
            }
            Err(e) => {
                error!("Startup cleanup failed: {:?}", e);
                if let Some(splash) = app_handle.get_webview_window("splash") {
                    update_splash(
                        &splash,
                        &SplashUpdate::error(
                            t!("dialog.cleanup_failed"),
                            t!("dialog.cleanup_failed_detail", error = format!("{e:#}")),
                            99,
                        ),
                    );
                }
                // 清理失败：复位标志允许再次触发，不放行退出。
                startup_cleanup_started.store(false, Ordering::SeqCst);
                return;
            }
        }

        // 清理完成后放行退出并结束进程。
        allow_exit.store(true, Ordering::SeqCst);
        app_handle.exit(0);
    });
}

/// 从内嵌 Cargo.toml 解析时间炸弹配置。
///
/// # Errors
/// 配置段声明 enabled 但缺少 expires-at，或时间格式不是合法 RFC 3339 时
/// 返回 Err；未启用或没有配置段时返回 Ok(None)。
fn time_bomb_config() -> Result<Option<TimeBombConfig>> {
    let Some(section) = cargo_toml_section("package.metadata.alas-launcher.time-bomb") else {
        return Ok(None);
    };
    // enabled 字段缺省按 false 处理：解析不到就当作未启用时间炸弹。
    let enabled = cargo_toml_value(section, "enabled")
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(false);
    if !enabled {
        return Ok(None);
    }

    let expires_at = cargo_toml_value(section, "expires-at")
        .ok_or_else(|| anyhow!(t!("errors.time_bomb_not_configured")))?;
    let expires_at = DateTime::parse_from_rfc3339(&expires_at)
        .map_err(|err| anyhow!(t!("errors.time_bomb_format_error", error = err.to_string())))?;
    // network-time-url 与 message 均有内置默认值，缺省也能工作。
    let network_time_url = cargo_toml_value(section, "network-time-url")
        .unwrap_or_else(|| "http://www.gstatic.com/generate_204".to_owned());
    let message = cargo_toml_value(section, "message")
        .unwrap_or_else(|| t!("errors.time_bomb_expired").to_string());

    Ok(Some(TimeBombConfig {
        expires_at,
        network_time_url,
        message,
    }))
}

/// 在内嵌 Cargo.toml 文本中定位 `[section_name]` 段，返回段体文本
/// （到下一个段头或文件末尾为止）。找不到段时返回 None。
///
/// 采用手写查找而非完整 TOML 库：仅需读取一个已知段，避免额外依赖。
fn cargo_toml_section(section_name: &str) -> Option<&'static str> {
    let header = format!("[{section_name}]");
    let start = TIME_BOMB_CONFIG_SOURCE.find(&header)? + header.len();
    let rest = &TIME_BOMB_CONFIG_SOURCE[start..];
    let end = rest.find("\n[").unwrap_or(rest.len());
    Some(&rest[..end])
}

/// 在 TOML 段文本中按行查找 `key = value`，返回去掉两侧引号后的字符串值。
///
/// 只处理单行简单键值：`#` 之后视为注释；找不到 key 时返回 None。
fn cargo_toml_value(section: &str, key: &str) -> Option<String> {
    for line in section.lines() {
        let line = line
            .split_once('#')
            .map(|(left, _)| left)
            .unwrap_or(line)
            .trim();
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        if left.trim() != key {
            continue;
        }
        let value = right.trim();
        return Some(
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value)
                .to_owned(),
        );
    }
    None
}

/// 判断时间炸弹是否已过期。
///
/// # Returns
/// 已过期返回 Some(配置的提示文案)；未配置或未过期返回 Ok(None)。
///
/// # Errors
/// 配置解析或网络时间获取失败时返回 Err（调用方会告警并放行启动）。
fn time_bomb_expiration_message() -> Result<Option<String>> {
    let Some(config) = time_bomb_config()? else {
        return Ok(None);
    };
    let network_time = fetch_network_time(&config.network_time_url)?;
    if network_time >= config.expires_at.with_timezone(&Utc) {
        Ok(Some(config.message))
    } else {
        Ok(None)
    }
}

/// 从指定 URL 的 HTTP Date 响应头获取"可信"网络时间。
///
/// 直接读取响应头而非解析响应体，可复用任意轻量端点（如 gstatic 的
/// generate_204），无需依赖专门的授时服务。
///
/// # Errors
/// 请求失败、响应缺少 Date 头或 Date 头不是合法 RFC 2822 时间时返回 Err。
fn fetch_network_time(url: &str) -> Result<DateTime<Utc>> {
    // 5 秒超时并禁用系统代理：取的是响应头时间，必须直连目标服务器，
    // 避免代理故障拖慢启动或干扰结果。
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()?;
    let response = client.get(url).send()?;
    let date_header = response
        .headers()
        .get(DATE)
        .ok_or_else(|| anyhow!(t!("errors.network_time_missing")))?
        .to_str()?;
    Ok(DateTime::parse_from_rfc2822(date_header)?.with_timezone(&Utc))
}

/// 构造更新请求的公共 HTTP 头：伪装成 Chrome 浏览器并附带语言偏好，
/// 以通过 CDN/网关的默认访问规则。
fn launcher_update_browser_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(LAUNCHER_UPDATE_BROWSER_UA),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/octet-stream,application/json,text/plain,*/*;q=0.8"),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
    );
    headers
}

/// 构造启动器更新专用的 reqwest 阻塞客户端。
///
/// # Errors
/// 客户端构建失败（如 mTLS 身份解析出错）时返回 Err。
fn launcher_update_http_client(
    timeout: Option<Duration>,
    with_mtls_identity: bool,
) -> Result<Client> {
    // 15 秒连接超时兜底；整体超时由调用方按用途传入。
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .no_proxy()
        .default_headers(launcher_update_browser_headers());
    // 仅请求更新载荷时携带 mTLS 身份；清单是公开数据，无需证书。
    if with_mtls_identity && !LAUNCHER_UPDATE_MTLS_IDENTITY.is_empty() {
        builder = builder.identity(reqwest::Identity::from_pem(LAUNCHER_UPDATE_MTLS_IDENTITY)?);
    }
    // 传 None 表示不限整体超时（大文件下载按块读取，不能设死总时长）。
    builder = match timeout {
        Some(timeout) => builder.timeout(timeout),
        None => builder.timeout(None),
    };
    Ok(builder.build()?)
}

/// 拉取启动器更新清单：先试主 URL，失败后回退备用 URL。
///
/// # Errors
/// 所有 URL 均失败时返回 Err。
fn fetch_launcher_update_manifest(client: &Client) -> Result<LauncherUpdateManifest> {
    fetch_launcher_update_manifest_from_urls(
        client,
        &[LAUNCHER_UPDATE_URL, LAUNCHER_UPDATE_FALLBACK_URL],
    )
}

/// 依次尝试给定的清单 URL 列表，取第一个成功的结果。
///
/// # Errors
/// 全部 URL 都失败时返回 Err，错误信息累积了各 URL 的失败详情，便于
/// 同时排查主/备两个源的问题。
fn fetch_launcher_update_manifest_from_urls(
    client: &Client,
    urls: &[&str],
) -> Result<LauncherUpdateManifest> {
    let mut failures = Vec::new();

    for (index, url) in urls.iter().enumerate() {
        // 跳过重复 URL（主备地址可能配置成同一个）。
        if urls[..index].iter().any(|previous| previous == url) {
            continue;
        }

        match fetch_launcher_update_manifest_from_url(client, url) {
            Ok(manifest) => {
                if index > 0 {
                    info!("Using fallback launcher update manifest: {url}");
                }
                return Ok(manifest);
            }
            Err(error) => {
                warn!("Unable to fetch launcher update manifest from {url}: {error:#}");
                failures.push(format!("{url}: {error:#}"));
            }
        }
    }

    bail!(
        "Unable to fetch launcher update manifest from all configured URLs: {}",
        failures.join("; ")
    )
}

/// 从单个 URL 拉取并解析启动器更新清单 JSON。
///
/// # Errors
/// 请求失败、非 2xx 状态、读取响应体或 JSON 解析任一步失败时返回 Err。
fn fetch_launcher_update_manifest_from_url(
    client: &Client,
    url: &str,
) -> Result<LauncherUpdateManifest> {
    let manifest_text = client
        .get(url)
        .send()
        .with_context(|| format!("request launcher update manifest from {url}"))?
        .error_for_status()
        .with_context(|| format!("validate launcher update manifest response from {url}"))?
        .text()
        .with_context(|| format!("read launcher update manifest from {url}"))?;
    serde_json::from_str(&manifest_text)
        .with_context(|| format!("parse launcher update manifest from {url}"))
}

/// 判断给定版本号是否为迷你版启动器（仅版本 0.0.1，忽略 v 前缀）。
fn launcher_version_is_mini(version: &str) -> bool {
    version.strip_prefix('v').unwrap_or(version) == MINI_LAUNCHER_VERSION
}

/// 检查启动器自更新；发现新版本时下载、校验、替换自身并重启进程。
///
/// 进度经 status_updater 回调推送到 splash 进度条。返回 Ok(true) 表示已
/// 拉起新版本进程（原进程应尽快退出交棒）；Ok(false) 表示已是最新版本。
///
/// # Errors
/// 清单拉取失败、平台无载荷、版本号非法、下载或校验失败、替换重启失败
/// 时返回 Err；迷你版发现"已是最新"同样视为错误（必须升级到正式版）。
fn check_launcher_update_and_restart(mut status_updater: impl FnMut(SplashUpdate)) -> Result<bool> {
    // 自更新重启后的新一轮启动带有跳过标记：取下标记并直接放行。
    if std::env::var_os(LAUNCHER_UPDATE_SKIP_ENV).is_some() {
        info!("Skipping launcher update check after restart");
        std::env::remove_var(LAUNCHER_UPDATE_SKIP_ENV);
        return Ok(false);
    }

    // 当前版本、迷你版标记与平台标识共同决定检查结果与下载载荷。
    let current_version = env!("CARGO_PKG_VERSION");
    let mini_launcher = launcher_version_is_mini(current_version);
    let platform_key = launcher_update_platform_key();
    let manifest_client = match launcher_update_http_client(Some(Duration::from_secs(10)), false) {
        Ok(client) => client,
        Err(err) => {
            warn!("Unable to create launcher update client: {err:#}");
            return Err(anyhow!(t!(
                "launcher_update.check_failed",
                error = format!("{err:#}")
            )));
        }
    };
    let manifest = match fetch_launcher_update_manifest(&manifest_client) {
        Ok(manifest) => manifest,
        Err(err) => {
            return Err(anyhow!(t!(
                "launcher_update.check_failed",
                error = format!("{err:#}")
            )));
        }
    };
    let update_available = launcher_version_is_newer(current_version, &manifest.version)
        .ok_or_else(|| {
            warn!(
                "Launcher update manifest contains an invalid version: {}",
                manifest.version
            );
            anyhow!(t!(
                "launcher_update.invalid_manifest_version",
                version = manifest.version.clone()
            ))
        })?;
    if !update_available {
        info!(
            "Launcher is up to date: current={}, latest={}",
            current_version, manifest.version
        );
        // 迷你版不允许停留：即使无更新也报错，提示安装正式版。
        if mini_launcher {
            return Err(anyhow!(t!(
                "launcher_update.mini_update_missing",
                current = current_version,
                latest = manifest.version
            )));
        }
        return Ok(false);
    }

    // 清单中没有当前平台的载荷：无法更新，报错提示。
    let Some(platform) = manifest.platforms.get(platform_key) else {
        warn!("No launcher update payload for platform {platform_key}");
        return Err(anyhow!(t!(
            "launcher_update.payload_missing",
            platform = platform_key
        )));
    };

    info!(
        "Launcher update available: {} -> {}",
        current_version, manifest.version
    );
    status_updater(
        SplashUpdate::loading(
            t!("launcher_update.updating"),
            t!(
                "launcher_update.available_detail",
                version = manifest.version.clone()
            ),
            8,
        )
        .with_subtitle(t!("launcher_update.status")),
    );

    // 下载到临时路径，校验通过后再替换自身并重启。
    let current_exe = std::env::current_exe()?;
    let update_path = launcher_update_temp_path(&current_exe);
    if let Err(err) = download_launcher_update(
        &platform.url,
        &update_path,
        &platform.sha256,
        &mut status_updater,
    ) {
        warn!("Launcher update download failed: {err:#}");
        return Err(err);
    }
    // 非 Windows 平台需补执行权限（Windows 的可执行性由 .exe 扩展名决定）。
    make_executable(&update_path)?;
    status_updater(
        SplashUpdate::loading(
            t!("launcher_update.restart_title"),
            t!("launcher_update.restarting_detail"),
            100,
        )
        .with_subtitle(t!("launcher_update.restart_status")),
    );
    // 替换自身并拉起新进程；新进程带跳过标记，不再重复检查更新。
    if let Err(err) = replace_launcher_and_restart(&current_exe, &update_path) {
        warn!("Launcher update replacement failed: {err:#}");
        return Err(err);
    }
    Ok(true)
}

/// 下载启动器更新载荷到本地临时路径，并校验 SHA-256。
///
/// 先探测服务器是否支持 HTTP Range：支持且载荷足够大时多线程并行分片
/// 下载，否则退回单连接顺序下载。下载或校验失败时清理残留文件。
///
/// # Errors
/// 载荷 URL 校验失败、下载中断、合并后字节数不符或 SHA-256 不匹配时
/// 返回 Err。
fn download_launcher_update(
    url: &str,
    update_path: &Path,
    expected_sha256: &str,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    validate_launcher_update_payload(url, expected_sha256)?;

    // 载荷 URL 来自公开清单；ESA 边缘节点要求载荷请求携带 mTLS 客户端证书。
    let client = launcher_update_http_client(None, true)?;
    // 预清理残留的 .part 与目标文件，保证本次下载从零开始。
    let part_path = launcher_update_part_path(update_path);
    remove_launcher_update_file_if_exists(&part_path)?;
    remove_launcher_update_file_if_exists(update_path)?;

    info!("Downloading launcher update from {url}");
    // 先用 bytes=0-0 探测 Range 支持并获取总字节数，再决定下载策略。
    let range_total = launcher_update_range_total(&client, url)?;
    let download_result = match range_total {
        Some(total_bytes) => {
            let ranges = launcher_update_byte_ranges(total_bytes);
            // 载荷大于一个分片才值得并行，小文件单连接更快更省事。
            if ranges.len() > 1 {
                status_updater(
                    SplashUpdate::loading(
                        t!("launcher_update.updating"),
                        t!(
                            "launcher_update.parallel_downloading_detail",
                            connections = ranges.len().to_string()
                        ),
                        LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_START,
                    )
                    .with_subtitle(t!("launcher_update.status")),
                );
                download_launcher_update_parallel(
                    &client,
                    url,
                    total_bytes,
                    &ranges,
                    &part_path,
                    &mut status_updater,
                )
            } else {
                info!("Launcher update payload is too small for parallel download");
                download_launcher_update_sequential(
                    &client,
                    url,
                    &part_path,
                    Some(total_bytes),
                    &mut status_updater,
                )
            }
        }
        None => {
            info!("Launcher update server does not support HTTP byte ranges; using one connection");
            download_launcher_update_sequential(&client, url, &part_path, None, &mut status_updater)
        }
    };
    // 下载失败时清理 .part 与目标文件，避免留下损坏的半成品。
    let _downloaded = match download_result {
        Ok(downloaded) => downloaded,
        Err(err) => {
            cleanup_launcher_update_download_files(&part_path, update_path);
            return Err(err);
        }
    };

    status_updater(
        SplashUpdate::loading(
            t!("launcher_update.updating"),
            t!("launcher_update.verifying_detail"),
            92,
        )
        .with_subtitle(t!("launcher_update.status")),
    );

    // 下载完成后进入 SHA-256 校验与落地阶段（.part 重命名为正式文件）。
    let downloaded =
        match verify_and_promote_launcher_update(&part_path, update_path, expected_sha256) {
            Ok(downloaded) => downloaded,
            Err(err) => {
                cleanup_launcher_update_download_files(&part_path, update_path);
                return Err(err);
            }
        };

    info!(
        "Launcher update downloaded: {} bytes -> {}",
        downloaded,
        update_path.display()
    );
    Ok(())
}

/// 校验更新载荷 URL 与摘要格式：仅接受带主机的 HTTPS URL 与 64 位十六进制
/// SHA-256 摘要，防止清单被篡改成明文 HTTP 或本地地址。
///
/// # Errors
/// URL 不是 https、缺少主机名或摘要格式非法时返回 Err。
fn validate_launcher_update_payload(url: &str, expected_sha256: &str) -> Result<()> {
    let parsed_url =
        Url::parse(url).with_context(|| format!("invalid launcher update URL: {url}"))?;
    if parsed_url.scheme() != "https" || parsed_url.host_str().is_none() {
        bail!("launcher update URL must use HTTPS and include a host: {url}");
    }
    if !launcher_update_sha256_is_valid(expected_sha256) {
        bail!("launcher update manifest contains an invalid SHA-256 digest");
    }
    Ok(())
}

/// 判断字符串是否为合法的 SHA-256 十六进制摘要（64 个十六进制字符）。
fn launcher_update_sha256_is_valid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 探测更新载荷服务器是否支持 HTTP Range，并取得载荷总字节数。
///
/// 发送 bytes=0-0 的探测请求；仅当响应为 206 且 Content-Range 表明总大小
/// 有效时返回 Some(total)。
///
/// # Errors
/// 探测请求本身失败（网络错误、非 2xx）时返回 Err；服务器不支持 Range
/// 时返回 Ok(None)。
fn launcher_update_range_total(client: &Client, url: &str) -> Result<Option<u64>> {
    let response = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .send()?
        .error_for_status()?;
    // 非 206 说明服务器忽略了 Range，无法并行，也拿不到可靠总大小。
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Ok(None);
    }

    let Some((start, end, total)) = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_launcher_update_content_range)
    else {
        return Ok(None);
    };
    // 探测请求只取 1 字节：start/end 必须都是 0 才能据此信任 total。
    if start != 0 || end != 0 || total == 0 {
        return Ok(None);
    }
    Ok(Some(total))
}

/// 解析 Content-Range 头（形如 `bytes 10-19/42`）。
///
/// # Returns
/// 合法且满足 start <= end < total 时返回 Some((start, end, total))；
/// 其余情况（含通配 `*`）返回 None。
fn parse_launcher_update_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.trim().strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    let total = total.parse().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

/// 把载荷总字节数均匀切分为若干连续字节区间。
///
/// 区间数量按"每片至少 LAUNCHER_UPDATE_MIN_CHUNK_BYTES"向上取整，并夹在
/// 1 与 LAUNCHER_UPDATE_MAX_CONNECTIONS 之间；整除余数逐片分摊给前面的
/// 区间，保证各区间首尾相接且恰好覆盖整个载荷。
fn launcher_update_byte_ranges(total_bytes: u64) -> Vec<LauncherUpdateByteRange> {
    if total_bytes == 0 {
        return Vec::new();
    }

    // 分片数 = 向上取整(总量/单片最小值)，并夹在 [1, MAX_CONNECTIONS] 之间。
    let range_count = total_bytes
        .saturating_add(LAUNCHER_UPDATE_MIN_CHUNK_BYTES - 1)
        .checked_div(LAUNCHER_UPDATE_MIN_CHUNK_BYTES)
        .unwrap_or(1)
        .clamp(1, LAUNCHER_UPDATE_MAX_CONNECTIONS as u64) as usize;
    let base_size = total_bytes / range_count as u64;
    let extra_bytes = total_bytes % range_count as u64;
    let mut start = 0;
    let mut ranges = Vec::with_capacity(range_count);

    for index in 0..range_count {
        let size = base_size + u64::from(index < extra_bytes as usize);
        let end = start + size - 1;
        ranges.push(LauncherUpdateByteRange { start, end });
        start = end + 1;
    }
    ranges
}

/// 多线程并行下载各字节区间到临时目录的分片文件，随后按序合并。
///
/// 每个分片由独立线程下载，经 mpsc 通道上报进度；主循环每 100ms 汇聚一次
/// 增量并节流刷新 splash。全部完成后校验总字节数一致才执行合并。
///
/// # Errors
/// 任一 worker panic、分片字节数合计与总大小不符或合并失败时返回 Err。
fn download_launcher_update_parallel(
    client: &Client,
    url: &str,
    total_bytes: u64,
    ranges: &[LauncherUpdateByteRange],
    part_path: &Path,
    status_updater: &mut impl FnMut(SplashUpdate),
) -> Result<u64> {
    // 分片先写入独立临时目录，全部校验通过后才合并到正式 .part 路径。
    let temp_dir = TempDirBuilder::new()
        .prefix("azurpilot-launcher-update-")
        .tempdir()
        .context("create temporary launcher update download directory")?;
    let (progress_sender, progress_receiver) = mpsc::channel();
    let mut workers = Vec::with_capacity(ranges.len());
    let mut chunk_paths = Vec::with_capacity(ranges.len());

    // 每个区间一个 worker 线程，独立持有客户端克隆、URL 与分片路径。
    for (index, range) in ranges.iter().copied().enumerate() {
        let chunk_path = temp_dir.path().join(format!("chunk-{index:02}"));
        let worker_client = client.clone();
        let worker_url = url.to_owned();
        let worker_path = chunk_path.clone();
        let worker_sender = progress_sender.clone();
        workers.push(thread::spawn(move || {
            download_launcher_update_range(
                &worker_client,
                &worker_url,
                range,
                &worker_path,
                &worker_sender,
            )
        }));
        chunk_paths.push(chunk_path);
    }
    // 主动丢弃主发送端：worker 全部结束后通道断开，主循环得以退出。
    drop(progress_sender);

    let started_at = Instant::now();
    let mut downloaded_so_far = 0u64;
    let mut last_reported_progress = LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_START;
    let mut last_reported_at = Instant::now() - Duration::from_secs(1);
    // 每 100ms 醒一次：汇总各 worker 上报的进度增量并节流刷新 splash。
    while workers.iter().any(|worker| !worker.is_finished()) {
        match progress_receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(downloaded) => {
                downloaded_so_far = downloaded_so_far.saturating_add(downloaded);
                report_launcher_update_download_progress(
                    status_updater,
                    downloaded_so_far,
                    total_bytes,
                    started_at,
                    &mut last_reported_progress,
                    &mut last_reported_at,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // worker 收尾后再清空一次通道，避免漏计尾部进度。
    while let Ok(downloaded) = progress_receiver.try_recv() {
        downloaded_so_far = downloaded_so_far.saturating_add(downloaded);
    }

    // join 各 worker 并累计字节数：任一 worker panic 视为整体失败。
    let mut completed_bytes = 0u64;
    for worker in workers {
        completed_bytes = completed_bytes.saturating_add(
            worker
                .join()
                .map_err(|_| anyhow!("launcher update download worker panicked"))??,
        );
    }
    if completed_bytes != total_bytes {
        bail!(
            "launcher update download incomplete: expected {} bytes, got {} bytes",
            total_bytes,
            completed_bytes
        );
    }

    // 合并前把进度刷满下载区间，避免 UI 停在中间值。
    report_launcher_update_download_progress(
        status_updater,
        total_bytes,
        total_bytes,
        started_at,
        &mut last_reported_progress,
        &mut last_reported_at,
    );
    let merged_bytes = merge_launcher_update_chunks(&chunk_paths, part_path)?;
    if merged_bytes != total_bytes {
        bail!(
            "launcher update merge incomplete: expected {} bytes, got {} bytes",
            total_bytes,
            merged_bytes
        );
    }
    Ok(merged_bytes)
}

/// 下载单个字节区间到指定分片文件。
///
/// 严格校验响应：必须为 206、Content-Range 与请求区间一致、Content-Length
/// （如存在）等于区间长度，防止服务器忽略 Range 导致分片错位。
///
/// # Errors
/// 校验失败、写文件失败或实际下载数与区间长度不符时返回 Err。
fn download_launcher_update_range(
    client: &Client,
    url: &str,
    range: LauncherUpdateByteRange,
    chunk_path: &Path,
    progress_sender: &mpsc::Sender<u64>,
) -> Result<u64> {
    let requested_range = format!("bytes={}-{}", range.start, range.end);
    let mut response = client
        .get(url)
        .header(RANGE, requested_range)
        .send()?
        .error_for_status()?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        bail!(
            "launcher update server ignored byte range {}-{}",
            range.start,
            range.end
        );
    }

    let Some((start, end, _)) = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_launcher_update_content_range)
    else {
        bail!("launcher update range response is missing a valid Content-Range header");
    };
    if start != range.start || end != range.end {
        bail!(
            "launcher update range response does not match requested bytes {}-{}",
            range.start,
            range.end
        );
    }

    // 期望字节数即闭区间长度（两端均含）。
    let expected_bytes = range.end - range.start + 1;
    if response
        .content_length()
        .is_some_and(|content_length| content_length != expected_bytes)
    {
        bail!(
            "launcher update range response has wrong length for bytes {}-{}",
            range.start,
            range.end
        );
    }

    // 边读边写分片文件，并把每次写入的字节数经通道上报给进度汇总。
    let mut file = fs::File::create(chunk_path)?;
    let mut downloaded = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let size = response.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        file.write_all(&buffer[..size])?;
        downloaded += size as u64;
        let _ = progress_sender.send(size as u64);
    }
    file.flush()?;

    // 最终核对：实际下载数必须与区间长度完全一致。
    if downloaded != expected_bytes {
        bail!(
            "launcher update range download incomplete for bytes {}-{}: expected {} bytes, got {} bytes",
            range.start,
            range.end,
            expected_bytes,
            downloaded
        );
    }
    Ok(downloaded)
}

/// 按顺序把各分片文件合并写入目标 .part 文件。
///
/// # Errors
/// 创建输出文件、读取分片或写盘失败时返回 Err。
fn merge_launcher_update_chunks(chunk_paths: &[PathBuf], part_path: &Path) -> Result<u64> {
    let mut output = fs::File::create(part_path).with_context(|| {
        t!(
            "errors.write_update_failed",
            error = part_path.display().to_string()
        )
    })?;
    let mut written = 0u64;
    for chunk_path in chunk_paths {
        let mut chunk = fs::File::open(chunk_path)?;
        written = written.saturating_add(std::io::copy(&mut chunk, &mut output)?);
    }
    output.flush().with_context(|| {
        t!(
            "errors.write_update_failed",
            error = part_path.display().to_string()
        )
    })?;
    Ok(written)
}

/// 汇总并行下载进度并节流刷新 splash：进度百分比提升时立即刷新，否则
/// 至多每 250ms 刷新一次，避免高频更新拖慢 UI。
fn report_launcher_update_download_progress(
    status_updater: &mut impl FnMut(SplashUpdate),
    downloaded: u64,
    total_bytes: u64,
    started_at: Instant,
    last_reported_progress: &mut u8,
    last_reported_at: &mut Instant,
) {
    let (progress, detail) =
        launcher_download_progress_detail(downloaded, Some(total_bytes), started_at);
    if progress > *last_reported_progress
        || last_reported_at.elapsed() >= Duration::from_millis(250)
    {
        *last_reported_progress = progress;
        *last_reported_at = Instant::now();
        status_updater(
            SplashUpdate::loading(t!("launcher_update.updating"), detail, progress)
                .with_subtitle(t!("launcher_update.status")),
        );
    }
}

/// 校验 .part 文件的 SHA-256，通过后重命名为正式更新文件。
///
/// # Errors
/// 摘要不匹配（同时删除 .part）、读取元数据或重命名失败时返回 Err。
fn verify_and_promote_launcher_update(
    part_path: &Path,
    update_path: &Path,
    expected_sha256: &str,
) -> Result<u64> {
    let digest_hex = sha256_file(part_path)?;
    if !digest_hex.eq_ignore_ascii_case(expected_sha256) {
        // 摘要不匹配：删除半成品，避免下次误用。
        let _ = fs::remove_file(part_path);
        bail!(
            "launcher update sha256 mismatch: expected {}, got {}",
            expected_sha256,
            digest_hex
        );
    }

    let downloaded = fs::metadata(part_path)?.len();
    remove_launcher_update_file_if_exists(update_path)?;
    // 同目录 rename 原子性较好：校验通过后"晋升"为正式更新文件。
    fs::rename(part_path, update_path).with_context(|| {
        format!(
            "promote verified launcher update from {} to {}",
            part_path.display(),
            update_path.display()
        )
    })?;
    Ok(downloaded)
}

/// 由最终更新文件路径推导下载中的临时 .part 路径（同名追加 .part 后缀）。
fn launcher_update_part_path(update_path: &Path) -> PathBuf {
    let Some(file_name) = update_path.file_name() else {
        return update_path.with_extension("part");
    };
    let mut part_name = file_name.to_os_string();
    part_name.push(".part");
    update_path.with_file_name(part_name)
}

/// 删除指定文件；文件不存在视为成功。
///
/// # Errors
/// 删除时发生非 NotFound 的 IO 错误时返回 Err。
fn remove_launcher_update_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// 清理下载失败后的 .part 与目标文件；清理失败仅记录告警，不中断调用方
/// 的错误处理流程。
fn cleanup_launcher_update_download_files(part_path: &Path, update_path: &Path) {
    for path in [part_path, update_path] {
        if let Err(error) = remove_launcher_update_file_if_exists(path) {
            warn!(
                "Unable to clean launcher update download {}: {error}",
                path.display()
            );
        }
    }
}

/// 单连接顺序下载更新载荷到指定路径，边下载边节流上报进度。
///
/// # Errors
/// 请求失败、写盘失败或与预期总字节数不符时返回 Err。
fn download_launcher_update_sequential(
    client: &Client,
    url: &str,
    update_path: &Path,
    expected_total_bytes: Option<u64>,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<u64> {
    let mut response = client.get(url).send()?.error_for_status()?;
    // 优先使用 Range 探测得到的总大小，其次取响应 Content-Length。
    let total_bytes = expected_total_bytes.or_else(|| response.content_length());
    let mut file = fs::File::create(update_path).with_context(|| {
        t!(
            "errors.write_update_failed",
            error = update_path.display().to_string()
        )
    })?;
    let mut downloaded = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    let mut last_reported_progress = LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_START;
    let mut last_reported_at = Instant::now() - Duration::from_secs(1);
    let download_started_at = Instant::now();

    loop {
        let size = response
            .read(&mut buffer)
            .with_context(|| t!("errors.download_update_failed", url = url))?;
        if size == 0 {
            break;
        }
        file.write_all(&buffer[..size]).with_context(|| {
            t!(
                "errors.write_update_failed",
                error = update_path.display().to_string()
            )
        })?;
        downloaded += size as u64;

        // 进度刷新节流：百分比提升立即刷，否则至多每 250ms 一次。
        let (progress, detail) =
            launcher_download_progress_detail(downloaded, total_bytes, download_started_at);
        if progress > last_reported_progress
            || last_reported_at.elapsed() >= Duration::from_millis(250)
        {
            last_reported_progress = progress;
            last_reported_at = Instant::now();
            status_updater(
                SplashUpdate::loading(t!("launcher_update.updating"), detail, progress)
                    .with_subtitle(t!("launcher_update.status")),
            );
        }
    }
    file.flush().with_context(|| {
        t!(
            "errors.write_update_failed",
            error = update_path.display().to_string()
        )
    })?;

    // 已知总大小时核对下载完整性。
    if let Some(total_bytes) = total_bytes {
        if downloaded != total_bytes {
            return Err(anyhow!(
                "launcher update download incomplete: expected {} bytes, got {} bytes",
                total_bytes,
                downloaded
            ));
        }
    }

    Ok(downloaded)
}

/// 分块计算指定文件的 SHA-256 摘要，返回小写十六进制字符串。
///
/// # Errors
/// 文件打开或读取失败时返回 Err。
fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];

    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hasher.update(&buffer[..size]);
    }

    let digest = hasher.finalize();
    Ok(bytes_to_hex(&digest))
}

/// 根据已下载字节数生成 splash 进度值与详情文案。
///
/// 已知总大小时把下载百分比线性映射进 [START, END] 进度区间；未知时按
/// 每 MiB 一格推进并封顶在 END，文案改用"已下载/未知总量"版本。
fn launcher_download_progress_detail(
    downloaded: u64,
    total_bytes: Option<u64>,
    started_at: Instant,
) -> (u8, String) {
    let speed = format_speed(download_speed_bytes_per_second(downloaded, started_at));
    if let Some(total) = total_bytes.filter(|total| *total > 0) {
        let percentage = (downloaded.min(total).saturating_mul(100) / total) as u8;
        let detail = t!(
            "launcher_update.downloading_detail",
            downloaded = format_bytes(downloaded),
            total = format_bytes(total),
            percent = percentage.to_string(),
            speed = speed
        )
        .to_string();
        let progress_span =
            LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_END - LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_START;
        let progress = LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_START
            + ((u16::from(percentage) * u16::from(progress_span)) / 100) as u8;
        return (progress, detail);
    }

    let mib_downloaded = downloaded / (1024 * 1024);
    let progress = (LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_START
        + mib_downloaded.min(u64::from(
            LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_END - LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_START,
        )) as u8)
        .min(LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_END);
    let detail = t!(
        "launcher_update.downloading_detail_unknown",
        downloaded = format_bytes(downloaded),
        speed = speed
    )
    .to_string();
    (progress, detail)
}

/// 把字节数格式化为人类可读字符串（B/KiB/MiB/GiB，保留 1 位小数）。
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// 计算平均下载速度（字节/秒）；耗时不足 0.1 秒时按 0.1 秒折算，避免除零。
fn download_speed_bytes_per_second(downloaded: u64, started_at: Instant) -> f64 {
    let elapsed = started_at.elapsed().as_secs_f64().max(0.1);
    downloaded as f64 / elapsed
}

/// 把速度（字节/秒）四舍五入后复用字节数格式化输出。
fn format_speed(bytes_per_second: f64) -> String {
    format_bytes(bytes_per_second.max(0.0).round() as u64)
}

/// 返回当前操作系统与架构对应的更新清单平台标识（如 windows-x86_64）。
/// 未识别的组合返回 "unknown"（清单中无该载荷，更新会报缺失）。
fn launcher_update_platform_key() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "x86") => "windows-i686",
        ("windows", "aarch64") => "windows-aarch64",
        _ => "unknown",
    }
}

/// 比较版本号，判断 latest 是否严格大于 current。
///
/// # Returns
/// 任一版本号无法解析时返回 None（调用方按无效清单处理）。
fn launcher_version_is_newer(current: &str, latest: &str) -> Option<bool> {
    let current = parse_launcher_version(current)?;
    let latest = parse_launcher_version(latest)?;
    Some(latest > current)
}

/// 解析启动器版本号为可比较的四元组 (major, minor, patch, 后缀序号)。
///
/// 支持 `v` 前缀、`-` 预发布后缀与 `+` 构建元数据；后缀序号取预发布段
/// 末尾的连续数字（如 build.3 -> 3，无数字视为 0），实现 alpha/beta 之类
/// 预发布版本间的次序比较。
///
/// # Returns
/// 格式非法（缺段、段非数字、后缀非法）时返回 None。
fn parse_launcher_version(version: &str) -> Option<(u64, u64, u64, u64)> {
    let version = version.strip_prefix('v').unwrap_or(version);
    // 构建元数据（+ 后段）只做合法性校验，不参与比较。
    let version = match version.split_once('+') {
        Some((version, build_metadata)) if valid_launcher_version_suffix(build_metadata) => version,
        Some(_) => return None,
        None => version,
    };
    let (core, suffix) = version.split_once('-').unwrap_or((version, ""));
    if version.contains('-') && !valid_launcher_version_suffix(suffix) {
        return None;
    }
    let mut nums = core.split('.');
    let major = nums.next()?.parse::<u64>().ok()?;
    let minor = nums.next()?.parse::<u64>().ok()?;
    let patch = nums.next()?.parse::<u64>().ok()?;
    if nums.next().is_some() {
        return None;
    }
    // 后缀序号：取预发布段末尾连续数字并倒序重组（如 rc12 -> 12）。
    let suffix_rank = suffix
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .parse::<u64>()
        .unwrap_or(0);
    Some((major, minor, patch, suffix_rank))
}

/// 判断语义化版本后缀是否合法：非空且以 `.` 分隔的各段均由 ASCII 字母、
/// 数字或连字符组成。
fn valid_launcher_version_suffix(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
}

/// 检查命令行参数（跳过 argv[0]）中是否出现给定标志之一（忽略大小写）。
fn launcher_arg_present(flags: &[&str]) -> bool {
    std::env::args().skip(1).any(|arg| {
        let arg = arg.to_ascii_lowercase();
        flags.iter().any(|flag| arg == *flag)
    })
}

/// 是否以"跳过更新检查"预览模式启动（用于测试无更新时的启动路径）。
fn preview_no_update_arg_present() -> bool {
    launcher_arg_present(PREVIEW_NO_UPDATE_ARGS)
}

/// 是否以"启动即报错"预览模式启动（用于测试 splash 错误态展示）。
fn preview_crash_arg_present() -> bool {
    launcher_arg_present(PREVIEW_CRASH_ARGS)
}

/// 是否以"启动后最小化到托盘"模式启动。
fn start_minimized_arg_present() -> bool {
    launcher_arg_present(START_MINIMIZED_ARGS)
}

/// 生成更新载荷的临时落地路径：系统临时目录 + 进程 ID + 原可执行文件名，
/// 避免多实例或并发更新互相覆盖。
fn launcher_update_temp_path(current_exe: &Path) -> PathBuf {
    let file_name = current_exe
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("alas-launcher");
    std::env::temp_dir().join(format!(
        "azurpilot-launcher-update-{}-{file_name}",
        std::process::id()
    ))
}

/// 把字节序列格式化为小写十六进制字符串。
fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// 确保更新后的可执行文件具备执行权限。
///
/// 平台差异：Unix 需显式 chmod 0o755；Windows 的可执行性由 .exe 扩展名
/// 决定，无需处理。
///
/// # Errors
/// Unix 上读取或设置权限失败时返回 Err。
fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 非 Windows 平台的自更新落地：直接把新可执行文件重命名覆盖旧文件，
/// 随后带"跳过更新"环境变量拉起新进程。
///
/// Unix 允许覆盖正在运行的程序文件（inode 已映射），因此无需助手进程。
///
/// # Errors
/// 重命名或拉起新进程失败时返回 Err。
#[cfg(not(windows))]
fn replace_launcher_and_restart(current_exe: &Path, update_path: &Path) -> Result<()> {
    fs::rename(update_path, current_exe).with_context(|| {
        t!(
            "errors.replace_launcher_failed",
            error = current_exe.display().to_string()
        )
    })?;
    Command::new(current_exe)
        .env(LAUNCHER_UPDATE_SKIP_ENV, "1")
        .spawn()
        .with_context(|| {
            t!(
                "errors.restart_launcher_failed",
                error = current_exe.display().to_string()
            )
        })?;
    Ok(())
}

/// Windows 平台的自更新落地：运行中的 exe 无法直接覆盖自身，因此先复制
/// 自身为临时助手进程，再由助手等待本进程退出后完成替换与重启。
///
/// # Errors
/// 复制自身或启动助手失败时返回 Err。
#[cfg(windows)]
fn replace_launcher_and_restart(current_exe: &Path, update_path: &Path) -> Result<()> {
    let helper_path = std::env::temp_dir().join(format!(
        "azurpilot-launcher-update-helper-{}.exe",
        std::process::id()
    ));

    fs::copy(current_exe, &helper_path).with_context(|| {
        t!(
            "errors.copy_file_failed",
            src = current_exe.display().to_string(),
            dest = helper_path.display().to_string()
        )
    })?;

    use std::os::windows::process::CommandExt;
    use winapi::um::winbase::CREATE_NO_WINDOW;
    // CREATE_NO_WINDOW：助手是控制台程序的副本，执行时避免闪出黑窗。
    Command::new(&helper_path)
        .arg(LAUNCHER_UPDATE_APPLY_ARG)
        .arg(current_exe)
        .arg(update_path)
        .env(LAUNCHER_UPDATE_SKIP_ENV, "1")
        .env(LAUNCHER_UPDATE_NO_CONSOLE_ENV, "1")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| {
            t!(
                "errors.start_update_script_failed",
                error = helper_path.display().to_string()
            )
        })?;
    Ok(())
}

/// Windows 更新助手模式入口：检查命令行是否携带 --apply-launcher-update。
///
/// 命中时按"目标 exe 路径 + 更新载荷路径"执行替换并重启，返回 Ok(true)
/// 表示本进程以助手身份完成使命，main 应直接返回、不再走正常启动流程。
///
/// # Errors
/// 有标记但缺少路径参数，或替换过程失败时返回 Err。
#[cfg(windows)]
fn try_apply_launcher_update_from_args() -> Result<bool> {
    use std::ffi::OsStr;

    // argv[1] 是模式标记，argv[2]/argv[3] 分别为目标 exe 与更新载荷路径。
    let mut args = std::env::args_os();
    let _ = args.next();
    let Some(mode) = args.next() else {
        return Ok(false);
    };
    if mode != OsStr::new(LAUNCHER_UPDATE_APPLY_ARG) {
        return Ok(false);
    }

    let target_path = args
        .next()
        .ok_or_else(|| anyhow!("missing launcher update target path"))?;
    let update_path = args
        .next()
        .ok_or_else(|| anyhow!("missing launcher update payload path"))?;
    apply_launcher_update_and_restart(PathBuf::from(target_path), PathBuf::from(update_path))?;
    Ok(true)
}

/// 助手核心逻辑：以 1 秒间隔至多重试 60 次替换目标 exe（等待旧进程退出并
/// 释放文件占用），成功后重启启动器并把助手自身的临时副本登记为重启后删除。
///
/// # Errors
/// 60 次重试后仍无法替换时返回最后一次的 Err。
#[cfg(windows)]
fn apply_launcher_update_and_restart(target_path: PathBuf, update_path: PathBuf) -> Result<()> {
    let mut last_error = None;
    // 每次失败休眠 1 秒再试：给旧进程留出退出并释放 exe 句柄的时间。
    for _ in 0..60 {
        match move_file_replace(&update_path, &target_path) {
            Ok(()) => {
                restart_launcher_after_update(&target_path)?;
                schedule_file_delete_on_reboot(&std::env::current_exe()?);
                return Ok(());
            }
            Err(err) => {
                last_error = Some(err);
                thread::sleep(Duration::from_secs(1));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("launcher update replacement timed out")))
}

/// 调用 Win32 MoveFileExW 以"替换已存在 + 允许跨卷 + 直写磁盘"标志移动
/// 文件，实现覆盖式替换。
///
/// # Errors
/// API 返回 0（替换失败）时返回携带 last_os_error 的 Err。
#[cfg(windows)]
fn move_file_replace(from: &Path, to: &Path) -> Result<()> {
    use winapi::um::winbase::{
        MoveFileExW, MOVEFILE_COPY_ALLOWED, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from_wide = path_to_wide(from);
    let to_wide = path_to_wide(to);
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_COPY_ALLOWED | MOVEFILE_WRITE_THROUGH;
    let moved = unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), flags) };
    if moved == 0 {
        return Err(anyhow!(
            "{}: {}",
            t!(
                "errors.replace_launcher_failed",
                error = to.display().to_string()
            ),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// 更新落地后重启启动器：带"跳过更新"与"无控制台"环境变量拉起新 exe。
///
/// # Errors
/// 进程创建失败时返回 Err。
#[cfg(windows)]
fn restart_launcher_after_update(target_path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use winapi::um::winbase::CREATE_NO_WINDOW;

    Command::new(target_path)
        .env(LAUNCHER_UPDATE_SKIP_ENV, "1")
        .env(LAUNCHER_UPDATE_NO_CONSOLE_ENV, "1")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| {
            t!(
                "errors.restart_launcher_failed",
                error = target_path.display().to_string()
            )
        })?;
    Ok(())
}

/// 把指定文件登记为系统重启时删除（MOVEFILE_DELAY_UNTIL_REBOOT）。
/// 此处用于清理更新助手自身（旧启动器的临时副本）；登记失败无伤大雅，
/// 因此忽略返回值。
#[cfg(windows)]
fn schedule_file_delete_on_reboot(path: &Path) {
    use std::ptr;
    use winapi::um::winbase::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};

    let path_wide = path_to_wide(path);
    let _ = unsafe { MoveFileExW(path_wide.as_ptr(), ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
}

/// 把路径编码为以空终止符结尾的 Windows 宽字符（UTF-16），供 Win32 API 使用。
#[cfg(windows)]
fn path_to_wide(path: &Path) -> Vec<u16> {
    use std::{iter, os::windows::ffi::OsStrExt};

    path.as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect()
}

/// main.rs 纯函数逻辑的单元测试：覆盖时间炸弹配置解析、Cargo.toml 键值
/// 读取、splash HTML 生成、日志截断、标题栏注入脚本、版本比较、更新清单
/// 主备回退、Range 探测与并行下载合并、校验晋升等。
#[cfg(test)]
mod tests {
    use super::*;

    /// 时间炸弹段 enabled 时应能解析出配置（且与 enabled 值一致）。
    #[test]
    fn test_time_bomb_config_parses_when_enabled() {
        let section =
            cargo_toml_section("package.metadata.alas-launcher.time-bomb").expect("section exists");
        let enabled = cargo_toml_value(section, "enabled").as_deref() == Some("true");
        let config = time_bomb_config().expect("time bomb config parses");
        assert_eq!(config.is_some(), enabled);
    }

    /// cargo_toml_value 应能读到 expires-at 与中文 message 字段。
    #[test]
    fn test_cargo_toml_value_reads_time_bomb_fields() {
        let section =
            cargo_toml_section("package.metadata.alas-launcher.time-bomb").expect("section exists");
        assert!(cargo_toml_value(section, "expires-at").is_some());
        assert_eq!(
            Some("测试已结束，请安装正式版".to_owned()),
            cargo_toml_value(section, "message")
        );
    }

    /// backend_unready_error 携带日志失败原因时拼接展示，无原因时保持原样。
    #[test]
    #[test]
    fn backend_unready_error_carries_the_reason_from_the_log() {
        let with_reason = backend_unready_error(
            anyhow!("connect failed"),
            Some("React 前端构建失败".to_owned()),
        );
        assert!(format!("{with_reason:#}").contains("React 前端构建失败"));

        let without_reason = backend_unready_error(anyhow!("connect failed"), None);
        assert_eq!(format!("{without_reason:#}"), "connect failed");
    }

    /// 英文环境下 splash HTML 的文案应来自 JSON 字面量（而非 JS 单引号拼接）。
    fn test_english_splash_i18n_uses_json_literals() {
        rust_i18n::set_locale("en");

        let html = splash_redesigned_shell_html("video", "font");

        assert!(html.contains(r#""defaultTip":"Sakura Empire's cherry blossoms"#));
        assert!(!html.contains("const defaultTip = '"));
        assert!(html.contains("window.__ALAS_SPLASH_READY = true;"));
        assert!(html.contains("data:video/mp4;base64,video"));
        assert!(html.contains("font-family: \"MiSans\""));
        assert!(html.contains("data:font/ttf;base64,font"));
        assert!(!html.contains("text-transform: uppercase;"));
    }

    /// splash HTML 应包含可选的 uv 安装进度子条相关标记与样式。
    #[test]
    fn test_splash_includes_optional_uv_progress() {
        let html = splash_redesigned_shell_html("video", "font");

        assert!(html.contains("data:video/mp4;base64,video"));
        assert!(html.contains("id=\"uv-progress-container\""));
        assert!(html.contains("payload.uv_progress"));
        assert!(html.contains("id=\"uv-progress-detail\""));
        assert!(html.contains("grid-template-columns: minmax(0, 1fr) auto"));
        assert!(html.contains("background: rgba(250, 250, 247, 0.78)"));
        assert!(html.contains("content: \"✦\""));
        assert!(!html.contains("'Tips: ' + subtitle.tip"));
        assert!(!html.contains("animation: sweep"));
    }

    /// 截断日志文件应把已存在的旧内容清空。
    #[test]
    fn test_truncate_log_file_replaces_existing_contents() {
        let temp_dir = TempDirBuilder::new()
            .prefix("launcher-log-truncate-test-")
            .tempdir()
            .expect("create temporary log directory");
        let filename = "launcher.txt";
        let path = temp_dir.path().join(filename);
        fs::write(&path, "old launcher log").expect("write old log");

        truncate_log_file(temp_dir.path(), filename).expect("truncate launcher log");

        assert_eq!(fs::read(&path).expect("read truncated log"), b"");
    }

    /// 标题栏应使用 WebView 可拖拽区域（pointerdown + app-region）支持触摸拖动。
    #[test]
    fn test_titlebars_use_webview_draggable_regions_for_touch_dragging() {
        let splash_html = splash_redesigned_shell_html("video", "font");

        assert!(splash_html.contains("touch-action: none;"));
        assert!(splash_html.contains("addEventListener('pointerdown'"));
        assert!(splash_html.contains("-webkit-app-region: drag;"));
        assert!(splash_html.contains("-webkit-app-region: no-drag;"));
        assert!(splash_html.contains("webviewDraggableRegionsEnabled"));
        assert!(splash_html.contains("if (webviewDraggableRegionsEnabled) {"));
        assert!(!splash_html.contains("$NATIVE_TOUCH_DRAG"));

        #[cfg(windows)]
        assert!(splash_html.contains("const webviewDraggableRegionsEnabled = true;"));

        #[cfg(not(target_os = "macos"))]
        let titlebar_script = main_window_titlebar_injection_script();

        #[cfg(not(target_os = "macos"))]
        {
            assert!(titlebar_script.contains("touch-action:none"));
            assert!(titlebar_script.contains("addEventListener('pointerdown'"));
            assert!(titlebar_script.contains("window_start_dragging"));
            assert!(titlebar_script.contains("-webkit-app-region:no-drag"));
            assert!(titlebar_script.contains(
                ".alas-titlebar-drag-zone{position:absolute;inset:0 148px 0 0;height:100%;pointer-events:none"
            ));
            assert!(titlebar_script.contains("getComputedStyle(element).cursor === 'pointer'"));
            assert!(titlebar_script.contains("min-height:28px"));
            assert!(titlebar_script.contains("background:rgba(250,250,247,.78)"));
            assert!(titlebar_script.contains(".icon-close{color:#e64f58}"));
            assert!(titlebar_script.contains("--alas-titlebar-height:56px"));
            assert!(titlebar_script.contains("transform:translateY(-6px) scale(.96)"));
            assert!(!titlebar_script.contains("scale(.72)"));
            assert!(titlebar_script.contains("alas-close-menu"));
            assert!(!titlebar_script.contains("alas-close-optics"));
            assert!(!titlebar_script.contains("alas-island-open"));
            assert!(titlebar_script.contains("__ALAS_OPEN_CLOSE_PROMPT"));
            assert!(titlebar_script.contains("window_exit_application"));
            assert!(!titlebar_script.contains("addEventListener('scroll'"));
            assert!(!titlebar_script.contains("MutationObserver"));
            assert!(titlebar_script.contains("data-theme*="));
            assert!(titlebar_script.contains("#alas-launcher-titlebar.is-dark"));
        }
    }

    /// tauri.conf.json 中 Windows 窗口应启用 WebView2 拖拽区域等浏览器参数。
    #[test]
    fn test_windows_enable_webview_draggable_regions() {
        let config: serde_json::Value =
            serde_json::from_str(TAURI_CONFIG_SOURCE).expect("valid config");
        let windows = config["app"]["windows"].as_array().expect("window configs");

        for window in windows {
            let args = window["additionalBrowserArgs"]
                .as_str()
                .expect("draggable regions arguments");
            assert!(args.contains("msWebView2EnableDraggableRegions"));
            assert!(args.contains("ElasticOverscroll"));
            assert!(args.contains("msWebOOUI,msPdfOOUI,msSmartScreenProtection"));
            assert!(args.contains("--no-proxy-server"));
        }
    }

    /// 版本比较：合法版本可比较，非法版本返回 None。
    #[test]
    fn test_launcher_update_versions_must_be_valid() {
        assert_eq!(launcher_version_is_newer("2.1.6", "2.1.7"), Some(true));
        assert_eq!(
            launcher_version_is_newer("2.1.6", "2.1.6+build.1"),
            Some(false)
        );
        assert_eq!(launcher_version_is_newer("2.1.6", "not-a-version"), None);
        assert_eq!(launcher_version_is_newer("2.1", "2.1.7"), None);
        assert_eq!(launcher_version_is_newer("2.1.6", "2.1.7-"), None);
    }

    /// 主清单 URL 失败（503）时应回退到备用 URL 并成功解析。
    #[test]
    fn test_launcher_update_manifest_uses_fallback_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = std::thread::spawn(move || {
            let fallback_body = r#"{"version":"2.1.8","platforms":{}}"#;
            let responses = [
                (
                    "/primary",
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned(),
                ),
                (
                    "/fallback",
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{fallback_body}",
                        fallback_body.len()
                    ),
                ),
            ];

            for (expected_path, response) in responses {
                let (mut stream, _) = listener.accept().expect("accept manifest request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set read timeout");
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0u8; 1024];
                    let read = stream.read(&mut buffer).expect("read manifest request");
                    assert!(read > 0, "manifest request ended before headers");
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                assert!(request.starts_with(&format!("GET {expected_path} ")));
                stream
                    .write_all(response.as_bytes())
                    .expect("write manifest response");
            }
        });

        let client = Client::builder().no_proxy().build().expect("build client");
        let primary = format!("http://{address}/primary");
        let fallback = format!("http://{address}/fallback");
        let manifest = fetch_launcher_update_manifest_from_urls(&client, &[&primary, &fallback])
            .expect("fetch manifest from fallback");

        assert_eq!(manifest.version, "2.1.8");
        server.join().expect("manifest server completed");
    }

    /// 载荷校验：仅接受 HTTPS URL 与合法 SHA-256 摘要。
    #[test]
    fn test_launcher_update_payload_requires_https_and_sha256() {
        let digest = "a".repeat(64);

        assert!(
            validate_launcher_update_payload("https://updates.example/launcher", &digest).is_ok()
        );
        assert!(
            validate_launcher_update_payload("http://updates.example/launcher", &digest).is_err()
        );
        assert!(validate_launcher_update_payload(
            "https://updates.example/launcher",
            "not-a-digest"
        )
        .is_err());
    }

    /// 字节区间切分：区间数量、覆盖范围与无重叠应满足约定。
    #[test]
    fn test_launcher_update_byte_ranges_cover_payload_once() {
        let total_bytes = LAUNCHER_UPDATE_MIN_CHUNK_BYTES * 8 + 17;
        let ranges = launcher_update_byte_ranges(total_bytes);

        assert_eq!(ranges.len(), LAUNCHER_UPDATE_MAX_CONNECTIONS);
        assert_eq!(ranges.first().map(|range| range.start), Some(0));
        assert_eq!(ranges.last().map(|range| range.end), Some(total_bytes - 1));
        assert_eq!(
            ranges
                .iter()
                .map(|range| range.end - range.start + 1)
                .sum::<u64>(),
            total_bytes
        );
        assert!(ranges
            .windows(2)
            .all(|pair| pair[0].end + 1 == pair[1].start));
    }

    /// Content-Range 解析：合法值与非法值（通配、倒序、无前缀）的处理。
    #[test]
    fn test_launcher_update_content_range_parser() {
        assert_eq!(
            parse_launcher_update_content_range("bytes 10-19/42"),
            Some((10, 19, 42))
        );
        assert_eq!(parse_launcher_update_content_range("bytes 0-0/*"), None);
        assert_eq!(parse_launcher_update_content_range("bytes 19-10/42"), None);
        assert_eq!(parse_launcher_update_content_range("not-a-range"), None);
    }

    /// 服务器忽略 Range 直接返回 200 时，探测应返回 None 以走顺序下载。
    #[test]
    fn test_launcher_update_range_probe_falls_back_when_ignored() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept range probe");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            let mut request = Vec::new();
            loop {
                let mut buffer = [0u8; 1024];
                let read = stream.read(&mut buffer).expect("read range probe");
                assert!(read > 0, "range probe ended before headers");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(String::from_utf8_lossy(&request)
                .lines()
                .any(|line| line.eq_ignore_ascii_case("range: bytes=0-0")));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("write range probe response");
        });

        let client = Client::builder().no_proxy().build().expect("build client");
        let url = format!("http://{address}/launcher");
        assert_eq!(
            launcher_update_range_total(&client, &url).expect("probe range support"),
            None
        );
        server.join().expect("range probe server completed");
    }

    /// 并行下载应按 Range 取回各分片，并合并出与原载荷一致的文件。
    #[test]
    fn test_parallel_launcher_update_download_merges_ranges() {
        let payload: Vec<u8> = (0..(LAUNCHER_UPDATE_MIN_CHUNK_BYTES * 2 + 17))
            .map(|index| (index % 251) as u8)
            .collect();
        let payload = Arc::new(payload);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let request_count = launcher_update_byte_ranges(payload.len() as u64).len() + 1;
        let server_payload = Arc::clone(&payload);
        let server = std::thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().expect("accept range request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set read timeout");
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0u8; 1024];
                    let read = stream.read(&mut buffer).expect("read range request");
                    assert!(read > 0, "range request ended before headers");
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }

                let request = String::from_utf8_lossy(&request);
                let range = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("range") {
                            value.trim().strip_prefix("bytes=")
                        } else {
                            None
                        }
                    })
                    .expect("range request header");
                let (start, end) = range.split_once('-').expect("range bounds");
                let start: usize = start.parse().expect("range start");
                let end: usize = end.parse().expect("range end");
                assert!(start <= end && end < server_payload.len());
                let body = &server_payload[start..=end];
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    body.len(),
                    start,
                    end,
                    server_payload.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write headers");
                stream.write_all(body).expect("write range body");
            }
        });

        let client = Client::builder().no_proxy().build().expect("build client");
        let url = format!("http://{address}/launcher");
        let temp_dir = TempDirBuilder::new()
            .prefix("launcher-range-test-")
            .tempdir()
            .unwrap();
        let part_path = temp_dir.path().join("launcher.part");
        let total_bytes = launcher_update_range_total(&client, &url)
            .expect("probe range support")
            .expect("server supports ranges");
        let ranges = launcher_update_byte_ranges(total_bytes);
        let mut progress = Vec::new();

        let downloaded = download_launcher_update_parallel(
            &client,
            &url,
            total_bytes,
            &ranges,
            &part_path,
            &mut |update| progress.push(update.progress),
        )
        .expect("parallel launcher download");

        assert_eq!(downloaded, payload.len() as u64);
        let merged = fs::read(&part_path).expect("read merged file");
        assert_eq!(merged.as_slice(), payload.as_slice());
        assert!(progress
            .iter()
            .any(|value| *value > LAUNCHER_UPDATE_DOWNLOAD_PROGRESS_START));
        server.join().expect("range server completed");
    }

    /// 校验晋升：摘要匹配才允许重命名落地，不匹配时清理半成品。
    #[test]
    fn test_launcher_update_promotion_requires_valid_digest() {
        let temp_dir = TempDirBuilder::new()
            .prefix("launcher-promotion-test-")
            .tempdir()
            .unwrap();
        let part_path = temp_dir.path().join("launcher.part");
        let update_path = temp_dir.path().join("launcher.update");
        fs::write(&part_path, b"verified update").expect("write update part");
        let digest = sha256_file(&part_path).expect("hash update part");

        assert_eq!(
            verify_and_promote_launcher_update(&part_path, &update_path, &digest)
                .expect("promote verified update"),
            b"verified update".len() as u64
        );
        assert!(!part_path.exists());
        assert_eq!(
            fs::read(&update_path).expect("read promoted update"),
            b"verified update"
        );

        let failed_part_path = temp_dir.path().join("failed.part");
        let failed_update_path = temp_dir.path().join("failed.update");
        fs::write(&failed_part_path, b"unverified update").expect("write failed update part");
        assert!(verify_and_promote_launcher_update(
            &failed_part_path,
            &failed_update_path,
            &"0".repeat(64),
        )
        .is_err());
        assert!(!failed_part_path.exists());
        assert!(!failed_update_path.exists());
    }
}

/// 切换 macOS 激活策略：Regular 在 Dock 显示应用图标，Accessory 隐藏
/// Dock 图标（托盘化状态）。
///
/// 平台差异：激活策略是 macOS 特有概念；无可见常规窗口时切换为 Accessory
/// 可避免应用因"没有可见窗口"而表现异常。
#[cfg(target_os = "macos")]
fn set_macos_activation_policy(app: &tauri::AppHandle, regular: bool) {
    let policy = if regular {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    };
    if let Err(e) = app.set_activation_policy(policy) {
        error!("Failed to set activation policy: {}", e);
    }
}

/// Windows 专用：检查 Node.js 可用性，缺失或版本过低时弹窗引导安装。
///
/// 仅 Windows 需要：其发布包可能在运行时现场构建前端产物，依赖 Node.js；
/// macOS/Linux 发布包通常已自带构建产物。
///
/// # Returns
/// 是否可继续启动流程：Node 就绪、用户拒绝安装、安装失败（已提示）均返回
/// true；用户取消或安装被中断返回 false。
#[cfg(windows)]
fn prompt_for_missing_nodejs(
    app_handle: &tauri::AppHandle,
    splash: &WebviewWindow,
    mut status_updater: &mut impl FnMut(SplashUpdate),
    cancel_requested: &AtomicBool,
    start_minimized: bool,
) -> bool {
    let availability = crate::nodejs::is_nodejs_available();
    if matches!(availability, crate::nodejs::NodeJsAvailability::Ready) {
        return true;
    }
    if cancel_requested.load(Ordering::SeqCst) {
        return false;
    }

    // 两种情形文案不同：完全没装，与装了但版本低于构建要求。
    let (title, message, log) = match &availability {
        crate::nodejs::NodeJsAvailability::Outdated(found) => (
            t!("dialog.nodejs_outdated_title"),
            t!(
                "dialog.nodejs_outdated_message",
                found = found,
                minimum = crate::nodejs::NODEJS_MIN_FRONTEND_VERSION_TEXT
            ),
            "Node.js version is below the version required by the frontend build",
        ),
        _ => (
            t!("dialog.nodejs_missing_title"),
            t!("dialog.nodejs_missing_message"),
            "Node.js was not found on this Windows system",
        ),
    };
    warn!("{log}");
    // 最小化启动时先把 splash 唤起，保证弹窗有父窗口可依附。
    if start_minimized {
        let _ = reveal_window(splash);
    }
    status_updater(SplashUpdate::loading(
        t!("setup.checking_nodejs"),
        t!("setup.checking_nodejs"),
        5,
    ));

    // 阻塞式弹窗：用户选择"立即安装"才进入安装流程，"暂不"则继续启动。
    let install_requested = app_handle
        .dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            t!("dialog.nodejs_install").to_string(),
            t!("dialog.nodejs_not_now").to_string(),
        ))
        .parent(splash)
        .blocking_show();
    if !install_requested {
        info!("Node.js installation was declined by the user");
        return true;
    }

    match crate::nodejs::install_nodejs(&availability, cancel_requested, &mut status_updater) {
        Ok(()) => {
            info!("Node.js installation completed successfully");
            true
        }
        Err(_) if cancel_requested.load(Ordering::SeqCst) => false,
        Err(error) => {
            error!("Node.js installation failed: {error:#}");
            // 安装失败：splash 进入错误态并弹窗提示，但流程仍继续——
            // 后续步骤失败时会给出更具体的报错。
            status_updater(SplashUpdate::error(
                t!("dialog.nodejs_install_failed"),
                t!(
                    "dialog.nodejs_install_failed_detail",
                    error = format!("{error:#}")
                ),
                7,
            ));
            app_handle
                .dialog()
                .message(t!(
                    "dialog.nodejs_install_failed_detail",
                    error = format!("{error:#}")
                ))
                .title(t!("dialog.nodejs_install_failed"))
                .kind(MessageDialogKind::Error)
                .parent(splash)
                .blocking_show();
            true
        }
    }
}

/// 应用入口：装配 Tauri Builder（自定义协议、命令、插件、托盘、单实例），
/// 注册 Ready/ExitRequested/WindowEvent 等运行事件处理，并在后台线程驱动
/// "自更新检查 → 仓库准备 → 后端启动 → 主窗口展示"的完整启动流程。
///
/// # Errors
/// 环境准备（setup_environment）或日志初始化失败时返回 Err，进程随即结束。
fn main() -> Result<()> {
    // Windows 更新助手模式：命中 --apply-launcher-update 参数时完成替换并
    // 重启目标，随后直接退出，不走正常启动流程。
    #[cfg(windows)]
    if try_apply_launcher_update_from_args()? {
        return Ok(());
    }

    // 更新助手拉起的进程不附着控制台；普通启动则尝试附着父进程控制台，
    // 使命令行运行时能看到 stderr 输出（结果记录进 HAS_CONSOLE）。
    #[cfg(windows)]
    unsafe {
        use crate::window_util::HAS_CONSOLE;
        use std::sync::atomic::Ordering;
        use winapi::um::wincon::{AttachConsole, ATTACH_PARENT_PROCESS};
        if std::env::var_os(LAUNCHER_UPDATE_NO_CONSOLE_ENV).is_some() {
            std::env::remove_var(LAUNCHER_UPDATE_NO_CONSOLE_ENV);
        } else {
            HAS_CONSOLE.store(AttachConsole(ATTACH_PARENT_PROCESS) != 0, Ordering::Relaxed);
        }
    }
    // 初始化运行环境（切换工作目录等，详见 setup 模块）。
    setup_environment()?;
    // 日志守卫必须存活到进程结束，退出时负责刷盘。
    let _log_guard = initialize_logging()?;
    crate::i18n::init();
    // 解析预览与最小化启动参数（测试/演示用）。
    let preview_crash = preview_crash_arg_present();
    let preview_no_update = preview_crash || preview_no_update_arg_present();
    let start_minimized = start_minimized_arg_present();

    info!("=== AzurPilot starting ===");
    info!("Launcher log file: log/{}", today_launcher_log_filename());
    if preview_no_update {
        info!("Preview no-update mode enabled; skipping launcher update check");
    }
    if preview_crash {
        info!("Preview crash mode enabled; splash will stop on an artificial error state");
    }
    if start_minimized {
        info!("Start minimized mode enabled; main window will stay in tray after backend is ready");
    }

    // 读取 deploy.yaml 得到 WebUI 启动配置；缺失时使用默认配置（端口 22267）。
    let deploy_config = get_deploy_config();
    let webui_config = WebuiLaunchConfig::from_deploy_config(deploy_config.as_ref());
    if deploy_config.is_none() {
        warn!("config/deploy.yaml not found or invalid, using default WebUI launch config");
    }
    let port = webui_config.port;

    // 各线程共享的运行状态：后端句柄与一组生命周期标志
    // （退出放行、启动阻塞、setup 取消/运行/完成、清理触发、主窗口重建中）。
    let backend = Arc::new(Mutex::new(None));
    let allow_exit = Arc::new(AtomicBool::new(false));
    let launch_blocked = Arc::new(AtomicBool::new(false));
    let setup_cancel_requested = Arc::new(AtomicBool::new(false));
    let setup_running = Arc::new(AtomicBool::new(false));
    let setup_completed = Arc::new(AtomicBool::new(false));
    let startup_cleanup_started = Arc::new(AtomicBool::new(false));
    let recreating_main_window = Arc::new(AtomicBool::new(false));

    let allow_exit_for_setup = allow_exit.clone();
    let launch_blocked_for_setup = launch_blocked.clone();
    let recreating_main_window_for_single_instance = recreating_main_window.clone();
    let recreating_main_window_for_setup = recreating_main_window.clone();
    let recreating_main_window_for_run = recreating_main_window.clone();
    let launch_blocked_for_run = launch_blocked.clone();
    let start_minimized_for_run = start_minimized;

    info!("Starting Webview...");
    // 注册 alas-error:// 自定义协议：服务后端连接失败的错误页面。
    tauri::Builder::default()
        .register_uri_scheme_protocol("alas-error", |_ctx, request| {
            backend_error_response(request)
        })
        // 注册 alas-splash:// 自定义协议：服务 splash 启动画面页面。
        .register_uri_scheme_protocol("alas-splash", |_ctx, _request| splash_response())
        // 注册前端可调用的 Tauri 命令：文件保存、日志下载、窗口控制等。
        .manage(ExitControl(allow_exit.clone()))
        .invoke_handler(tauri::generate_handler![
            save_as,
            download_today_gui_log,
            download_today_launcher_log,
            retry_backend_connection,
            window_hide,
            window_minimize,
            window_toggle_maximize,
            window_close,
            window_exit_application,
            window_start_dragging,
            window_is_maximized
        ])
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // 单实例插件：二次启动时唤起已有实例的主窗口，而不是再开一个进程。
        .plugin(tauri_plugin_single_instance::init(
            move |app, _argv, _cwd| {
                restore_main_window_from_tray(
                    app,
                    port,
                    recreating_main_window_for_single_instance.clone(),
                );
            },
        ))
        // setup 回调：时间炸弹校验、主窗口创建与系统托盘装配。
        .setup(move |app| {
            // 时间炸弹校验：已过期则弹窗提示并以退出码 0 结束，阻止继续启动。
            match time_bomb_expiration_message() {
                Ok(Some(message)) => {
                    launch_blocked_for_setup.store(true, Ordering::SeqCst);
                    allow_exit_for_setup.store(true, Ordering::SeqCst);
                    let app_handle = app.handle().clone();
                    app.dialog()
                        .message(message)
                        .title(t!("dialog.test_ended"))
                        .show(move |_| {
                            app_handle.exit(0);
                        });
                    return Ok(());
                }
                Ok(None) => {}
                Err(err) => {
                    warn!("Unable to verify test expiration time: {:?}", err);
                }
            }

            // 提前创建主窗口（隐藏态），等后端就绪后再导航并显示。
            create_main_window(&app.handle(), port)?;

            // Windows 与 macOS：创建系统托盘（Linux 桌面托盘兼容性参差，暂不提供）。
            #[cfg(any(windows, target_os = "macos"))]
            {
                info!("Creating system tray...");
                let allow_exit = allow_exit_for_setup.clone();
                let recreating_main_window_for_menu = recreating_main_window_for_setup.clone();
                #[cfg(windows)]
                let recreating_main_window_for_tray = recreating_main_window_for_setup.clone();
                // 托盘菜单：显示/隐藏主窗口与退出两项。
                let show_item = MenuItemBuilder::new(t!("tray.toggle_visibility"))
                    .id("toggle_visibility")
                    .build(app)?;
                let quit_item = MenuItemBuilder::new(t!("tray.quit"))
                    .id("quit")
                    .build(app)?;
                let tray_menu = MenuBuilder::new(app)
                    .item(&show_item)
                    .separator()
                    .item(&quit_item)
                    .build()?;

                info!("Tray menu created successfully");

                // 使用内嵌图标字节，保证打包后的应用也能正确加载托盘图标。
                let icon = tray_icon_for_platform();

                info!("Building tray icon...");
                let mut tray_builder = TrayIconBuilder::with_id("main-tray")
                    .icon(icon)
                    .tooltip("AzurPilot")
                    .menu(&tray_menu);

                // Windows：右键点击托盘图标才弹出菜单。
                #[cfg(windows)]
                {
                    tray_builder = tray_builder.show_menu_on_left_click(false);
                }

                // macOS：左键点击即弹出菜单（菜单栏图标没有其他左键行为）。
                #[cfg(target_os = "macos")]
                {
                    info!("Setting macOS tray to show menu on left click");
                    tray_builder = tray_builder.show_menu_on_left_click(true);
                }

                match tray_builder
                    .on_menu_event(move |app, event| {
                        debug!("Tray menu event: {:?}", event.id());
                        match event.id().as_ref() {
                            "toggle_visibility" => {
                                toggle_main_window_visibility(
                                    app,
                                    port,
                                    recreating_main_window_for_menu.clone(),
                                );
                            }
                            "quit" => {
                                allow_exit.store(true, Ordering::SeqCst);
                                app.exit(0);
                            }
                            _ => {}
                        }
                    })
                    .on_tray_icon_event(move |tray, event| {
                        // Windows：左键单击切换主窗口可见性；macOS 左键已用于
                        // 弹出菜单，此处忽略。
                        #[cfg(windows)]
                        if let tauri::tray::TrayIconEvent::Click {
                            button: tauri::tray::MouseButton::Left,
                            button_state: tauri::tray::MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            toggle_main_window_visibility(
                                &app,
                                port,
                                recreating_main_window_for_tray.clone(),
                            );
                        }

                        #[cfg(target_os = "macos")]
                        {
                            let _ = tray;
                            let _ = event;
                        }
                    })
                    .build(app)
                {
                    Ok(_) => {
                        info!("System tray created successfully!");
                    }
                    Err(e) => {
                        error!("Failed to create system tray: {:?}", e);
                        return Err(Box::new(e));
                    }
                }
            }

            Ok(())
        })
        // 运行事件循环；Ready 事件触发后在后台线程执行完整启动流程。
        .build(tauri::generate_context!())?
        .run(move |app_handle, event| {
            match event {
                tauri::RunEvent::Ready => {
                    // 时间炸弹已过期：什么都不做，等待弹窗回调退出进程。
                    if launch_blocked_for_run.load(Ordering::SeqCst) {
                        debug!("Launch blocked by test expiration");
                        return;
                    }

                    debug!("RunEvent::Ready");
                    // Ctrl-C：置放行标志后直接退出，保证命令行下也能干净终止。
                    let allow_exit = allow_exit.clone();
                    let allow_exit_for_ctrlc = allow_exit.clone();
                    let handle1 = app_handle.clone();
                    ctrlc::set_handler(move || {
                        allow_exit_for_ctrlc.store(true, Ordering::SeqCst);
                        handle1.exit(0);
                    })
                    .expect("Error setting Ctrl-C handler");
                    let app_handle = app_handle.clone();
                    let backend = backend.clone();
                    let webui_config = webui_config.clone();
                    let setup_cancel_requested = setup_cancel_requested.clone();
                    let setup_running = setup_running.clone();
                    let setup_completed = setup_completed.clone();
                    let recreating_main_window_for_notify = recreating_main_window_for_run.clone();
                    let start_minimized = start_minimized_for_run;
                    // 完整启动流程放在后台线程执行，避免阻塞 Tauri 事件循环。
                    thread::spawn(move || {
                        setup_running.store(true, Ordering::SeqCst);
                        // splash 由 tauri.conf.json 静态声明，此处必然能取到。
                        let splash = app_handle.get_webview_window("splash").unwrap();
                        initialize_splash(&splash, !start_minimized);
                        // 进度只升不降：记录历史最高值，防止不同阶段进度回跳。
                        let last_progress = Cell::new(0u8);
                        let mut status_updater = |mut update: SplashUpdate| {
                            update.progress = update.progress.max(last_progress.get());
                            last_progress.set(update.progress);
                            update_splash(&splash, &update);
                        };

                        status_updater(
                            SplashUpdate::loading(
                                t!("splash.starting"),
                                t!("splash.webui_init"),
                                4,
                            )
                            .with_subtitle(format!(
                                "{} | Tips:{}",
                                t!("splash.initializing"),
                                crate::setup::get_tip()
                            )),
                        );

                        // 启动器自更新检查：成功替换重启时直接退出本进程交棒新版本。
                        if !preview_no_update {
                            // 自更新的进度单独记一条"历史最高"，与主流程互不回退。
                            let launcher_progress = Cell::new(0u8);
                            let mut launcher_status_updater = |mut update: SplashUpdate| {
                                update.progress = update.progress.max(launcher_progress.get());
                                launcher_progress.set(update.progress);
                                update_splash(&splash, &update);
                            };

                            match check_launcher_update_and_restart(&mut launcher_status_updater) {
                                Ok(true) => {
                                    info!("Launcher update installed, restarting");
                                    setup_completed.store(true, Ordering::SeqCst);
                                    setup_running.store(false, Ordering::SeqCst);
                                    allow_exit.store(true, Ordering::SeqCst);
                                    app_handle.exit(0);
                                    return;
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    warn!("Required launcher update failed: {e:#}");
                                    if start_minimized {
                                        let _ = reveal_window(&splash);
                                    }
                                    launcher_status_updater(SplashUpdate::error(
                                        t!("launcher_update.failed"),
                                        t!(
                                            "launcher_update.failed_detail",
                                            error = format!("{e:#}")
                                        ),
                                        launcher_progress.get().max(8),
                                    ));
                                    setup_completed.store(true, Ordering::SeqCst);
                                    setup_running.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }

                        // 预览崩溃模式：直接在 splash 呈现错误态并停止启动流程。
                        if preview_crash {
                            if start_minimized {
                                let _ = reveal_window(&splash);
                            }
                            status_updater(
                                SplashUpdate::error(
                                    t!("dialog.startup_failed"),
                                    t!("splash.preview_crash_detail"),
                                    42,
                                )
                                .with_subtitle(format!(
                                    "{} | Tips：{}",
                                    t!("splash.preview_crash_mode"),
                                    crate::setup::get_tip()
                                )),
                            );
                            setup_completed.store(true, Ordering::SeqCst);
                            setup_running.store(false, Ordering::SeqCst);
                            return;
                        }

                        // Windows：检查 Node.js，缺失或版本过低时引导安装
                        // （前端可能需在运行时现场构建）。
                        #[cfg(windows)]
                        if !prompt_for_missing_nodejs(
                            &app_handle,
                            &splash,
                            &mut status_updater,
                            &setup_cancel_requested,
                            start_minimized,
                        ) {
                            setup_running.store(false, Ordering::SeqCst);
                            return;
                        }
                        if setup_cancel_requested.load(Ordering::SeqCst) {
                            setup_running.store(false, Ordering::SeqCst);
                            return;
                        }

                        // 准备 ALAS 仓库与 Python 运行环境（克隆/更新/依赖同步等耗时步骤）。
                        if let Err(e) = setup_alas_repo(
                            &mut status_updater,
                            setup_cancel_requested.clone(),
                            preview_no_update,
                        ) {
                            error!("{e}");
                            setup_running.store(false, Ordering::SeqCst);
                            if setup_cancel_requested.load(Ordering::SeqCst) {
                                return;
                            }
                            if start_minimized {
                                let _ = reveal_window(&splash);
                            }
                            status_updater(SplashUpdate::error(
                                t!("dialog.startup_failed"),
                                t!("dialog.repo_setup_failed", error = e.to_string()),
                                last_progress.get().max(8),
                            ));
                            return;
                        }
                        info!("Starting gui.py on http://127.0.0.1:{}/", port);
                        status_updater(
                            SplashUpdate::loading(
                                t!("splash.starting"),
                                t!("splash.webui_init_slow"),
                                97,
                            )
                            .with_subtitle(format!(
                                "{} | Tips:{}",
                                t!("splash.starting_backend"),
                                crate::setup::get_tip()
                            )),
                        );
                        // 后端启动循环：启动超时且未恢复过时，重建 venv 并重试一次。
                        let mut backend_recovery_used = false;
                        let backend_result = loop {
                            match ManagedBackend::new(&webui_config) {
                                Ok(backend) => break Ok(backend),
                                Err(error)
                                    if !backend_recovery_used
                                        && is_backend_startup_timeout(&error) =>
                                {
                                    backend_recovery_used = true;
                                    if setup_cancel_requested.load(Ordering::SeqCst) {
                                        break Err(error);
                                    }

                                    warn!(
                                        "Backend startup timed out; rebuilding .venv and retrying once"
                                    );
                                    if let Err(recovery_error) = rebuild_venv_and_sync_dependencies(
                                        &mut status_updater,
                                        setup_cancel_requested.clone(),
                                    ) {
                                        break Err(recovery_error.context(
                                            "Failed to rebuild .venv after backend startup timeout",
                                        ));
                                    }

                                    info!(
                                        "Retrying gui.py after rebuilding dependencies on http://127.0.0.1:{port}/"
                                    );
                                    status_updater(
                                        SplashUpdate::loading(
                                            t!("splash.starting"),
                                            t!("splash.webui_init_slow"),
                                            97,
                                        )
                                        .with_subtitle(format!(
                                            "{} | Tips:{}",
                                            t!("splash.starting_backend"),
                                            crate::setup::get_tip()
                                        )),
                                    );
                                }
                                Err(error) => break Err(error),
                            }
                        };
                        let b = match backend_result {
                            Ok(backend) => backend,
                            Err(e) => {
                                error!("{e}");
                                setup_running.store(false, Ordering::SeqCst);
                                if setup_cancel_requested.load(Ordering::SeqCst) {
                                    return;
                                }
                                if start_minimized {
                                    let _ = reveal_window(&splash);
                                }
                                status_updater(SplashUpdate::error(
                                    t!("dialog.startup_failed"),
                                    t!("dialog.backend_launch_failed", error = e.to_string()),
                                    last_progress.get().max(97),
                                ));
                                return;
                            }
                        };
                        // 后端句柄存入共享状态，退出时统一 terminate。
                        *backend.lock().unwrap() = Some(b);
                        // 通知点击回调：唤起主窗口（可能在任意线程触发，需调度）。
                        let notification_click: NotificationClickHandler = {
                            let app_handle = app_handle.clone();
                            let recreating_main_window = recreating_main_window_for_notify.clone();
                            Arc::new(move || {
                                restore_main_window_from_any_thread(
                                    app_handle.clone(),
                                    port,
                                    recreating_main_window.clone(),
                                );
                            })
                        };
                        // 启动 SSE 通知流与反向控制流：接收后端推送的通知/弹窗
                        // 与退出指令。
                        start_notify_stream(
                            app_handle.clone(),
                            port,
                            allow_exit.clone(),
                            notification_click,
                        );
                        start_launcher_control_stream(port, allow_exit.clone());
                        status_updater(
                            SplashUpdate::loading(t!("splash.opening"), t!("splash.ready"), 100)
                                .with_subtitle(format!(
                                    "{} | Tips:{}",
                                    t!("splash.startup_complete"),
                                    crate::setup::get_tip()
                                )),
                        );
                        // 一切就绪：销毁 splash，主窗口导航到 WebUI 后按需显示。
                        let _ = splash.destroy();
                        debug!("Destroyed splash window after startup");

                        info!("Webview is ready");
                        let window = app_handle.get_webview_window("main").unwrap();
                        window.set_resizable(true).unwrap();
                        if let Err(e) = navigate_backend_or_error(&window, port) {
                            error!("Failed to navigate main window: {:?}", e);
                        }
                        // 最小化启动：后端就绪后仍保持隐藏，仅驻留托盘。
                        if start_minimized {
                            info!("Backend is ready; keeping main window hidden in tray");
                            let _ = window.hide();
                        } else {
                            reveal_window(&window).unwrap();
                        }
                        setup_completed.store(true, Ordering::SeqCst);
                        setup_running.store(false, Ordering::SeqCst);
                    });
                }
                tauri::RunEvent::ExitRequested { api, .. } => {
                    // 启动尚未完成时的退出请求（如 splash 阶段被关闭）：
                    // 先执行启动清理，清理完成后再自行退出。
                    if !setup_completed.load(Ordering::SeqCst)
                        && !startup_cleanup_started.load(Ordering::SeqCst)
                    {
                        api.prevent_exit();
                        begin_startup_cleanup(
                            app_handle.clone(),
                            allow_exit.clone(),
                            setup_cancel_requested.clone(),
                            setup_running.clone(),
                            startup_cleanup_started.clone(),
                        );
                        return;
                    }

                    let should_allow = allow_exit.load(Ordering::SeqCst);
                    debug!("ExitRequested event: allow_exit={}", should_allow);

                    // 仅在显式放行（托盘退出、window_exit_application、Ctrl-C
                    // 等）时才允许退出；否则阻止退出并把主窗口最小化到托盘。
                    if !should_allow {
                        api.prevent_exit();
                        debug!("Minimizing main window to tray");
                        minimize_main_window_to_tray(&app_handle);
                        return;
                    }

                    debug!("allow_exit is TRUE, proceeding with app shutdown");
                    // 放行退出：先终止 gui.py 后端进程再结束应用。
                    info!("App exit allowed, shutting down backend...");
                    if let Some(ref mut b) = *backend.lock().unwrap() {
                        if let Err(e) = b.terminate() {
                            warn!("Failed to terminate backend process: {:?}", e);
                        }
                    }
                }
                // macOS Dock 图标点击重开（Reopen）：恢复主窗口并切回 Regular 策略。
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Reopen { .. } => {
                    restore_main_window_from_any_thread(
                        app_handle.clone(),
                        port,
                        recreating_main_window_for_run.clone(),
                    );
                }
                tauri::RunEvent::WindowEvent {
                    label,
                    event: tauri::WindowEvent::CloseRequested { ref api, .. },
                    ..
                } => {
                    debug!("Window {} close requested", label);

                    // splash 阶段的关闭请求：启动未完成时转启动清理流程。
                    if label == "splash" && !setup_completed.load(Ordering::SeqCst) {
                        api.prevent_close();
                        begin_startup_cleanup(
                            app_handle.clone(),
                            allow_exit.clone(),
                            setup_cancel_requested.clone(),
                            setup_running.clone(),
                            startup_cleanup_started.clone(),
                        );
                        return;
                    }

                    // 其余情况下关闭 splash 视为放弃启动：置放行标志并退出。
                    if label == "splash" && !allow_exit.load(Ordering::SeqCst) {
                        api.prevent_close();
                        allow_exit.store(true, Ordering::SeqCst);
                        app_handle.exit(0);
                        return;
                    }

                    // Windows：不弹原生对话框，而是让主窗口打开自带的
                    // "退出/最小化到托盘"选择菜单；JS 注入缺失时降级为托盘化。
                    #[cfg(windows)]
                    {
                        if label == "main" && !allow_exit.load(Ordering::SeqCst) {
                            api.prevent_close();
                            if let Some(main_window) = app_handle.get_webview_window("main") {
                                if let Err(err) = main_window.eval(
                                    "if (typeof window.__ALAS_OPEN_CLOSE_PROMPT !== 'function') { throw new Error('close prompt is unavailable'); } window.__ALAS_OPEN_CLOSE_PROMPT();",
                                ) {
                                    warn!("Unable to open close chooser: {err:?}");
                                    minimize_main_window_to_tray(&app_handle);
                                }
                            } else {
                                minimize_main_window_to_tray(&app_handle);
                            }
                            return;
                        }
                    }

                    // macOS：隐藏窗口并切到 Accessory 策略，避免没有可见常规
                    // 窗口时应用被判定为应退出。
                    #[cfg(target_os = "macos")]
                    {
                        if label == "main" && !allow_exit.load(Ordering::SeqCst) {
                            api.prevent_close();
                            minimize_main_window_to_tray(&app_handle);
                            return;
                        }
                    }

                    // Linux：直接隐藏窗口（不弹确认）。
                    #[cfg(target_os = "linux")]
                    {
                        if label == "main" && !allow_exit.load(Ordering::SeqCst) {
                            api.prevent_close();
                            minimize_main_window_to_tray(&app_handle);
                            return;
                        }
                    }
                }

                _ => {}
            };
        });
    Ok(())
}

/// 初始化 tracing 日志：当日文件（log/{日期}_launcher.txt）+ stderr 双输出。
///
/// 文件层不带 ANSI 颜色与 target 前缀，便于直接阅读；两层均为 DEBUG 级别。
/// 返回的 WorkerGuard 必须存活到进程结束，drop 时自动刷盘。
///
/// # Errors
/// 创建或截断日志文件失败时返回 Err。
fn initialize_logging() -> Result<WorkerGuard> {
    let log_dir = Path::new("log");
    let log_filename = today_launcher_log_filename();
    truncate_log_file(log_dir, &log_filename)?;
    // rolling::never 不做按时间/大小轮转，固定写当日文件（启动时已先截断）。
    let file_appender = tracing_appender::rolling::never(log_dir, log_filename);
    let (non_blocking_file, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking_file)
        .with_ansi(false)
        .with_target(false)
        .with_filter(tracing::level_filters::LevelFilter::DEBUG);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(tracing::level_filters::LevelFilter::DEBUG);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(guard)
}

/// 确保日志目录存在并把日志文件截断为空（File::create 语义）。
/// 每次启动重新记录，避免单文件无限增长。
///
/// # Errors
/// 目录创建或文件创建失败时返回 Err。
fn truncate_log_file(log_dir: &Path, filename: &str) -> Result<()> {
    fs::create_dir_all(log_dir)?;
    let path = log_dir.join(filename);
    fs::File::create(&path)
        .with_context(|| format!("truncate launcher log file {}", path.display()))?;
    Ok(())
}

/// Tauri 命令：由注入的 window.saveAs 调用（见 page_load_injector）。
///
/// 前端传入文件名与 base64 编码的文件内容，此处解码后弹出系统"另存为"
/// 对话框并写盘。无返回值，失败仅记录日志。
#[tauri::command]
fn save_as(app_handle: tauri::AppHandle, filename: &str, data: &str) {
    match BASE64_STANDARD.decode(data) {
        // 解码成功：弹出系统"另存为"对话框；写盘在回调中完成，失败只记日志。
        Ok(decoded_data) => app_handle
            .dialog()
            .file()
            .set_file_name(filename)
            .save_file(move |path| {
                let result: Result<()> = (move || {
                    let file_path = path
                        .as_ref()
                        .and_then(FilePath::as_path)
                        .ok_or_else(|| anyhow!(t!("errors.invalid_file_path", path = format!("{:?}", &path))))?;
                    fs::write(file_path, &decoded_data)?;
                    info!("Saved file to {:?}", file_path);
                    Ok(())
                })();
                if let Err(e) = result {
                    error!("Failed to save file: {:?}", e);
                }
            }),
        Err(e) => {
            error!("Failed to decode file content: {:?}", e);
        }
    }
}

/// Tauri 命令：把今天的 GUI 日志（log/{日期}_gui.txt）另存到用户选择的位置。
/// 返回被保存的文件名；读取失败时返回错误字符串。
#[tauri::command]
fn download_today_gui_log(app_handle: tauri::AppHandle) -> std::result::Result<String, String> {
    download_log_file(app_handle, today_gui_log_filename(), "GUI")
}

/// Tauri 命令：把今天的启动器日志（log/{日期}_launcher.txt）另存到用户
/// 选择的位置。返回被保存的文件名；读取失败时返回错误字符串。
#[tauri::command]
fn download_today_launcher_log(
    app_handle: tauri::AppHandle,
) -> std::result::Result<String, String> {
    download_log_file(app_handle, today_launcher_log_filename(), "launcher")
}

/// 读取指定日志文件并弹出另存为对话框。
///
/// # Errors
/// 以 Err(String) 返回（前端直接展示）：获取工作目录或读取日志文件失败时
/// 携带本地化错误文案；保存对话框是异步的，写盘失败只记日志。
fn download_log_file(
    app_handle: tauri::AppHandle,
    filename: String,
    log_name: &str,
) -> std::result::Result<String, String> {
    let log_name = log_name.to_owned();
    let source_path = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join("log")
        .join(&filename);
    let data = fs::read(&source_path).map_err(|e| {
        t!(
            "errors.read_log_file",
            path = source_path.to_string_lossy().to_string(),
            error = e.to_string()
        )
    })?;

    app_handle
        .dialog()
        .file()
        .set_file_name(&filename)
        .save_file(move |path| {
            let log_name_for_save = log_name.clone();
            let result: Result<()> = (move || {
                let file_path = path
                    .as_ref()
                    .and_then(FilePath::as_path)
                    .ok_or_else(|| anyhow!(t!("errors.invalid_file_path", path = format!("{:?}", &path))))?;
                fs::write(file_path, &data)?;
                info!("Saved {} log to {:?}", log_name_for_save, file_path);
                Ok(())
            })();
            if let Err(e) = result {
                error!("Failed to save {} log: {:?}", log_name, e);
            }
        });

    Ok(filename)
}

/// 生成今天的 GUI 日志文件名（{日期}_gui.txt，日期取本地时区）。
fn today_gui_log_filename() -> String {
    format!("{}_gui.txt", Local::now().format("%Y-%m-%d"))
}

/// 生成今天的启动器日志文件名（{日期}_launcher.txt，日期取本地时区）。
fn today_launcher_log_filename() -> String {
    format!("{}_launcher.txt", Local::now().format("%Y-%m-%d"))
}

/// Tauri 命令：隐藏主窗口并最小化到托盘（自定义标题栏的"托盘化"按钮与
/// 关闭选择菜单的"最小化到托盘"选项均调用此命令）。
#[tauri::command]
fn window_hide(app_handle: tauri::AppHandle) -> tauri::Result<()> {
    minimize_main_window_to_tray(&app_handle);
    Ok(())
}

/// Tauri 命令：最小化窗口到任务栏（splash 与标题栏的最小化按钮调用）。
#[tauri::command]
fn window_minimize(window: WebviewWindow) -> tauri::Result<()> {
    window.minimize()
}

/// Tauri 命令：切换窗口最大化/还原状态。
/// 返回切换后是否处于最大化，供标题栏按钮同步图标。
#[tauri::command]
fn window_toggle_maximize(window: WebviewWindow) -> tauri::Result<bool> {
    if window.is_maximized()? {
        window.unmaximize()?;
        Ok(false)
    } else {
        window.maximize()?;
        Ok(true)
    }
}

/// Tauri 命令：关闭窗口（splash 的关闭按钮调用；主窗口的关闭行为由
/// RunEvent::WindowEvent 的 CloseRequested 分支按平台处理）。
#[tauri::command]
fn window_close(window: WebviewWindow) -> tauri::Result<()> {
    window.close()
}

/// Tauri 命令：用户在关闭选择菜单确认退出时调用——置位放行标志并以
/// 退出码 0 结束应用，使 ExitRequested 分支放行真正的关停（含终止后端）。
#[tauri::command]
fn window_exit_application(
    app_handle: tauri::AppHandle,
    exit_control: State<'_, ExitControl>,
) -> tauri::Result<()> {
    exit_control.0.store(true, Ordering::SeqCst);
    app_handle.exit(0);
    Ok(())
}

/// Tauri 命令：开始拖动窗口（标题栏拖拽区与 splash 拖拽区调用）。
#[tauri::command]
fn window_start_dragging(window: WebviewWindow) -> tauri::Result<()> {
    window.start_dragging()
}

/// Tauri 命令：查询窗口当前是否处于最大化（标题栏按钮同步图标用）。
#[tauri::command]
fn window_is_maximized(window: WebviewWindow) -> tauri::Result<bool> {
    window.is_maximized()
}

/// Tauri 命令：错误页面的"重试连接"按钮（含每秒自动重试）调用。
///
/// 在阻塞线程中等待后端端口恢复可达，随后换发免密令牌并导航过去；
/// 超时未恢复返回 Ok(false)（前端提示"仍然失败"），恢复并成功导航返回
/// Ok(true)。
///
/// # Errors
/// 以 Err(String) 返回：阻塞任务失败、URL 解析或导航失败时携带错误信息。
#[tauri::command]
async fn retry_backend_connection(
    window: WebviewWindow,
    port: u16,
) -> std::result::Result<bool, String> {
    // 等待与换发免密令牌均含阻塞调用，统一放在阻塞线程中执行。
    let target_url = tauri::async_runtime::spawn_blocking(move || {
        if wait_for_backend_connection(port, BACKEND_NAVIGATION_TIMEOUT).is_err() {
            return None;
        }
        Some(webui_navigate_url(port))
    })
    .await
    .map_err(|e| {
        error!("Backend retry task failed: {e:?}");
        e.to_string()
    })?;

    let Some(target_url) = target_url else {
        return Ok(false);
    };

    let url = Url::parse(&target_url).map_err(|e| e.to_string())?;
    window.navigate(url).map_err(|e| {
        error!("Failed to navigate to reconnected backend: {e:?}");
        e.to_string()
    })?;
    Ok(true)
}

/// 页面加载完成回调：向主窗口页面注入启动器辅助 JS。
///
/// 注入内容（一个幂等的 IIFE）：阻止浏览器历史后退；把 window.saveAs
/// 覆盖为"经 FileReader 转 base64 后调用 Tauri save_as 命令"的版本；并
/// 拼入自定义标题栏脚本（macOS 除外，见 main_window_titlebar_injection_script）。
fn page_load_injector(webview: WebviewWindow, payload: PageLoadPayload<'_>) {
    // 只在加载完成（Finished）时注入：开始加载阶段文档尚未就绪，注入无效。
    if payload.event() == PageLoadEvent::Finished {
        info!(
            "Injecting saveFile function to loaded page: {}",
            redacted_url_log(payload.url())
        );
        let injected_js = r#"
if (!window.alas_launcher_injected) {
    window.alas_launcher_injected = true;
    (function () {
        // Prevent going back
        history.pushState(null, document.title, location.href);
        window.addEventListener('popstate', event => {
            history.pushState(null, document.title, location.href);
        });
        // Overwrite original saveAs function
        window.saveAs = function (blob, filename) {
            const reader = new FileReader();
            reader.onload = async () => {
                const data = reader.result.split(',')[1];
                console.log(data);
                const tauriInvoke =
                    (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
                    || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
                if (typeof tauriInvoke === 'function') {
                    tauriInvoke('save_as', { filename, data });
                }
            };
            reader.readAsDataURL(blob);
        };
__ALAS_TITLEBAR_SCRIPT__
    })();
}
"#
        .replace(
            "__ALAS_TITLEBAR_SCRIPT__",
            &main_window_titlebar_injection_script(),
        );
        // 注入失败不影响页面本身功能，仅记录日志。
        if let Err(e) = webview.eval(&injected_js) {
            error!("Failed to inject JS to webview: {:?}", e);
        }
    }
}

/// 初始化 splash 窗口：导航到 alas-splash:// 页面，等待页面脚本就绪后按需
/// 显示窗口。就绪探测超时也照样显示，保证用户总能看到启动画面。
fn initialize_splash(splash: &WebviewWindow, show_window: bool) {
    match Url::parse(SPLASH_URL) {
        Ok(url) => {
            if let Err(e) = splash.navigate(url) {
                error!("Failed to navigate splash page: {:?}", e);
            }
            if !wait_for_splash_ready(splash, Duration::from_secs(2)) {
                warn!("Timed out waiting for splash page readiness; showing splash anyway");
            }
            // show_window 为 false 对应最小化启动：先不显示，等后端就绪再唤起。
            if show_window {
                if let Err(e) = splash.show() {
                    error!("Failed to show splash window: {:?}", e);
                }
            }
        }
        Err(e) => {
            error!("Failed to parse splash URL: {:?}", e);
        }
    }
}

/// 轮询探测 splash 页面是否就绪：执行一段检查 __ALAS_SPLASH_READY 标志的
/// JS，eval 成功即认为页面的更新回调已挂好、可以接收进度推送。
fn wait_for_splash_ready(splash: &WebviewWindow, timeout: Duration) -> bool {
    let started_at = Instant::now();
    while started_at.elapsed() < timeout {
        if splash
            .eval(
                r#"
                if (!window.__ALAS_SPLASH_READY) {
                    throw new Error("splash page is not ready");
                }
                "#,
            )
            .is_ok()
        {
            return true;
        }
        // 25ms 轮询：足够灵敏且不给 WebView 造成明显压力。
        thread::sleep(Duration::from_millis(25));
    }
    false
}

/// 把 SplashUpdate 序列化为 JSON 并调用 splash 页面的
/// window.__ALAS_SPLASH_UPDATE 回调刷新界面。
fn update_splash(splash: &WebviewWindow, update: &SplashUpdate) {
    let payload = to_string(update).unwrap();
    let script = format!("window.__ALAS_SPLASH_UPDATE && window.__ALAS_SPLASH_UPDATE({payload});");
    if let Err(e) = splash.eval(&script) {
        error!("Failed to update splash page: {:?}", e);
    }
}

/// 拼接后端 WebUI 的本地地址（http://127.0.0.1:{port}/）。
fn backend_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

/// 构造 alas-splash:// 协议的 HTML 响应：视频背景与字体以 base64 内嵌，
/// 无外部资源依赖，保证离线首屏可用。
fn splash_response() -> tauri::http::Response<Vec<u8>> {
    let video_bg_b64 = BASE64_STANDARD.encode(SPLASH_BG_VIDEO);
    let mi_sans_font_b64 = BASE64_STANDARD.encode(MI_SANS_FONT);
    tauri::http::Response::builder()
        .header(
            tauri::http::header::CONTENT_TYPE,
            "text/html; charset=utf-8",
        )
        .body(splash_redesigned_shell_html(&video_bg_b64, &mi_sans_font_b64).into_bytes())
        .unwrap()
}

/// 用一次 TCP 连接探测后端端口是否可达。
///
/// # Errors
/// 地址解析失败或连接超时（BACKEND_CONNECT_TIMEOUT）时返回 Err。
fn check_backend_connection(port: u16) -> Result<()> {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    TcpStream::connect_timeout(&address, BACKEND_CONNECT_TIMEOUT)
        .map(|_| ())
        .map_err(|e| {
        anyhow!(t!(
            "errors.backend_unreachable",
            address = address.to_string(),
            error = e.to_string()
        ))
    })
}

/// 未就绪提示与日志中的失败原因合成最终错误：日志里写了原因时一并带出。
fn backend_unready_error(timeout_error: anyhow::Error, reason: Option<String>) -> anyhow::Error {
    match reason {
        Some(reason) => anyhow!(t!(
            "errors.backend_unready_with_reason",
            error = format!("{timeout_error:#}"),
            reason = reason
        )),
        None => timeout_error,
    }
}

/// 在给定时限内轮询后端端口直到可达。
///
/// 每 200ms 探测一次；超时后把最后一次连接错误与后端日志中发现的失败
/// 原因合成最终错误（见 backend_unready_error）。
///
/// # Errors
/// 超时仍不可达时返回 Err（附带最后一次连接错误与可能的后端失败原因）。
fn wait_for_backend_connection(port: u16, timeout: Duration) -> Result<()> {
    let started_at = Instant::now();
    let mut last_error = None;
    while started_at.elapsed() < timeout {
        match check_backend_connection(port) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_error = Some(e);
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    let timeout_error = last_error.unwrap_or_else(|| anyhow!(t!("errors.backend_timeout")));
    Err(backend_unready_error(
        timeout_error,
        crate::backend::read_backend_failure_reason(),
    ))
}

/// 等待后端就绪后把窗口导航到 WebUI（必要时带免密令牌）；失败则导航到
/// alas-error:// 错误页面。
///
/// # Returns
/// Ok(true) 表示已导航到后端；Ok(false) 表示后端不可达、已转错误页面。
///
/// # Errors
/// 仅当错误页导航本身失败（URL 解析或 WebView 导航出错）时返回 Err。
fn navigate_backend_or_error(window: &WebviewWindow, port: u16) -> Result<bool> {
    match wait_for_backend_connection(port, BACKEND_NAVIGATION_TIMEOUT) {
        Ok(()) => {
            let url = webui_navigate_url(port);
            window.navigate(Url::parse(&url)?)?;
            Ok(true)
        }
        Err(e) => {
            warn!("Backend connection check failed before navigation: {:?}", e);
            navigate_to_backend_error(window, port, &e.to_string())?;
            Ok(false)
        }
    }
}

/// 把窗口导航到携带端口与错误详情的 alas-error:// 错误页面。
///
/// # Errors
/// 错误 URL 构造或导航失败时返回 Err。
fn navigate_to_backend_error(window: &WebviewWindow, port: u16, error_detail: &str) -> Result<()> {
    let url = backend_error_url(port, error_detail)?;
    window.navigate(url)?;
    Ok(())
}

/// 构造错误页面 URL：port 与错误详情作为 query 参数，经 URL 编码安全传递。
///
/// # Errors
/// URL 解析失败时返回 Err（基路径是编译期常量，正常不会失败）。
fn backend_error_url(port: u16, error_detail: &str) -> Result<Url> {
    let port = port.to_string();
    Ok(Url::parse_with_params(
        BACKEND_ERROR_URL_BASE,
        [("port", port.as_str()), ("detail", error_detail)],
    )?)
}

/// alas-error:// 协议处理器：从请求 URI 解析 port/detail，返回错误页 HTML。
fn backend_error_response(
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let (port, detail) = backend_error_request_params(request.uri().to_string().as_str());
    let html = backend_error_html(port, &detail);

    tauri::http::Response::builder()
        .header(
            tauri::http::header::CONTENT_TYPE,
            "text/html; charset=utf-8",
        )
        .body(html.into_bytes())
        .unwrap()
}

/// 从错误页请求 URI 中提取 port 与 detail query 参数。
///
/// 缺省值为默认端口 22267 与通用"无法连接"文案；非法端口直接忽略。
fn backend_error_request_params(uri: &str) -> (u16, String) {
    let mut port = 22267;
    let mut detail = t!("error_page.unable_connect").to_string();

    if let Ok(url) = Url::parse(uri) {
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "port" => {
                    if let Ok(parsed_port) = value.parse::<u16>() {
                        port = parsed_port;
                    }
                }
                "detail" => detail = value.into_owned(),
                _ => {}
            }
        }
    }

    (port, detail)
}

/// 主窗口导航拦截：目标不是后端地址时放行；是后端地址但连接不上时阻止
/// 本次导航，并异步把窗口切到错误页面。
///
/// # Returns
/// true 表示放行导航，false 表示已拦截。
fn handle_backend_navigation(app: tauri::AppHandle, port: u16, url: &Url) -> bool {
    if !is_backend_url(url, port) {
        return true;
    }

    match check_backend_connection(port) {
        Ok(()) => true,
        Err(e) => {
            let blocked_url = redacted_url_log(url);
            warn!(
                "Blocked navigation to unreachable backend {}: {:?}",
                blocked_url, e
            );
            let error_detail = e.to_string();
            // 在独立线程里切换到错误页，避免在导航回调内同步改写导航目标。
            thread::spawn(move || {
                if let Some(window) = app.get_webview_window("main") {
                    if let Err(e) = navigate_to_backend_error(&window, port, &error_detail) {
                        error!("Failed to show backend error page: {:?}", e);
                    }
                }
            });
            false
        }
    }
}

/// 判断 URL 是否指向本地后端：http/https + 127.0.0.1/localhost + 目标端口。
fn is_backend_url(url: &Url, port: u16) -> bool {
    matches!(url.scheme(), "http" | "https")
        && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
        && url.port_or_known_default() == Some(port)
}

/// 转义 HTML 特殊字符（& < > " '），用于把本地化文案安全嵌入 HTML。
fn escape_html(input: impl AsRef<str>) -> String {
    input
        .as_ref()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 生成 alas-error:// 错误页面的完整 HTML。
///
/// 页面包含：内嵌字体与 splash 视频背景、后端地址与错误详情展示、"重试"
/// 按钮（页面 JS 每秒自动重试一次）、GUI/启动器日志下载按钮。标题栏脚本
/// 同样注入，保持无边框窗口可拖拽；本地化文案以 JSON 注入前端。
fn backend_error_html(port: u16, error_detail: &str) -> String {
    let backend_url_json = to_string(&backend_url(port)).unwrap();
    let error_detail_json = to_string(error_detail).unwrap();
    let mi_sans_font_b64 = BASE64_STANDARD.encode(MI_SANS_FONT);
    let splash_video_b64 = BASE64_STANDARD.encode(SPLASH_BG_VIDEO);
    let titlebar_script = main_window_titlebar_injection_script();
    let i18n = serde_json::json!({
        "title": t!("error_page.title"),
        "heading": t!("error_page.heading"),
        "description": t!("error_page.description"),
        "address": t!("error_page.address"),
        "errorLabel": t!("error_page.error_label"),
        "retry": t!("error_page.retry"),
        "downloadGuiLog": t!("error_page.download_gui_log"),
        "downloadLauncherLog": t!("error_page.download_launcher_log"),
        "reconnecting": t!("error_page.reconnecting"),
        "stillFailed": t!("error_page.still_failed"),
        "retryFailed": t!("error_page.retry_failed"),
        "preparing": t!("error_page.preparing"),
        "saved": t!("error_page.saved"),
        "downloadFailed": t!("error_page.download_failed"),
    });
    let i18n_json = to_string(&i18n).unwrap();

    // HTML/CSS/JS 内的字面大括号在 format! 里需双写转义；文案经 JSON 注入
    // 避免引号/换行破坏 HTML 结构。
    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  @font-face {{
    font-family: "MiSans";
    src: url(data:font/ttf;base64,{mi_sans_font_b64}) format("truetype");
    font-weight: 100 900;
    font-style: normal;
    font-display: swap;
  }}
  :root {{
    color-scheme: light;
    --bg: #f4f6f8;
    --surface: #ffffff;
    --surface-soft: #f8fafb;
    --line: #e5e9ee;
    --text: #17212b;
    --muted: #687582;
    --accent: #176b67;
    --accent-hover: #105854;
    --accent-soft: #e8f4f2;
    --danger: #b64545;
    --danger-soft: #fff1f0;
  }}
  * {{
    box-sizing: border-box;
  }}
  html, body {{
    width: 100%;
    min-height: 100%;
    margin: 0;
    font-family: "MiSans", sans-serif;
    font-weight: 420;
    font-synthesis: none;
    color: var(--text);
    background: #dfe7ea;
  }}
  body {{
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 72px 44px 44px;
    position: relative;
    isolation: isolate;
    overflow: hidden;
    background: transparent;
    animation: page-in 420ms cubic-bezier(0.22, 1, 0.36, 1) both;
  }}
  .error-background-video {{
    position: fixed;
    inset: 0;
    z-index: 0;
    width: 100%;
    height: 100%;
    object-fit: cover;
    opacity: 0.9;
    pointer-events: none;
  }}
  .error-background-scrim {{
    position: fixed;
    inset: 0;
    z-index: 1;
    background: rgba(244, 247, 248, 0.36);
    pointer-events: none;
  }}
  .panel {{
    position: relative;
    z-index: 2;
    display: grid;
    grid-template-columns: 190px minmax(0, 1fr);
    width: min(820px, 100%);
    min-height: 390px;
    overflow: hidden;
    border: 1px solid var(--line);
    border-radius: 14px;
    background: rgba(255, 255, 255, 0.76);
    backdrop-filter: blur(22px) saturate(1.08);
    box-shadow: 0 20px 48px rgba(23, 33, 43, 0.11), 0 2px 6px rgba(23, 33, 43, 0.04);
    animation: panel-in 520ms cubic-bezier(0.22, 1, 0.36, 1) 70ms both;
  }}
  .signal {{
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--accent);
    color: #fff;
  }}
  .signal::before, .signal::after {{
    content: "";
    position: absolute;
    border: 1px solid rgba(255, 255, 255, 0.17);
    border-radius: 50%;
    opacity: 0;
    animation: signal-expand 3.2s ease-out infinite;
  }}
  .signal::before {{ width: 76px; height: 76px; }}
  .signal::after {{ width: 76px; height: 76px; animation-delay: 1.6s; }}
  .signal-core {{
    position: relative;
    z-index: 1;
    width: 76px;
    height: 76px;
    display: grid;
    place-items: center;
    border: 1px solid rgba(255, 255, 255, 0.45);
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.12);
    animation: core-breathe 2.8s ease-in-out infinite;
  }}
  .signal-core svg {{
    width: 36px;
    height: 36px;
    fill: none;
    stroke: currentColor;
    stroke-linecap: round;
    stroke-linejoin: round;
    stroke-width: 1.7;
  }}
  .content {{
    display: flex;
    flex-direction: column;
    min-width: 0;
    padding: 38px 42px 32px;
    animation: content-in 500ms cubic-bezier(0.22, 1, 0.36, 1) 140ms both;
  }}
  .eyebrow {{
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--muted);
    font-size: 11px;
    font-weight: 620;
    letter-spacing: 1.2px;
    text-transform: uppercase;
  }}
  .eyebrow::before {{
    content: "";
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--danger);
    box-shadow: 0 0 0 4px var(--danger-soft);
    animation: status-pulse 2s ease-in-out infinite;
  }}
  h1 {{
    max-width: 470px;
    margin: 16px 0 0;
    font-size: 30px;
    font-weight: 680;
    letter-spacing: -0.3px;
    line-height: 1.18;
  }}
  .lead {{
    max-width: 510px;
    margin: 12px 0 0;
    color: var(--muted);
    font-size: 14px;
    font-weight: 430;
    line-height: 1.65;
  }}
  .details {{
    margin: 24px 0 0;
    border: 1px solid var(--line);
    border-radius: 8px;
    overflow: hidden;
    background: var(--surface-soft);
  }}
  .row {{
    display: grid;
    grid-template-columns: 70px minmax(0, 1fr);
    gap: 14px;
    padding: 10px 13px;
    border-top: 1px solid var(--line);
    font-size: 12px;
    font-weight: 460;
    line-height: 1.5;
  }}
  .row:first-child {{ border-top: 0; }}
  .label {{ color: var(--muted); }}
  .value {{
    min-width: 0;
    overflow-wrap: anywhere;
    color: var(--text);
    font-family: inherit;
    font-weight: 500;
    font-variant-numeric: tabular-nums;
  }}
  .actions {{
    display: flex;
    align-items: center;
    gap: 9px;
    flex-wrap: wrap;
    margin-top: auto;
    padding-top: 24px;
  }}
  button {{
    min-height: 36px;
    border: 1px solid transparent;
    border-radius: 7px;
    padding: 0 13px;
    font: inherit;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
    transition: background 140ms ease, border-color 140ms ease, color 140ms ease, opacity 140ms ease;
    will-change: transform;
  }}
  button:hover {{ transform: translateY(-1px); }}
  button:active {{ transform: translateY(0); }}
  .action-button {{
    color: #fff;
    background: var(--accent);
  }}
  .action-button:hover {{ background: var(--accent-hover); }}
  .secondary-button {{
    color: var(--accent);
    border-color: #c5dfdc;
    background: var(--accent-soft);
  }}
  .secondary-button:hover {{ background: #dcefeb; border-color: #a8d2cd; }}
  button:disabled {{ cursor: default; opacity: 0.55; }}
  button:disabled:hover {{ transform: none; }}
  .status {{
    flex: 1 1 100%;
    min-height: 18px;
    color: var(--muted);
    font-size: 12px;
    font-weight: 460;
  }}
  .footer {{
    margin-top: 16px;
    color: #9aa5ae;
    font-size: 11px;
    font-weight: 430;
  }}
  @media (max-width: 680px) {{
    body {{ padding: 62px 18px 24px; align-items: flex-start; }}
    .panel {{ grid-template-columns: 1fr; min-height: 0; }}
    .signal {{ min-height: 120px; }}
    .signal::before {{ width: 76px; height: 76px; }}
    .signal::after {{ width: 76px; height: 76px; }}
    .content {{ padding: 28px 24px 24px; }}
    h1 {{ font-size: 25px; }}
    .actions {{ margin-top: 22px; }}
    button {{ flex: 1 1 auto; }}
  }}
  @media (max-width: 420px) {{
    .row {{ grid-template-columns: 1fr; gap: 3px; }}
    button {{ width: 100%; }}
  }}
  @keyframes page-in {{ from {{ opacity: 0; }} to {{ opacity: 1; }} }}
  @keyframes panel-in {{ from {{ opacity: 0; transform: translateY(12px) scale(0.985); }} to {{ opacity: 1; transform: translateY(0) scale(1); }} }}
  @keyframes content-in {{ from {{ opacity: 0; transform: translateX(10px); }} to {{ opacity: 1; transform: translateX(0); }} }}
  @keyframes signal-expand {{ 0% {{ opacity: 0.72; transform: scale(0.72); }} 68% {{ opacity: 0.12; }} 100% {{ opacity: 0; transform: scale(2.8); }} }}
  @keyframes core-breathe {{ 0%, 100% {{ transform: scale(1); }} 50% {{ transform: scale(1.045); }} }}
  @keyframes status-pulse {{ 0%, 100% {{ opacity: 0.62; }} 50% {{ opacity: 1; }} }}
  @media (prefers-reduced-motion: reduce) {{
    *, *::before, *::after {{ animation-duration: 0.01ms !important; animation-iteration-count: 1 !important; transition-duration: 0.01ms !important; }}
  }}
</style>
</head>
<body>
  <video class="error-background-video" autoplay muted loop playsinline preload="auto" aria-hidden="true">
    <source src="data:video/mp4;base64,{splash_video_b64}" type="video/mp4">
  </video>
  <div class="error-background-scrim" aria-hidden="true"></div>
  <main class="panel">
    <div class="signal" aria-hidden="true">
      <div class="signal-core">
        <svg viewBox="0 0 24 24"><path d="M12 8v4m0 4h.01"/><path d="M10.3 3.9 2.8 17a2 2 0 0 0 1.7 3h15a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0Z"/></svg>
      </div>
    </div>
    <div class="content">
      <div class="eyebrow">{error_label}</div>
      <h1>{heading}</h1>
      <p class="lead">{description}</p>
      <section class="details" aria-label="{connection_info}">
        <div class="row">
          <div class="label">{address}</div>
          <div id="backend-url" class="value"></div>
        </div>
        <div class="row">
          <div class="label">{error_label}</div>
          <div id="error-detail" class="value"></div>
        </div>
      </section>
      <div class="actions">
        <button id="retry-button" class="action-button" type="button">{retry}</button>
        <button id="gui-log-button" class="secondary-button" type="button">{download_gui_log}</button>
        <button id="launcher-log-button" class="secondary-button" type="button">{download_launcher_log}</button>
        <span id="retry-status" class="status"></span>
      </div>
      <div class="footer">AzurPilot · {connection_info}</div>
    </div>
  </main>
  <script>
    (function () {{
{titlebar_script}
    }})();

    const i18n = {i18n_json};
    const backendUrl = {backend_url_json};
    const errorDetail = {error_detail_json};
    const port = {port};
    const retryButton = document.getElementById('retry-button');
    const guiLogButton = document.getElementById('gui-log-button');
    const launcherLogButton = document.getElementById('launcher-log-button');
    const retryStatus = document.getElementById('retry-status');
    const invoke =
      (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
      || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);

    document.getElementById('backend-url').textContent = backendUrl;
    document.getElementById('error-detail').textContent = errorDetail;

    retryButton.addEventListener('click', async () => {{
      retryButton.disabled = true;
      retryStatus.textContent = i18n.reconnecting;
      try {{
        if (typeof invoke !== 'function') {{
          throw new Error('Tauri invoke is unavailable');
        }}
        const connected = await invoke('retry_backend_connection', {{ port }});
        if (!connected) {{
          retryStatus.textContent = i18n.stillFailed;
          retryButton.disabled = false;
        }}
      }} catch (error) {{
        retryStatus.textContent = i18n.retryFailed + (error && error.message ? error.message : error);
        retryButton.disabled = false;
      }}
    }});

    async function downloadLog(button, command, label) {{
      button.disabled = true;
      retryStatus.textContent = i18n.preparing.replace('%{{label}}', label);
      try {{
        if (typeof invoke !== 'function') {{
          throw new Error('Tauri invoke is unavailable');
        }}
        const filename = await invoke(command);
        retryStatus.textContent = i18n.saved.replace('%{{filename}}', filename);
      }} catch (error) {{
        retryStatus.textContent = i18n.downloadFailed.replace('%{{label}}', label) + (error && error.message ? error.message : error);
      }} finally {{
        button.disabled = false;
      }}
    }}

    guiLogButton.addEventListener('click', () => {{
      downloadLog(guiLogButton, 'download_today_gui_log', '{gui_log_label}');
    }});

    launcherLogButton.addEventListener('click', () => {{
      downloadLog(launcherLogButton, 'download_today_launcher_log', '{launcher_log_label}');
    }});

    // 每秒尝试自动刷新（重试连接）
    setInterval(() => {{
      if (!retryButton.disabled) {{
        retryButton.click();
      }}
    }}, 1000);
  </script>
</body>
</html>"#,
        title = t!("error_page.title"),
        heading = t!("error_page.heading"),
        description = t!("error_page.description"),
        address = t!("error_page.address"),
        error_label = t!("error_page.error_label"),
        retry = t!("error_page.retry"),
        download_gui_log = t!("error_page.download_gui_log"),
        download_launcher_log = t!("error_page.download_launcher_log"),
        gui_log_label = t!("error_page.download_gui_log"),
        launcher_log_label = t!("error_page.download_launcher_log"),
        connection_info = t!("error_page.connection_info"),
    )
}

/// 生成 splash 启动画面的完整 HTML 外壳。
///
/// 包含：视频背景、自定义红绿灯窗口按钮（最小化/关闭）、顶部拖拽区、
/// 主进度条（含可选的 uv 安装进度子条）、错误态样式与"下载日志"按钮。
/// 静态占位符经 replace 注入：base64 资源、版本号、本地化 JSON 与文案、
/// 以及仅 Windows 启用的原生触摸拖拽开关。
fn splash_redesigned_shell_html(video_bg_b64: &str, mi_sans_font_b64: &str) -> String {
    // 页面文案聚合为 JSON 注入，前端按 key 取用，避免逐个拼接转义。
    let i18n = serde_json::json!({
        "defaultTip": t!("tips.17"),
        "loading": t!("splash.loading_badge"),
        "webuiInit": t!("splash.webui_init"),
        "starting": t!("splash.starting"),
        "errorBadge": t!("splash.error_badge"),
        "initStopped": t!("splash.init_stopped"),
        "progressMetaReady": t!("splash.progress_meta_ready"),
        "preparingLog": t!("splash.preparing_log"),
        "logSavedPrefix": t!("splash.log_saved_prefix"),
        "logFailed": t!("splash.log_failed"),
    });
    let i18n_json = to_string(&i18n).unwrap();

    r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  @font-face {
    font-family: "MiSans";
    src: url(data:font/ttf;base64,$MI_SANS_FONT) format("truetype");
    font-weight: 100 900;
    font-style: normal;
    font-display: swap;
  }
  :root {
    --primary-color: #4facfe;
    --secondary-color: #00f2fe;
    --text-main: #ffffff;
    --text-sub: rgba(255, 255, 255, 0.76);
    --text-muted: rgba(255, 255, 255, 0.52);
    --surface-soft: rgba(255, 255, 255, 0.16);
    --surface-border: rgba(255, 255, 255, 0.15);
    --danger: #ff5f57;
    --warning: #ffbd2e;
  }
  * {
    box-sizing: border-box;
    margin: 0;
    padding: 0;
    user-select: none;
  }
  html,
  body {
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: #111827;
  }
  body {
    font-family: "MiSans", sans-serif;
    font-weight: 420;
    font-synthesis: none;
    color: var(--text-main);
  }
  button {
    font: inherit;
  }
  .launcher-window {
    position: relative;
    width: 100%;
    height: 100%;
    overflow: hidden;
    border-radius: 0;
    background: #111827;
    box-shadow: none;
    display: flex;
    flex-direction: column;
    justify-content: space-between;
  }
  .splash-background-video {
    position: absolute;
    inset: 0;
    z-index: 0;
    width: 100%;
    height: 100%;
    object-fit: cover;
    pointer-events: none;
  }
  .launcher-window::before {
    content: "";
    position: absolute;
    inset: 0;
    z-index: 1;
    background:
      linear-gradient(to bottom, rgba(0, 0, 0, 0.05) 0%, rgba(0, 0, 0, 0.03) 42%, rgba(0, 0, 0, 0.28) 100%),
      linear-gradient(115deg, rgba(12, 30, 72, 0.10), rgba(255, 126, 117, 0.05));
    pointer-events: none;
  }
  .top-bar {
    position: relative;
    z-index: 2;
    display: flex;
    justify-content: space-between;
    align-items: center;
    min-height: 56px;
    padding: 10px 18px;
    touch-action: none;
    app-region: drag;
    -webkit-app-region: drag;
  }
  .brand-zone {
    display: flex;
    align-items: center;
    min-width: 0;
    gap: 10px;
  }
  .app-title {
    color: var(--text-main);
    font-size: 18px;
    font-weight: 610;
    letter-spacing: 0;
    text-shadow: 0 2px 6px rgba(0, 0, 0, 0.22);
  }
  .app-version {
    color: var(--text-sub);
    font-size: 12px;
    font-weight: 460;
    line-height: 1;
    background: rgba(255, 255, 255, 0.14);
    border: 1px solid rgba(255, 255, 255, 0.11);
    padding: 4px 9px;
    border-radius: 999px;
    backdrop-filter: blur(8px);
  }
  .top-right {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .status-badge {
    max-width: 260px;
    min-height: 32px;
    display: inline-flex;
    align-items: center;
    gap: 7px;
    border-radius: 999px;
    padding: 6px 13px;
    color: #394451;
    background: rgba(250, 250, 247, 0.78);
    border: 1px solid rgba(255, 255, 255, 0.92);
    backdrop-filter: blur(16px) saturate(1.2);
    box-shadow: 0 4px 14px rgba(61, 79, 97, 0.1), inset 0 1px 0 rgba(255, 255, 255, 0.36);
    font-size: 12px;
    font-weight: 460;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .status-badge::before {
    content: "";
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--secondary-color);
    box-shadow: 0 0 12px rgba(0, 242, 254, 0.7);
    flex: 0 0 auto;
  }
  .window-controls {
    display: flex;
    align-items: center;
    gap: 2px;
    min-height: 36px;
    padding: 3px 4px;
    border: 1px solid rgba(255, 255, 255, 0.92);
    border-radius: 18px;
    background: rgba(250, 250, 247, 0.78);
    box-shadow: 0 4px 14px rgba(61, 79, 97, 0.1), inset 0 1px 0 rgba(255, 255, 255, 0.36);
    backdrop-filter: blur(16px) saturate(1.2);
    flex: 0 0 auto;
    app-region: no-drag;
    -webkit-app-region: no-drag;
  }
  .window-controls * {
    app-region: no-drag;
    -webkit-app-region: no-drag;
  }
  .win-btn {
    width: 28px;
    height: 28px;
    border: 0;
    border-radius: 12px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    cursor: pointer;
    padding: 0;
    color: #727b86;
    background: transparent;
    transition: transform 140ms cubic-bezier(.23, 1, .32, 1), background-color 140ms ease, color 140ms ease;
  }
  .win-btn:hover {
    color: #202832;
    background: rgba(255, 255, 255, 0.72);
  }
  .win-btn:active {
    transform: scale(0.96);
  }
  .win-btn svg {
    width: 11px;
    height: 11px;
    stroke: currentColor;
    stroke-width: 1.35;
    stroke-linecap: round;
    opacity: 1;
  }
  .win-btn.minimize {
    color: #727b86;
  }
  .win-btn.close {
    color: #e64f58;
  }
  .win-btn.close:hover {
    color: #b5202e;
    background: rgba(244, 91, 91, 0.15);
  }
  .main-content {
    position: relative;
    z-index: 2;
    padding: 0 40px 35px;
  }
  .update-status {
    margin-bottom: 25px;
    max-width: min(650px, 100%);
  }
  .title-group {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 8px;
  }
  .spinner {
    width: 22px;
    height: 22px;
    border: 2.5px solid rgba(255, 255, 255, 0.24);
    border-top-color: var(--text-main);
    border-radius: 50%;
    animation: spin 0.9s linear infinite;
    flex: 0 0 auto;
  }
  .err-dot {
    width: 22px;
    height: 22px;
    border-radius: 50%;
    background: #ffffff;
    color: #c73532;
    align-items: center;
    justify-content: center;
    font-size: 14px;
    font-weight: 800;
    box-shadow: 0 5px 16px rgba(0, 0, 0, 0.2);
    flex: 0 0 auto;
  }
  .main-action-text {
    min-width: 0;
    color: var(--text-main);
    font-size: 24px;
    line-height: 1.2;
    font-weight: 620;
    letter-spacing: 0;
    text-shadow: 0 2px 10px rgba(0, 0, 0, 0.32);
  }
  .sub-action-text {
    color: var(--text-sub);
    font-size: 12px;
    font-weight: 480;
    letter-spacing: 1.2px;
    line-height: 1.45;
    margin: 0;
    max-width: min(650px, 100%);
    max-height: 54px;
    overflow: hidden;
    text-shadow: 0 1px 5px rgba(0, 0, 0, 0.28);
    white-space: pre-line;
  }
  .progress-container {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    gap: 12px;
    margin-bottom: 15px;
    min-height: 40px;
  }
  .progress-bar-bg {
    grid-column: 1;
    grid-row: 1;
    width: 100%;
    height: 5px;
    border-radius: 999px;
    background: rgba(255, 255, 255, 0.2);
    position: relative;
    overflow: visible;
    box-shadow: inset 0 1px 1px rgba(0, 0, 0, 0.12);
    backdrop-filter: blur(8px);
  }
  .progress-bar-fill {
    width: 4%;
    height: 100%;
    border-radius: inherit;
    background: linear-gradient(90deg, #4facfe, #43d7f5);
    box-shadow: 0 0 10px rgba(67, 215, 245, 0.38);
    position: relative;
    overflow: hidden;
    transition: width 0.35s cubic-bezier(.23, 1, .32, 1), background-color 0.2s ease;
  }
  .progress-head {
    position: absolute;
    left: clamp(30px, var(--progress, 4%), calc(100% - 30px));
    top: 50%;
    width: 60px;
    height: 40px;
    object-fit: contain;
    transform: translate(-50%, -80%);
    pointer-events: none;
    user-select: none;
    transition: left 0.35s cubic-bezier(.23, 1, .32, 1);
  }
  .progress-bar-fill::after {
    display: none;
  }
  .progress-bar-fill-error {
    background: linear-gradient(90deg, #ff5f57, #ffbd2e);
    box-shadow: 0 0 14px rgba(255, 95, 87, 0.46);
  }
  .progress-bar-fill-error::after {
    display: none;
  }
  .progress-percentage {
    grid-column: 2;
    grid-row: 1;
    min-width: 34px;
    color: var(--text-main);
    font-size: 12px;
    font-weight: 560;
    text-align: right;
    font-variant-numeric: tabular-nums;
    text-shadow: 0 1px 5px rgba(0, 0, 0, 0.28);
  }
  .uv-progress-container {
    display: none;
    margin-top: -3px;
    margin-bottom: 15px;
  }
  .uv-progress-container.is-visible {
    display: block;
  }
  .uv-progress-header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 6px;
    color: var(--text-sub);
    font-size: 11px;
    font-variant-numeric: tabular-nums;
  }
  .uv-progress-detail {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-align: right;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .uv-progress-bar-bg {
    width: 100%;
    height: 4px;
    overflow: hidden;
    border-radius: 999px;
    background: rgba(255, 255, 255, 0.14);
  }
  .uv-progress-bar-fill {
    position: relative;
    width: 2%;
    height: 100%;
    overflow: hidden;
    border-radius: inherit;
    background: #55cda0;
    box-shadow: 0 0 8px rgba(85, 205, 160, 0.34);
    transition: width 0.4s cubic-bezier(.23, 1, .32, 1);
  }
  .uv-progress-bar-fill::after {
    display: none;
  }
  .footer-info {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
    min-height: 28px;
    font-size: 12px;
  }
  .tip-text {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
    max-width: 520px;
    color: var(--text-sub);
    background: rgba(15, 23, 42, 0.26);
    border: 1px solid rgba(255, 255, 255, 0.16);
    border-radius: 12px;
    padding: 7px 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-weight: 460;
    backdrop-filter: blur(12px) saturate(1.1);
  }
  .tip-text::before {
    content: "✦";
    color: var(--primary-color);
    font-size: 12px;
    line-height: 1;
    text-shadow: 0 0 10px rgba(79, 172, 254, 0.55);
    flex: 0 0 auto;
  }
  .footer-right {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 10px;
    flex: 0 0 auto;
  }
  .notice-text {
    color: var(--text-muted);
    white-space: nowrap;
    font-weight: 450;
  }
  .splash-actions {
    display: none;
  }
  .splash-actions-err {
    display: block;
  }
  .splash-log-button {
    min-height: 34px;
    border: 1px solid rgba(255, 255, 255, 0.78);
    border-radius: 12px;
    padding: 0 14px;
    color: #394451;
    background: rgba(250, 250, 247, 0.78);
    box-shadow: 0 4px 14px rgba(61, 79, 97, 0.1), inset 0 1px 0 rgba(255, 255, 255, 0.36);
    backdrop-filter: blur(14px) saturate(1.15);
    cursor: pointer;
    font-size: 12px;
    font-weight: 560;
    transition: transform 140ms cubic-bezier(.23, 1, .32, 1), background-color 140ms ease;
  }
  .splash-log-button:hover {
    background: rgba(255, 255, 255, 0.9);
  }
  .splash-log-button:active {
    transform: scale(0.97);
  }
  .splash-log-button:disabled {
    cursor: default;
    opacity: 0.65;
  }
  body.error-state .status-badge {
    background: rgba(255, 255, 255, 0.18);
    animation: none;
  }
  body.error-state .status-badge::before {
    background: #ff5f57;
    box-shadow: 0 0 12px rgba(255, 95, 87, 0.76);
  }
  body.error-state .tip-text {
    border-color: rgba(255, 189, 46, 0.42);
  }
  body.error-state .tip-text::before {
    color: #ffbd2e;
  }
  @media (max-width: 720px) {
    .top-bar {
      padding: 10px 16px;
    }
    .status-badge {
      max-width: 180px;
    }
    .main-content {
      padding: 0 28px 28px;
    }
    .main-action-text {
      font-size: 22px;
    }
  }
  @media (max-width: 560px), (max-height: 340px) {
    .top-right {
      gap: 12px;
    }
    .status-badge {
      display: none;
    }
    .footer-info {
      flex-direction: column;
      align-items: flex-start;
      gap: 8px;
    }
    .footer-right {
      width: 100%;
      justify-content: space-between;
    }
    .tip-text {
      max-width: 100%;
    }
  }
  @media (max-height: 340px) {
    .main-content {
      padding-bottom: 24px;
    }
    .update-status {
      margin-bottom: 18px;
    }
    .sub-action-text {
      max-height: 36px;
    }
  }
  @keyframes spin {
    to { transform: rotate(360deg); }
  }
</style>
</head>
<body>
  <div class="launcher-window">
    <video class="splash-background-video" autoplay muted loop playsinline preload="auto" aria-hidden="true">
      <source src="data:video/mp4;base64,$VIDEO_BG" type="video/mp4">
    </video>
    <div id="splash-drag-region" class="top-bar">
      <div class="brand-zone">
        <span class="app-title">AzurPilot</span>
        <span class="app-version">v$LAUNCHER_VERSION</span>
      </div>
      <div class="top-right">
        <div id="badge" class="status-badge">
          <span id="badge-text">$I18N_INITIALIZING</span>
        </div>
        <div class="window-controls">
          <button id="window-minimize" class="win-btn minimize" type="button" aria-label="$I18N_MINIMIZE" title="$I18N_MINIMIZE">
            <svg viewBox="0 0 8 8" aria-hidden="true"><line x1="2" y1="4" x2="6" y2="4"></line></svg>
          </button>
          <button id="window-close" class="win-btn close" type="button" aria-label="$I18N_CLOSE" title="$I18N_CLOSE">
            <svg viewBox="0 0 8 8" aria-hidden="true"><line x1="2" y1="2" x2="6" y2="6"></line><line x1="6" y1="2" x2="2" y2="6"></line></svg>
          </button>
        </div>
      </div>
    </div>

    <div class="main-content">
      <div class="update-status">
        <div class="title-group">
          <div id="spinner" class="spinner"></div>
          <div id="error-dot" class="err-dot" style="display:none;">!</div>
          <h1 id="title" class="main-action-text">$I18N_STARTING</h1>
        </div>
        <p id="detail" class="sub-action-text">$I18N_WEBUI_INIT</p>
      </div>

      <div class="progress-container">
        <div id="progress-pct" class="progress-percentage">4%</div>
        <div class="progress-bar-bg">
          <div id="progress-fill" class="progress-bar-fill" style="width: 4%;"></div>
          <img class="progress-head" src="data:image/webp;base64,$PROGRESS_HEAD" alt="" aria-hidden="true" draggable="false">
        </div>
      </div>

      <div id="uv-progress-container" class="uv-progress-container" aria-hidden="true">
        <div class="uv-progress-header">
          <span id="uv-progress-detail" class="uv-progress-detail"></span>
          <span id="uv-progress-pct">0%</span>
        </div>
        <div class="uv-progress-bar-bg">
          <div id="uv-progress-fill" class="uv-progress-bar-fill" style="width: 2%;"></div>
        </div>
      </div>

      <div class="footer-info">
        <div id="tip-text" class="tip-text">$I18N_DEFAULT_TIP</div>
        <div class="footer-right">
          <div id="progress-meta" class="notice-text">$I18N_PROGRESS_META</div>
          <div id="splash-actions" class="splash-actions">
            <button id="splash-log-button" class="splash-log-button" type="button">$I18N_DOWNLOAD_LOG</button>
          </div>
        </div>
      </div>
    </div>
  </div>

  <script>
    const i18n = $I18N_JSON;
    const defaultTip = i18n.defaultTip;
    const invoke =
      (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
      || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
    const webviewDraggableRegionsEnabled = $NATIVE_TOUCH_DRAG;

    window.addEventListener('contextmenu', event => {
      event.preventDefault();
    }, { capture: true });

    function splitSubtitle(value) {
      const text = String(value || '').trim();
      if (!text) {
        return { status: i18n.loading, tip: defaultTip };
      }
      const match = text.match(/^(.*?)\s*\|\s*Tips[:：]\s*(.*)$/);
      if (!match) {
        return { status: text, tip: defaultTip };
      }
      return {
        status: match[1].trim() || i18n.loading,
        tip: match[2].trim() || defaultTip,
      };
    }

    function normalizeDetail(value) {
      const text = String(value || '').trim();
      return text || i18n.webuiInit;
    }

    window.__ALAS_SPLASH_UPDATE = function (payload) {
      const badge = document.getElementById('badge');
      const badgeText = document.getElementById('badge-text');
      const spinner = document.getElementById('spinner');
      const errorDot = document.getElementById('error-dot');
      const progressFill = document.getElementById('progress-fill');
      const progressPct = document.getElementById('progress-pct');
      const uvProgressContainer = document.getElementById('uv-progress-container');
      const uvProgressFill = document.getElementById('uv-progress-fill');
      const uvProgressPct = document.getElementById('uv-progress-pct');
      const uvProgressDetail = document.getElementById('uv-progress-detail');
      const progressMeta = document.getElementById('progress-meta');
      const splashActions = document.getElementById('splash-actions');
      const subtitle = splitSubtitle(payload.subtitle);

      badgeText.textContent = payload.is_error ? i18n.errorBadge : subtitle.status;
      document.getElementById('tip-text').textContent = subtitle.tip;
      document.getElementById('title').textContent = payload.title || i18n.starting;
      document.getElementById('detail').textContent = normalizeDetail(payload.detail);
      progressMeta.textContent = payload.is_error
        ? i18n.initStopped
        : i18n.progressMetaReady;

      const progress = Math.max(0, Math.min(100, Number(payload.progress || 0)));
      progressFill.style.width = progress + '%';
      progressFill.parentElement.style.setProperty('--progress', progress + '%');
      progressPct.textContent = progress + '%';

      const uvState = payload.uv_progress;
      const hasUvProgress = !payload.is_error
        && uvState
        && Number.isFinite(Number(uvState.progress));
      uvProgressContainer.classList.toggle('is-visible', Boolean(hasUvProgress));
      uvProgressContainer.setAttribute('aria-hidden', String(!hasUvProgress));
      if (hasUvProgress) {
        const uvProgress = Math.max(0, Math.min(99, Number(uvState.progress)));
        uvProgressFill.style.width = uvProgress + '%';
        uvProgressPct.textContent = uvProgress + '%';
        uvProgressDetail.textContent = String(uvState.detail || '');
      }

      if (payload.is_error) {
        document.body.classList.add('error-state');
        badge.className = 'status-badge status-badge-err';
        spinner.style.display = 'none';
        errorDot.style.display = 'flex';
        progressFill.className = 'progress-bar-fill progress-bar-fill-error';
        splashActions.className = 'splash-actions splash-actions-err';
      } else {
        document.body.classList.remove('error-state');
        badge.className = 'status-badge';
        spinner.style.display = 'block';
        errorDot.style.display = 'none';
        progressFill.className = 'progress-bar-fill';
        splashActions.className = 'splash-actions';
      }
    };

    const splashDragRegion = document.getElementById('splash-drag-region');
    splashDragRegion.addEventListener('pointerdown', event => {
      if (!event.isPrimary || event.button !== 0 || event.target.closest('button')) {
        return;
      }
      if (webviewDraggableRegionsEnabled) {
        return;
      }
      event.preventDefault();
      if (typeof invoke !== 'function') {
        return;
      }
      invoke('window_start_dragging').catch(error => {
        console.error('Failed to drag splash window', error);
      });
    });

    document.getElementById('window-minimize').addEventListener('click', event => {
      event.stopPropagation();
      if (typeof invoke === 'function') {
        invoke('window_minimize').catch(error => {
          console.error('Failed to minimize splash window', error);
        });
      }
    });

    document.getElementById('window-close').addEventListener('click', event => {
      event.stopPropagation();
      if (typeof invoke === 'function') {
        invoke('window_close').catch(error => {
          console.error('Failed to close splash window', error);
        });
      }
    });

    document.getElementById('splash-log-button').addEventListener('click', async () => {
      const button = document.getElementById('splash-log-button');
      const progressMeta = document.getElementById('progress-meta');
      button.disabled = true;
      progressMeta.textContent = i18n.preparingLog;
      try {
        if (typeof invoke !== 'function') {
          throw new Error('Tauri invoke is unavailable');
        }
        const filename = await invoke('download_today_launcher_log');
        progressMeta.textContent = i18n.logSavedPrefix + filename;
      } catch (error) {
        progressMeta.textContent = i18n.logFailed + (error && error.message ? error.message : error);
      } finally {
        button.disabled = false;
      }
    });

    window.__ALAS_SPLASH_READY = true;
  </script>
</body>
</html>"#
    // 占位符替换：$PROGRESS_HEAD/$VIDEO_BG/$MI_SANS_FONT 为 base64 资源，
    // $LAUNCHER_VERSION 版本号，$I18N_* 为本地化文案（已 HTML 转义），
    // $NATIVE_TOUCH_DRAG 仅 Windows 为 true（WebView2 原生拖拽区域开关）。
    .replace("$PROGRESS_HEAD", &BASE64_STANDARD.encode(SPLASH_PROGRESS_HEAD))
    .replace("$VIDEO_BG", video_bg_b64)
    .replace("$MI_SANS_FONT", mi_sans_font_b64)
    .replace("$LAUNCHER_VERSION", env!("CARGO_PKG_VERSION"))
    .replace("$I18N_JSON", &i18n_json)
    .replace("$NATIVE_TOUCH_DRAG", if cfg!(windows) { "true" } else { "false" })
    .replace("$I18N_INITIALIZING", &escape_html(t!("splash.initializing")))
    .replace("$I18N_MINIMIZE", &escape_html(t!("titlebar.minimize")))
    .replace("$I18N_CLOSE", &escape_html(t!("titlebar.close")))
    .replace("$I18N_STARTING", &escape_html(t!("splash.starting")))
    .replace("$I18N_WEBUI_INIT", &escape_html(t!("splash.webui_init")))
    .replace("$I18N_DEFAULT_TIP", &escape_html(t!("tips.17")))
    .replace("$I18N_PROGRESS_META", &escape_html(t!("splash.progress_meta_ready")))
    .replace("$I18N_DOWNLOAD_LOG", &escape_html(t!("splash.download_log")))
}

/// 按 tauri.conf.json 中 label 为 main 的配置创建主窗口。
///
/// 挂载导航拦截（handle_backend_navigation）与页面加载注入
/// （page_load_injector）。
///
/// # Errors
/// 配置缺失或窗口构建失败时返回 Err。
fn create_main_window(app: &tauri::AppHandle, port: u16) -> Result<WebviewWindow> {
    let main_config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .ok_or_else(|| anyhow!(t!("errors.main_window_missing")))?;

    let app_for_navigation = app.clone();
    let main_window = tauri::WebviewWindowBuilder::from_config(app, main_config)?
        .on_navigation(move |url| handle_backend_navigation(app_for_navigation.clone(), port, url))
        .on_page_load(page_load_injector)
        .build()?;
    main_window.set_resizable(true)?;

    // Windows/Linux：主窗口同样去掉原生装饰（无边框，配合注入的标题栏）；
    // splash 则已在 tauri.conf.json 中配置为无边框。
    #[cfg(not(target_os = "macos"))]
    {
        main_window.set_decorations(false)?;
    }

    Ok(main_window)
}

/// 显示窗口并聚焦；若窗口处于最小化状态先还原。
///
/// # Errors
/// 任一窗口操作失败时返回 Err。
fn reveal_window(window: &WebviewWindow) -> tauri::Result<()> {
    if window.is_minimized()? {
        window.unminimize()?;
    }
    window.show()?;
    window.set_focus()?;
    Ok(())
}

/// 把主窗口最小化到托盘（按平台采取不同策略）。
///
/// 平台差异：Windows 上直接销毁窗口以释放 WebView 资源（恢复时重建）；
/// macOS/Linux 仅隐藏窗口；macOS 额外切换到 Accessory 激活策略以隐藏
/// Dock 图标。
fn minimize_main_window_to_tray(app: &tauri::AppHandle) {
    #[cfg(windows)]
    {
        if let Some(window) = app.get_webview_window("main") {
            info!("Destroying main window to release WebView resources while trayed");
            if let Err(e) = window.destroy() {
                warn!("Failed to destroy main window for tray mode: {:?}", e);
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.hide();
        }
    }

    #[cfg(target_os = "macos")]
    {
        set_macos_activation_policy(app, false);
    }
}

/// 任意线程安全地调度主窗口恢复：包装 run_on_main_thread，调度失败仅告警。
fn restore_main_window_from_any_thread(
    app: tauri::AppHandle,
    port: u16,
    recreating_main_window: Arc<AtomicBool>,
) {
    let app_for_restore = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        restore_main_window_from_tray(&app_for_restore, port, recreating_main_window);
    }) {
        warn!("Failed to schedule main window restore: {:?}", e);
    }
}

/// 从托盘恢复主窗口：窗口仍存在则直接显示并聚焦；已被销毁（Windows 托盘
/// 化会销毁窗口）则以防重入标志保护，在新线程中重建并导航。
fn restore_main_window_from_tray(
    app: &tauri::AppHandle,
    port: u16,
    recreating_main_window: Arc<AtomicBool>,
) {
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(target_os = "macos")]
        set_macos_activation_policy(app, true);
        let _ = reveal_window(&window);
        return;
    }

    // 防重入：托盘、单实例、通知点击可能并发触发，只允许一次重建。
    if recreating_main_window
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        debug!("Main window recreation already in progress");
        return;
    }

    // 重建在新线程执行：窗口构建与导航可能耗时，不能阻塞调用线程。
    let app_handle = app.clone();
    thread::spawn(move || {
        // 重建期间同样先把 macOS 切回 Regular，保证 Dock 图标即时出现。
        #[cfg(target_os = "macos")]
        set_macos_activation_policy(&app_handle, true);

        let result = (|| -> Result<()> {
            let window = create_main_window(&app_handle, port)?;
            navigate_backend_or_error(&window, port)?;
            reveal_window(&window)?;
            Ok(())
        })();

        // 无论成败都复位重建标志，允许后续再次触发。
        recreating_main_window.store(false, Ordering::SeqCst);

        if let Err(e) = result {
            error!("Failed to recreate main window from tray: {:?}", e);
        }
    });
}

/// 托盘"显示/隐藏"菜单与左键点击的入口：按当前窗口状态在托盘化与恢复
/// 之间切换；窗口不存在时走恢复（重建）路径。
fn toggle_main_window_visibility(
    app: &tauri::AppHandle,
    port: u16,
    recreating_main_window: Arc<AtomicBool>,
) {
    if let Some(window) = app.get_webview_window("main") {
        let is_visible = window.is_visible().unwrap_or(false);
        let is_minimized = window.is_minimized().unwrap_or(false);
        if is_visible && !is_minimized {
            minimize_main_window_to_tray(app);
        } else {
            restore_main_window_from_tray(app, port, recreating_main_window);
        }
    } else {
        restore_main_window_from_tray(app, port, recreating_main_window);
    }
}

/// 生成注入主窗口的自定义标题栏 JS 脚本。
///
/// 脚本职责：在页面顶部叠加拖拽区，右上角放置"最小化/最大化/关闭"按钮
/// 胶囊（样式随页面深色主题自动切换）；关闭按钮在 Windows 上打开窗口内
/// "退出 / 最小化到托盘"选择菜单，其它平台直接调用 window_close；双击
/// 标题栏切换最大化。按钮动作全部经 Tauri 命令完成。脚本还会定位页面
/// 已有的 topbar/header 元素同步高度与偏移，并监听主题与窗口尺寸变化。
///
/// 平台差异：macOS 返回空脚本——系统自带红绿灯按钮，无需自绘标题栏。
fn main_window_titlebar_injection_script() -> String {
    // macOS：系统自带红绿灯按钮，无需注入自绘标题栏。
    #[cfg(target_os = "macos")]
    {
        String::new()
    }
    #[cfg(not(target_os = "macos"))]
    {
        // 标题栏文案以 JSON 注入，避免字符串拼接时的转义问题。
        let i18n = serde_json::json!({
            "hideLabel": t!("titlebar.minimize_to_tray"),
            "minimizeLabel": t!("titlebar.minimize_window"),
            "minimizeTitle": t!("titlebar.minimize"),
            "maximizeLabel": t!("titlebar.maximize_restore_window"),
            "maximizeTitle": t!("titlebar.maximize"),
            "closeLabel": t!("titlebar.close_window"),
            "closeTitle": t!("titlebar.close"),
            "restoreTitle": t!("titlebar.restore"),
            "maximizeActionTitle": t!("titlebar.maximize_action"),
            "restoreLabel": t!("titlebar.restore_window"),
            "maximizeLabelText": t!("titlebar.maximize_window"),
            "closePrompt": t!("dialog.confirm_exit"),
            "exitAction": t!("dialog.exit"),
            "minimizeToTrayAction": t!("dialog.minimize_to_tray"),
        });
        let i18n_json = serde_json::to_string(&i18n).unwrap();
        let mut s = String::with_capacity(8192);
        s.push_str("const i18n = ");
        s.push_str(&i18n_json);
        // 关闭确认菜单仅 Windows 启用；其余平台保持单击直接关闭的行为。
        s.push_str(if cfg!(windows) {
            ";const closePromptEnabled = true;"
        } else {
            ";const closePromptEnabled = false;"
        });
        // 以下为原样注入的 JS 主体：创建标题栏 DOM 与样式、绑定拖拽和按钮
        // 事件、同步页面主题与布局。
        s.push_str(r#";
        const invoke =
            (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
            || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
        if (typeof invoke !== 'function') {
            return;
        }
        const ensureTitlebar = () => {
            if (!document.body || document.getElementById('alas-launcher-titlebar')) {
                return;
            }
            if (!document.getElementById('alas-launcher-titlebar-style')) {
                const style = document.createElement('style');
                style.id = 'alas-launcher-titlebar-style';
                style.textContent = ':root{--alas-titlebar-height:56px}#alas-launcher-titlebar{position:fixed;top:0;left:0;right:0;height:var(--topbar-height,var(--alas-titlebar-height,56px));z-index:2147483647;user-select:none;pointer-events:none;background:transparent}#alas-launcher-titlebar *{box-sizing:border-box}.alas-titlebar-drag-zone{position:absolute;inset:0 148px 0 0;height:100%;pointer-events:none;background:transparent;touch-action:none}.header-icon,.header-icon *{app-region:no-drag;-webkit-app-region:no-drag}.header-icon{display:flex;align-items:center;gap:3px;padding:3px 6px;position:absolute;top:50%;transform:translateY(-50%);right:10px;height:36px;pointer-events:auto;border:1px solid rgba(255,255,255,.92);border-radius:18px;background:rgba(250,250,247,.78);box-shadow:0 4px 14px rgba(61,79,97,.1),inset 0 1px 0 rgba(255,255,255,.36);backdrop-filter:blur(16px) saturate(1.2);-webkit-backdrop-filter:blur(16px) saturate(1.2);transition:background .2s ease,border-color .2s ease,box-shadow .2s ease}.icon{width:28px;height:28px;min-width:28px;min-height:28px;margin:0;padding:0;line-height:1;border-radius:12px;border:none;background:transparent;color:#727b86;cursor:pointer;flex:0 0 auto;position:relative;transition:transform 140ms cubic-bezier(.23,1,.32,1),background-color 140ms ease,color 140ms ease;display:inline-flex;align-items:center;justify-content:center}.icon:hover{color:#202832;background:rgba(255,255,255,.72)}.icon:active{transform:scale(.96)}.icon-close{color:#e64f58}.icon-close:hover{color:#b5202e;background:rgba(244,91,91,.15)}.icon svg{width:11px;height:11px;stroke:currentColor;fill:none;stroke-width:1.2;stroke-linecap:round;stroke-linejoin:round;opacity:1;display:block;margin:auto}';
                style.textContent += '#alas-close-menu{position:fixed;top:8px;right:10px;z-index:2147483647;width:272px;padding:14px;border:1px solid rgba(255,255,255,.9);border-radius:20px;background:rgba(250,250,247,.94);box-shadow:0 18px 46px rgba(46,58,72,.2),inset 0 1px 0 rgba(255,255,255,.7);backdrop-filter:blur(20px) saturate(1.18);-webkit-backdrop-filter:blur(20px) saturate(1.18);color:#202832;opacity:0;pointer-events:none;transform:translateY(-6px) scale(.96);transform-origin:calc(100% - 64px) 0;transition:opacity 140ms ease,transform 180ms cubic-bezier(.23,1,.32,1),background .2s ease,border-color .2s ease;app-region:no-drag;-webkit-app-region:no-drag}#alas-close-menu.is-open{opacity:1;pointer-events:auto;transform:translateY(0) scale(1)}#alas-close-menu *{box-sizing:border-box;app-region:no-drag;-webkit-app-region:no-drag}#alas-close-menu-title{margin:0 0 12px;font:500 13px/1.55 "MiSans",sans-serif;color:rgba(32,40,50,.82)}#alas-close-menu-actions{display:grid;grid-template-columns:1fr 1fr;gap:8px}#alas-close-menu button{display:flex;align-items:center;justify-content:center;min-width:0;min-height:36px;margin:0;padding:0 12px;border:1px solid rgba(92,105,120,.16);border-radius:11px;background:rgba(105,118,133,.08);color:#394451;font:600 12px/1 "MiSans",sans-serif;cursor:pointer;transition:background-color 140ms ease,color 140ms ease,transform 140ms cubic-bezier(.23,1,.32,1)}#alas-close-menu button:hover{background:rgba(105,118,133,.14)}#alas-close-menu button:active{transform:scale(.98)}#alas-close-menu button:disabled{opacity:.55;cursor:default;transform:none}#alas-close-menu .alas-close-confirm{border-color:rgba(205,62,69,.28);background:#d94b4b;color:#fff}#alas-close-menu .alas-close-confirm:hover{background:#c93e3e;color:#fff}';
                style.textContent += ':root[data-theme*="dark"] .header-icon,:root[data-color-mode="dark"] .header-icon,:root.dark .header-icon,body[data-theme*="dark"] .header-icon,body[data-color-mode="dark"] .header-icon,body.dark .header-icon,[data-theme*="dark"] .header-icon,[data-color-mode="dark"] .header-icon,#alas-launcher-titlebar.is-dark .header-icon{background:rgba(30,34,42,.78);border:1px solid rgba(255,255,255,.14);box-shadow:0 4px 16px rgba(0,0,0,.4),inset 0 1px 0 rgba(255,255,255,.14)}';
                style.textContent += ':root[data-theme*="dark"] .icon,:root[data-color-mode="dark"] .icon,:root.dark .icon,body[data-theme*="dark"] .icon,body[data-color-mode="dark"] .icon,body.dark .icon,[data-theme*="dark"] .icon,[data-color-mode="dark"] .icon,#alas-launcher-titlebar.is-dark .icon{color:rgba(220,228,238,.82)}';
                style.textContent += ':root[data-theme*="dark"] .icon:hover,:root[data-color-mode="dark"] .icon:hover,:root.dark .icon:hover,body[data-theme*="dark"] .icon:hover,body[data-color-mode="dark"] .icon:hover,body.dark .icon:hover,[data-theme*="dark"] .icon:hover,[data-color-mode="dark"] .icon:hover,#alas-launcher-titlebar.is-dark .icon:hover{color:#fff;background:rgba(255,255,255,.14)}';
                style.textContent += ':root[data-theme*="dark"] .icon:active,:root[data-color-mode="dark"] .icon:active,:root.dark .icon:active,body[data-theme*="dark"] .icon:active,body[data-color-mode="dark"] .icon:active,body.dark .icon:active,[data-theme*="dark"] .icon:active,[data-color-mode="dark"] .icon:active,#alas-launcher-titlebar.is-dark .icon:active{background:rgba(255,255,255,.22)}';
                style.textContent += ':root[data-theme*="dark"] .icon-close,:root[data-color-mode="dark"] .icon-close,:root.dark .icon-close,body[data-theme*="dark"] .icon-close,body[data-color-mode="dark"] .icon-close,body.dark .icon-close,[data-theme*="dark"] .icon-close,[data-color-mode="dark"] .icon-close,#alas-launcher-titlebar.is-dark .icon-close{color:#ff6470}';
                style.textContent += ':root[data-theme*="dark"] .icon-close:hover,:root[data-color-mode="dark"] .icon-close:hover,:root.dark .icon-close:hover,body[data-theme*="dark"] .icon-close:hover,body[data-color-mode="dark"] .icon-close:hover,body.dark .icon-close:hover,[data-theme*="dark"] .icon-close:hover,[data-color-mode="dark"] .icon-close:hover,#alas-launcher-titlebar.is-dark .icon-close:hover{color:#fff;background:#e0444d}';
                style.textContent += ':root[data-theme*="dark"] .icon-close:active,:root[data-color-mode="dark"] .icon-close:active,:root.dark .icon-close:active,body[data-theme*="dark"] .icon-close:active,body[data-color-mode="dark"] .icon-close:active,body.dark .icon-close:active,[data-theme*="dark"] .icon-close:active,[data-color-mode="dark"] .icon-close:active,#alas-launcher-titlebar.is-dark .icon-close:active{background:#c7363f}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu,:root[data-color-mode="dark"] #alas-close-menu,:root.dark #alas-close-menu,body[data-theme*="dark"] #alas-close-menu,body[data-color-mode="dark"] #alas-close-menu,body.dark #alas-close-menu,[data-theme*="dark"] #alas-close-menu,[data-color-mode="dark"] #alas-close-menu,#alas-close-menu.is-dark{background:rgba(28,32,38,.94);border:1px solid rgba(255,255,255,.14);box-shadow:0 18px 46px rgba(0,0,0,.55),inset 0 1px 0 rgba(255,255,255,.12);color:#f0f3f6}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu-title,:root[data-color-mode="dark"] #alas-close-menu-title,:root.dark #alas-close-menu-title,body[data-theme*="dark"] #alas-close-menu-title,body[data-color-mode="dark"] #alas-close-menu-title,body.dark #alas-close-menu-title,[data-theme*="dark"] #alas-close-menu-title,[data-color-mode="dark"] #alas-close-menu-title,#alas-close-menu.is-dark #alas-close-menu-title{color:rgba(240,244,250,.9)}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu button,:root[data-color-mode="dark"] #alas-close-menu button,:root.dark #alas-close-menu button,body[data-theme*="dark"] #alas-close-menu button,body[data-color-mode="dark"] #alas-close-menu button,body.dark #alas-close-menu button,[data-theme*="dark"] #alas-close-menu button,[data-color-mode="dark"] #alas-close-menu button,#alas-close-menu.is-dark button{border:1px solid rgba(255,255,255,.12);background:rgba(255,255,255,.08);color:rgba(230,235,245,.88)}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu button:hover,:root[data-color-mode="dark"] #alas-close-menu button:hover,:root.dark #alas-close-menu button:hover,body[data-theme*="dark"] #alas-close-menu button:hover,body[data-color-mode="dark"] #alas-close-menu button:hover,body.dark #alas-close-menu button:hover,[data-theme*="dark"] #alas-close-menu button:hover,[data-color-mode="dark"] #alas-close-menu button:hover,#alas-close-menu.is-dark button:hover{background:rgba(255,255,255,.14);color:#fff}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu .alas-close-confirm,:root[data-color-mode="dark"] #alas-close-menu .alas-close-confirm,:root.dark #alas-close-menu .alas-close-confirm,body[data-theme*="dark"] #alas-close-menu .alas-close-confirm,body[data-color-mode="dark"] #alas-close-menu .alas-close-confirm,body.dark #alas-close-menu .alas-close-confirm,[data-theme*="dark"] #alas-close-menu .alas-close-confirm,[data-color-mode="dark"] #alas-close-menu .alas-close-confirm,#alas-close-menu.is-dark .alas-close-confirm{border-color:rgba(244,91,91,.35);background:#d94b4b;color:#fff}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu .alas-close-confirm:hover,:root[data-color-mode="dark"] #alas-close-menu .alas-close-confirm:hover,:root.dark #alas-close-menu .alas-close-confirm:hover,body[data-theme*="dark"] #alas-close-menu .alas-close-confirm:hover,body[data-color-mode="dark"] #alas-close-menu .alas-close-confirm:hover,body.dark #alas-close-menu .alas-close-confirm:hover,[data-theme*="dark"] #alas-close-menu .alas-close-confirm:hover,[data-color-mode="dark"] #alas-close-menu .alas-close-confirm:hover,#alas-close-menu.is-dark .alas-close-confirm:hover{background:#c93e3e;color:#fff}';
                style.textContent += '@media (prefers-color-scheme:dark){:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .header-icon{background:rgba(30,34,42,.78);border:1px solid rgba(255,255,255,.14);box-shadow:0 4px 16px rgba(0,0,0,.4),inset 0 1px 0 rgba(255,255,255,.14)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon{color:rgba(220,228,238,.82)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon:hover{color:#fff;background:rgba(255,255,255,.14)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon:active{background:rgba(255,255,255,.22)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon-close{color:#ff6470}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon-close:hover{color:#fff;background:#e0444d}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon-close:active{background:#c7363f}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu{background:rgba(28,32,38,.94);border:1px solid rgba(255,255,255,.14);box-shadow:0 18px 46px rgba(0,0,0,.55),inset 0 1px 0 rgba(255,255,255,.12);color:#f0f3f6}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu-title{color:rgba(240,244,250,.9)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu button{border:1px solid rgba(255,255,255,.12);background:rgba(255,255,255,.08);color:rgba(230,235,245,.88)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu button:hover{background:rgba(255,255,255,.14);color:#fff}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu .alas-close-confirm{border-color:rgba(244,91,91,.35);background:#d94b4b;color:#fff}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu .alas-close-confirm:hover{background:#c93e3e;color:#fff}}';
                document.head.appendChild(style);
            }
            const titlebar = document.createElement('div');
            titlebar.id = 'alas-launcher-titlebar';
            titlebar.innerHTML = '<div class="alas-titlebar-drag-zone" aria-hidden="true"></div><div class="header-icon"><button type="button" class="icon icon-hide" data-action="hide" aria-label="'+i18n.hideLabel+'" title="'+i18n.hideLabel+'"><svg viewBox="0 0 6 6"><rect x="1" y="1" width="4" height="4" rx="1"/><path d="M2 3h2"/></svg></button><button type="button" class="icon icon-minimize" data-action="minimize" aria-label="'+i18n.minimizeLabel+'" title="'+i18n.minimizeTitle+'"><svg viewBox="0 0 6 6"><line x1="1" y1="3" x2="5" y2="3"/></svg></button><button type="button" class="icon icon-maximize" data-action="maximize" aria-label="'+i18n.maximizeLabel+'" title="'+i18n.maximizeTitle+'"><svg viewBox="0 0 6 6" class="svg-restore" style="display:none"><polyline points="1,3 1,1 3,1"/><polyline points="3,5 5,5 5,3"/></svg><svg viewBox="0 0 6 6" class="svg-maximize"><polyline points="1,2.5 1,1 2.5,1"/><polyline points="3.5,5 5,5 5,3.5"/></svg></button><button type="button" class="icon icon-close" data-action="close" aria-label="'+i18n.closeLabel+'" title="'+i18n.closeTitle+'"><svg viewBox="0 0 6 6"><line x1="1" y1="1" x2="5" y2="5"/><line x1="5" y1="1" x2="1" y2="5"/></svg></button></div>';
            document.body.dataset.alasCustomTitlebar = 'true';
            document.body.prepend(titlebar);
            const maximizeButton = titlebar.querySelector('[data-action="maximize"]');
            let closeMenu = document.getElementById('alas-close-menu');
            if (!closeMenu) {
                closeMenu = document.createElement('div');
                closeMenu.id = 'alas-close-menu';
                closeMenu.setAttribute('role', 'dialog');
                closeMenu.setAttribute('aria-modal', 'false');
                closeMenu.innerHTML = '<p id="alas-close-menu-title"></p><div id="alas-close-menu-actions"><button type="button" data-close-action="minimize"></button><button type="button" class="alas-close-confirm" data-close-action="exit"></button></div>';
                closeMenu.querySelector('#alas-close-menu-title').textContent = i18n.closePrompt;
                closeMenu.querySelector('[data-close-action="minimize"]').textContent = i18n.minimizeToTrayAction;
                closeMenu.querySelector('[data-close-action="exit"]').textContent = i18n.exitAction;
                closeMenu.addEventListener('pointerdown', event => event.stopPropagation());
                document.body.appendChild(closeMenu);
            }
            const syncTitlebarLayout = () => {
                try {
                    const topbar = document.querySelector('.topbar, header, #topbar, [class*="topbar"], [class*="Header"]');
                    let h = 0;
                    let topOffset = 0;
                    if (topbar) {
                        const rect = topbar.getBoundingClientRect();
                        if (rect.height > 20) {
                            h = rect.height;
                            topOffset = rect.top;
                        }
                    }
                    if (!h) {
                        const computed = getComputedStyle(document.documentElement).getPropertyValue('--topbar-height').trim();
                        if (computed) {
                            const parsed = parseFloat(computed);
                            if (!isNaN(parsed) && parsed > 20) {
                                h = parsed;
                            }
                        }
                    }
                    if (h > 0) {
                        titlebar.style.height = h + 'px';
                        if (topOffset >= 0) {
                            titlebar.style.top = topOffset + 'px';
                        }
                    }
                } catch (_) {}
            };
            const syncTheme = () => {
                try {
                    const doc = document.documentElement;
                    const body = document.body;
                    let isDark = false;
                    const dt = (doc.getAttribute('data-theme') || (body && body.getAttribute('data-theme')) || '').toLowerCase();
                    const dcm = (doc.getAttribute('data-color-mode') || (body && body.getAttribute('data-color-mode')) || '').toLowerCase();
                    if (dt.includes('dark') || dcm === 'dark') {
                        isDark = true;
                    } else if (dt.includes('light') || dcm === 'light') {
                        isDark = false;
                    } else if (doc.classList.contains('dark') || (body && body.classList.contains('dark'))) {
                        isDark = true;
                    } else if (doc.classList.contains('light') || (body && body.classList.contains('light'))) {
                        isDark = false;
                    } else {
                        try {
                            const st = (localStorage.getItem('azurpilot.theme') || localStorage.getItem('theme') || '').toLowerCase();
                            const sm = (localStorage.getItem('azurpilot.color-mode') || '').toLowerCase();
                            if (st.includes('dark') || sm === 'dark') isDark = true;
                            else if (st.includes('light') || sm === 'light') isDark = false;
                        } catch (_) {}
                        if (!isDark && window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches) {
                            isDark = true;
                        }
                    }
                    titlebar.classList.toggle('is-dark', isDark);
                    if (closeMenu) closeMenu.classList.toggle('is-dark', isDark);
                } catch (e) {
                    console.error('Failed to sync titlebar theme', e);
                }
            };
            const setCloseMenuOpen = open => {
                if (open) {
                    syncTheme();
                    syncTitlebarLayout();
                    try {
                        const iconRect = titlebar.querySelector('.header-icon').getBoundingClientRect();
                        closeMenu.style.top = Math.round(iconRect.bottom + 8) + 'px';
                        closeMenu.style.right = Math.max(10, Math.round(window.innerWidth - iconRect.right)) + 'px';
                    } catch (_) {}
                }
                closeMenu.classList.toggle('is-open', open);
                if (open) closeMenu.querySelector('[data-close-action="minimize"]').focus({ preventScroll: true });
            };
            const showClosePrompt = () => {
                if (!closePromptEnabled) {
                    invoke('window_close').catch(error => console.error('Failed to close window', error));
                    return;
                }
                setCloseMenuOpen(true);
            };
            window.__ALAS_OPEN_CLOSE_PROMPT = showClosePrompt;
            closeMenu.querySelector('[data-close-action="minimize"]').addEventListener('click', async () => {
                setCloseMenuOpen(false);
                try { await invoke('window_hide'); }
                catch (error) { console.error('Failed to minimize window to tray', error); }
            });
            closeMenu.querySelector('[data-close-action="exit"]').addEventListener('click', async () => {
                closeMenu.querySelectorAll('button').forEach(button => { button.disabled = true; });
                try { await invoke('window_exit_application'); }
                catch (error) {
                    closeMenu.querySelectorAll('button').forEach(button => { button.disabled = false; });
                    console.error('Failed to exit application', error);
                }
            });
            document.addEventListener('pointerdown', event => {
                if (closeMenu.classList.contains('is-open') && !closeMenu.contains(event.target)) setCloseMenuOpen(false);
            });
            document.addEventListener('keydown', event => {
                if (event.key === 'Escape' && closeMenu.classList.contains('is-open')) setCloseMenuOpen(false);
            });
            const interactiveSelector = 'a[href],button,input,select,textarea,summary,label[for],[role="button"],[role="link"],[contenteditable="true"],[tabindex]:not([tabindex="-1"]),[onclick]';
            const isInteractiveElement = element => {
                if (!element || !(element instanceof Element)) return false;
                return Boolean(
                    element.closest(interactiveSelector)
                    || element.closest('[data-alas-no-drag]')
                    || (getComputedStyle(element).cursor === 'pointer' && !element.closest('#alas-launcher-titlebar'))
                );
            };

            document.addEventListener('pointerdown', event => {
                syncTheme();
                syncTitlebarLayout();
                if (!event.isPrimary || event.button !== 0) return;
                const tbRect = titlebar.getBoundingClientRect();
                const titlebarHeight = tbRect.height || 56;
                const titlebarTop = tbRect.top || 0;
                const reservedRight = 148;
                if (event.clientY < titlebarTop || event.clientY > titlebarTop + titlebarHeight || event.clientX > window.innerWidth - reservedRight) return;
                const target = event.target;
                if (!target || !(target instanceof Element)) return;
                if (titlebar.contains(target) || target.closest('#alas-close-menu')) return;
                if (isInteractiveElement(target)) return;
                event.preventDefault();
                invoke('window_start_dragging').catch(error => {
                    console.error('Failed to start dragging from titlebar', error);
                });
            }, { capture: true });

            document.addEventListener('dblclick', async event => {
                syncTheme();
                syncTitlebarLayout();
                const tbRect = titlebar.getBoundingClientRect();
                const titlebarHeight = tbRect.height || 56;
                const titlebarTop = tbRect.top || 0;
                const reservedRight = 148;
                if (event.clientY < titlebarTop || event.clientY > titlebarTop + titlebarHeight || event.clientX > window.innerWidth - reservedRight) return;
                const target = event.target;
                if (!target || !(target instanceof Element)) return;
                if (titlebar.contains(target) || target.closest('#alas-close-menu')) return;
                if (isInteractiveElement(target)) return;
                try {
                    await invoke('window_toggle_maximize');
                    await syncMaximizeState();
                } catch (error) {
                    console.error('Failed to toggle maximize from titlebar', error);
                }
            }, { capture: true });

            const syncMaximizeState = async () => {
                if (!maximizeButton) return;
                try {
                    const maximized = await invoke('window_is_maximized');
                    maximizeButton.dataset.maximized = maximized ? 'true' : 'false';
                    maximizeButton.title = maximized ? i18n.restoreTitle : i18n.maximizeActionTitle;
                    maximizeButton.setAttribute('aria-label', maximized ? i18n.restoreLabel : i18n.maximizeLabelText);
                    maximizeButton.querySelector('.svg-maximize').style.display = maximized ? 'none' : '';
                    maximizeButton.querySelector('.svg-restore').style.display = maximized ? '' : 'none';
                } catch (e) {
                    console.error('Failed to sync maximize state', e);
                }
            };
            titlebar.querySelectorAll('button[data-action]').forEach(button => {
                button.addEventListener('click', async event => {
                    event.stopPropagation();
                    syncTheme();
                    try {
                        switch (button.dataset.action) {
                            case 'hide': await invoke('window_hide'); break;
                            case 'minimize': await invoke('window_minimize'); break;
                            case 'maximize': await invoke('window_toggle_maximize'); await syncMaximizeState(); break;
                            case 'close': showClosePrompt(); break;
                        }
                    } catch (error) {
                        console.error('Failed to handle ' + button.dataset.action + ' window action', error);
                    }
                });
            });
            window.addEventListener('resize', () => { syncTitlebarLayout(); void syncMaximizeState(); syncTheme(); });
            if (window.matchMedia) {
                try {
                    window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', syncTheme);
                } catch (_) {}
            }
            void syncMaximizeState();
            syncTheme();
            syncTitlebarLayout();
            setTimeout(syncTitlebarLayout, 100);
            setTimeout(syncTitlebarLayout, 500);
        };
        ensureTitlebar();
        if (!document.body) {
            window.addEventListener('DOMContentLoaded', ensureTitlebar, { once: true });
        }
        "#);
        s
    }
}
