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

#[derive(Debug, Clone, Copy)]
pub(super) enum CoordinationFilterListKind {
    Mpris,
    PipeWire,
}

pub(super) fn coordination_filter_mode_labels() -> Vec<&'static str> {
    CoordinationFilterMode::ALL
        .iter()
        .map(|mode| mode.label())
        .collect()
}

pub(super) fn coordination_filter_mode_for_settings(
    settings: &AppSettings,
    kind: CoordinationFilterListKind,
) -> CoordinationFilterMode {
    match kind {
        CoordinationFilterListKind::Mpris => settings.other_mpris_filter_mode,
        CoordinationFilterListKind::PipeWire => settings.pipewire_filter_mode,
    }
}

pub(super) fn set_coordination_filter_mode_for_settings(
    settings: &mut AppSettings,
    kind: CoordinationFilterListKind,
    mode: CoordinationFilterMode,
) {
    match kind {
        CoordinationFilterListKind::Mpris => settings.other_mpris_filter_mode = mode,
        CoordinationFilterListKind::PipeWire => settings.pipewire_filter_mode = mode,
    }
}

pub(super) fn coordination_filter_list_for_settings(
    settings: &AppSettings,
    kind: CoordinationFilterListKind,
) -> &[String] {
    match kind {
        CoordinationFilterListKind::Mpris => &settings.other_mpris_filter_list,
        CoordinationFilterListKind::PipeWire => &settings.pipewire_filter_list,
    }
}

pub(super) fn set_coordination_filter_target_listed(
    settings: &mut AppSettings,
    kind: CoordinationFilterListKind,
    identifiers: &[String],
    listed: bool,
) {
    let list = match kind {
        CoordinationFilterListKind::Mpris => &mut settings.other_mpris_filter_list,
        CoordinationFilterListKind::PipeWire => &mut settings.pipewire_filter_list,
    };

    if listed {
        if let Some(key) = identifiers.first().filter(|key| !key.trim().is_empty()) {
            list.push(key.to_string());
        }
    } else {
        list.retain(|entry| !coordination_filter_contains(&[entry.clone()], identifiers));
    }
    *list = sanitize_coordination_filter_entries(list);
}

pub(super) fn coordination_target_is_listed(
    settings: &AppSettings,
    kind: CoordinationFilterListKind,
    target: &CoordinationTargetSnapshot,
) -> bool {
    if !target.filterable || target.key.trim().is_empty() {
        return false;
    }

    let identifiers = [target.key.clone(), target.label.clone()];
    coordination_filter_contains(
        coordination_filter_list_for_settings(settings, kind),
        &identifiers,
    )
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
