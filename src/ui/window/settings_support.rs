use super::*;

pub(super) fn store_storage_root(root: &str) -> Result<(), crate::network::SecretStoreError> {
    crate::network::store_storage_root(root)
}

pub(super) fn delete_storage_root() {
    crate::network::delete_storage_root();
}

pub(super) fn app_settings(state: &Arc<Mutex<AppState>>) -> AppSettings {
    state.lock().unwrap().settings.clone()
}

pub(super) fn update_app_settings<F>(state: &Arc<Mutex<AppState>>, updater: F)
where
    F: FnOnce(&mut AppSettings),
{
    let mut guard = state.lock().unwrap();
    let mut settings = guard.settings.clone();
    updater(&mut settings);
    if let Err(err) = guard.cache.save_settings(&settings) {
        eprintln!("[SETTINGS] failed to persist settings: {err}");
        return;
    }
    guard.settings = settings;
    drop(guard);
    crate::player::notify_daemon_settings_changed();
}

pub(super) fn persist_and_sync_settings<F>(
    state: &Arc<Mutex<AppState>>,
    updater: F,
) -> anyhow::Result<()>
where
    F: FnOnce(&mut AppSettings),
{
    {
        let mut guard = state.lock().unwrap();
        let mut settings = guard.settings.clone();
        updater(&mut settings);
        guard.cache.save_settings(&settings)?;
        guard.settings = settings;
    }
    crate::player::notify_daemon_settings_changed();
    Ok(())
}

pub(super) fn preview_auto_cache_evictions<F>(
    state: &Arc<Mutex<AppState>>,
    updater: F,
) -> anyhow::Result<usize>
where
    F: FnOnce(&mut AppSettings),
{
    let guard = state.lock().unwrap();
    let mut settings = guard.settings.clone();
    updater(&mut settings);
    Ok(guard.cache.auto_download_eviction_count(&settings)?)
}

pub(super) fn build_settings_warning_dialog(
    stack: &Stack,
    title: &str,
    secondary_text: &str,
    accept_label: &str,
) -> MessageDialog {
    let dialog = MessageDialog::builder()
        .modal(true)
        .message_type(MessageType::Warning)
        .text(title)
        .secondary_text(secondary_text)
        .build();
    dialog.add_css_class("settings-warning-dialog");
    if let Some(window) = stack
        .root()
        .and_then(|root| root.downcast::<ApplicationWindow>().ok())
    {
        dialog.set_transient_for(Some(&window));
    }
    dialog.add_button("Cancel", ResponseType::Cancel);
    dialog.add_button(accept_label, ResponseType::Accept);
    dialog.set_default_response(ResponseType::Cancel);
    dialog
}

pub(super) fn persist_storage_root_override(
    state: &Arc<Mutex<AppState>>,
    raw_root: &str,
) -> anyhow::Result<()> {
    let trimmed = raw_root.trim().to_string();
    let mut guard = state.lock().unwrap();
    guard.cache.migrate_storage_root(&trimmed)?;
    if trimmed.is_empty() {
        delete_storage_root();
    } else {
        store_storage_root(&trimmed)?;
    }
    Ok(())
}
