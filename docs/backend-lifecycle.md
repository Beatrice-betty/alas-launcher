# 后端进程生命周期

`ManagedBackend` 管理 `gui.py` 及其工作进程，退出启动器时应保留已经启动的模拟器。
模拟器显式关闭、定时重启和故障恢复仍由 ALAS 的相应功能决定。

## Windows Job 与模拟器

Windows 会将 Job 归属传给后代；即使 ALAS 使用短命的 `cmd /c start` 断开进程树，
模拟器也仍在原来的 Job 中。因此 `TerminateJobObject` 会连带关闭它。
此外，模拟器若继承 `ALAS_LAUNCHER_PID`，也会命中启动器的环境标记兜底清理。

`src/backend_process.rs` 创建后端 Job，设置以下限制：

- `JOB_OBJECT_LIMIT_BREAKAWAY_OK`：允许子进程显式请求脱离。
- `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`：最后一个 Job 句柄关闭时回收普通 worker，包括根进程提前退出后的后代。

不能使用 `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`，否则普通 Python worker 也会自动脱离。
Python 根进程先以挂起状态创建，登记到 Job 后再恢复；登记或恢复失败时终止并回收根进程，避免启动竞态和残留。
Job 句柄由 `OwnedHandle` 持有，初始化失败、正常退出及错误展开均会释放。
显式终止后除等待根进程外，还查询 Job 的活动进程数，最多等待 5 秒确认普通 worker 全部退出；
超时或查询失败返回错误，避免把后端根进程退出误报为整组清理完成。

启动器向后端注入 `ALAS_LAUNCHER_JOB_BREAKAWAY=1` 作为能力声明。配套 ALAS 的
`PlatformWindows.execute(wait=False)` 为启动 cmd 增加 `CREATE_BREAKAWAY_FROM_JOB`，
并从复制的环境中移除 `ALAS_LAUNCHER_PID`、`ALAS_LAUNCHER_JOB_BREAKAWAY` 和
`ALAS_WEBUI_TRUST_SECRET`。模拟器及其后代因此既不属于后端 Job，也不会被残留进程扫描误杀。
该能力变量是双方协议，不能只更新一边就声称修复；旧 ALAS 不请求脱离，旧启动器不允许脱离。
Unix 仍使用现有进程组逻辑。

## 验证

`cargo test --locked backend_process::tests` 使用测试程序自身充当后端、worker 和模拟器，
检查显式终止、丢弃句柄及根进程提前退出三条路径：根进程与普通 worker 退出，显式脱离的模拟器继续存活。
测试通过已打开的专用进程句柄清理伪模拟器，不操作真实模拟器。
ALAS 侧的 `tests.test_emulator_process_lifetime` 验证能力判断、环境清理、同步管理命令与失败分支。

启动器的 Windows 清单要求管理员权限，未提权时完整测试程序会因错误 740 无法启动。
可在临时 Cargo 测试项目中通过 `#[path]` 加载原始 `backend_process.rs` 与 `window_util.rs`
运行同一组生命周期测试；这只隔离测试壳，不修改生产清单。完整项目编译另用
`cargo check --locked --all-targets` 验证，不能把测试壳通过当作全部启动器测试通过。

系统语义参考：[Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)。
