//! 状态栏常驻 — tray 图标 + 菜单 (打开主界面 / 退出)。
//!
//! 常驻策略: 平时保留 Dock 图标 (普通 App); 用户点窗口关闭按钮时隐藏窗口
//! 并隐藏 Dock 图标, 只留状态栏; 从状态栏“打开主界面”或点击 Dock 图标唤回时
//! 恢复 Dock 图标与窗口。

use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

pub const TRAY_ID: &str = "codexff-tray";

/// macOS 模板图标: 纯黑 + alpha 形状, 系统自动适配深浅色状态栏
#[cfg(target_os = "macos")]
fn macos_tray_icon() -> Option<tauri::image::Image<'static>> {
    const ICON_BYTES: &[u8] = include_bytes!("../icons/tray/template.png");
    tauri::image::Image::from_bytes(ICON_BYTES).ok()
}

/// 唤回主窗口 (tray 菜单 / deeplink 导入)
pub fn show_main_window(app: &AppHandle) {
    // 从状态栏唤回 = 恢复普通 App 形态 (Dock 图标可见)
    #[cfg(target_os = "macos")]
    let _ = app.set_dock_visibility(true);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn build_tray_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    MenuBuilder::new(app)
        .item(&MenuItem::with_id(
            app,
            "show_main",
            "打开主界面",
            true,
            None::<&str>,
        )?)
        .separator()
        .item(&MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?)
        .build()
}

fn handle_menu_event(app: &AppHandle, event_id: &str) {
    match event_id {
        "show_main" => show_main_window(app),
        "quit" => {
            log::info!("状态栏菜单退出");
            // Codex 仍指向本地路由时不能直接退出: Codex 不重读 config,
            // 下一请求仍打本地端口 → 502。弹窗提示先退出 Codex。
            if crate::local_router::codex_points_at_router()
                && crate::session_manager::codex_running()
            {
                use tauri::Emitter;
                show_main_window(app);
                let _ = app.emit("exit-blocked", ());
            } else {
                app.exit(0);
            }
        }
        _ => log::warn!("未处理的菜单事件: {event_id}"),
    }
}

/// 构建并注册状态栏图标 + 菜单
pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_tray_menu(app)?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("CodexFF")
        .menu(&menu)
        .on_menu_event(|app, event| handle_menu_event(app, &event.id.0))
        .show_menu_on_left_click(true);

    #[cfg(target_os = "macos")]
    {
        if let Some(icon) = macos_tray_icon() {
            builder = builder.icon(icon).icon_as_template(true);
        } else if let Some(icon) = app.default_window_icon() {
            log::warn!("回退用默认窗口图标作 tray 图标");
            builder = builder.icon(icon.clone());
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(icon) = app.default_window_icon() {
            builder = builder.icon(icon.clone());
        }
    }

    builder.build(app)?;
    Ok(())
}
