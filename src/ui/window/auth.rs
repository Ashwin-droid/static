use super::*;

pub(super) enum CookieValidationOutcome {
    Authenticated(Arc<NetworkState>),
    Rejected,
    Deferred(Arc<NetworkState>),
}

pub(super) async fn validate_cookie_and_build_client(
    cookie: String,
    source_label: &str,
) -> CookieValidationOutcome {
    let source = source_label.to_string();
    match gio::spawn_blocking(move || NetworkState::validate_cookie(cookie, &source)).await {
        Ok(crate::network_state::NetworkValidationOutcome::Authenticated(client)) => {
            CookieValidationOutcome::Authenticated(client)
        }
        Ok(crate::network_state::NetworkValidationOutcome::Rejected) => {
            CookieValidationOutcome::Rejected
        }
        Ok(crate::network_state::NetworkValidationOutcome::Deferred(client)) => {
            CookieValidationOutcome::Deferred(client)
        }
        Err(err) => {
            eprintln!("[AUTH] {source_label}: auth worker failed: {err:?}");
            CookieValidationOutcome::Rejected
        }
    }
}

pub(super) async fn stage_restored_client(client: Arc<NetworkState>, state: &Arc<Mutex<AppState>>) {
    let mut guard = state.lock().unwrap();
    guard.client = Some(client);
    guard.home_cache = None;
}

pub(super) async fn activate_authenticated_client(
    client: Arc<NetworkState>,
    state: &Arc<Mutex<AppState>>,
    stack: &Stack,
) {
    stage_restored_client(client, state).await;
    stack.set_visible_child_name("main");
}
