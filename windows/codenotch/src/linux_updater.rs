//! Linux builds are installed locally or through RPM, not the Windows updater.
//! Do not query the Windows release feed or advertise its .exe assets on Linux.
use serde::Serialize;
use tauri::AppHandle;

#[derive(Serialize)]
pub struct UpdateState {
    managed: bool,
    message: &'static str,
}

#[tauri::command]
pub fn get_update_state() -> UpdateState {
    UpdateState { managed: true, message: "Update through your Linux installation" }
}

#[tauri::command]
pub fn check_for_update(_app: AppHandle) {}

#[tauri::command]
pub fn install_update(_app: AppHandle) {}

#[tauri::command]
pub fn open_update_installer() -> Result<(), String> {
    Err("Use your Linux package or rebuild from source".into())
}

pub fn check_on_launch(_app: &AppHandle) {}

#[cfg(test)]
mod tests {
    #[test]
    fn linux_never_offers_a_windows_update() {
        let state = serde_json::to_value(super::get_update_state()).unwrap();
        assert_eq!(state["managed"], true);
        assert!(state.get("available").is_none());
        assert!(super::open_update_installer().is_err());
    }
}
