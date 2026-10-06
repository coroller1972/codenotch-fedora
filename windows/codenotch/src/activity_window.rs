//! The Activity window (upstream's TimelinePane): sessions per project over a day, a week or a
//! month, with what each cost. An ordinary decorated, resizable window, the Mac's 1100 × 720 with a
//! 900 × 560 minimum. Created when opened and destroyed when closed, as the settings window is.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "activity";

/// Built on a later turn of the event loop, for the reason `settings_window::open` gives
pub fn open(app: &AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || {
        let app = handle.clone();
        let _ = handle.run_on_main_thread(move || open_now(&app));
    });
}

fn open_now(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("activity.html".into()))
        .title("Codenotch Activity")
        .inner_size(1100.0, 720.0)
        .min_inner_size(900.0, 560.0)
        .center()
        .theme(crate::theme_choice(app))
        .initialization_script(crate::theme_script(crate::resolved_theme(app)));
    match builder.build() {
        Ok(w) => {
            if let Some(icon) = crate::trayicon::window_mark() {
                let _ = w.set_icon(icon);
            }
            let _ = w.set_focus();
        }
        Err(e) => crate::applog(&format!("activity window: {e}")),
    }
}

#[tauri::command]
pub fn open_activity(app: AppHandle) {
    open(&app);
}
