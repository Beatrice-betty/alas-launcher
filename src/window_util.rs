//! 子进程窗口控制工具：在 Windows 上阻止子进程弹出控制台黑窗。
//! 通过扩展 trait [`CreateNoWindow`] 为 `std::process::Command` 与
//! command-group 的 `CommandGroupBuilder` 统一附加 `CREATE_NO_WINDOW`
//! 创建标志；非 Windows 平台没有控制台窗口的概念，trait 提供空实现，
//! 使调用方（backend.rs 拉起 gui.py、netstat、lsof 等）无需条件编译。
#[cfg(windows)]
use command_group::builder::CommandGroupBuilder;
#[cfg(windows)]
use std::process::Command;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
/// 当前进程是否已附加到父控制台（由 main.rs 在入口处初始化）。
///
/// 为 `true` 说明启动器是从终端启动的调试场景：此时保留子进程的控制台
/// 继承，方便直接查看 gui.py 的输出；为 `false`（图形界面启动）时才给
/// 子进程附加 `CREATE_NO_WINDOW`，避免每个子进程弹出黑色控制台窗口。
#[cfg(windows)]
pub static HAS_CONSOLE: AtomicBool = AtomicBool::new(false);

/// 为可创建子进程的类型附加"不创建控制台窗口"的语义。
///
/// `std::process::Command` 与 command-group 的 `CommandGroupBuilder`
/// 各有一份 Windows 实现；Unix 上的空白实现保证调用方 API 一致。
pub trait CreateNoWindow {
    /// 按当前平台与控制台状态配置创建标志，返回自身以便链式调用。
    fn create_no_window(&mut self) -> &mut Self;
}

/// 为单个子进程命令附加 `CREATE_NO_WINDOW`。
#[cfg(windows)]
impl CreateNoWindow for Command {
    fn create_no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        use winapi::um::winbase::CREATE_NO_WINDOW;
        // 仅图形环境需要隐藏窗口；从终端启动（HAS_CONSOLE 为 true）时
        // 保留控制台，便于调试时直接观察子进程输出。
        if !HAS_CONSOLE.load(Ordering::Relaxed) {
            self.creation_flags(CREATE_NO_WINDOW)
        } else {
            self
        }
    }
}

/// 为进程组构建器附加 `CREATE_NO_WINDOW`。
///
/// 进程组同样接受创建标志，保证 gui.py 及其派生的整组进程都不弹窗。
#[cfg(windows)]
impl<T> CreateNoWindow for CommandGroupBuilder<'_, T> {
    fn create_no_window(&mut self) -> &mut Self {
        use winapi::um::winbase::CREATE_NO_WINDOW;
        // 判断逻辑与 Command 版本一致：调试场景保留控制台可见性。
        if !HAS_CONSOLE.load(Ordering::Relaxed) {
            self.creation_flags(CREATE_NO_WINDOW)
        } else {
            self
        }
    }
}

/// 非 Windows 平台的空实现：Unix/macOS 没有控制台窗口概念，原样返回。
#[cfg(not(windows))]
impl<T> CreateNoWindow for T {
    fn create_no_window(&mut self) -> &mut Self {
        self
    }
}
