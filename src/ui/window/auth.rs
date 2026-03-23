use super::*;

pub(super) fn store_cookie(cookie: &str) -> Result<(), keyring::Error> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    entry.set_password(cookie)
}

pub(super) fn get_cookie() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
    entry.get_password().ok()
}

pub(super) fn delete_cookie() {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
        let _ = entry.delete_credential();
    }
}

pub(super) fn store_vr_refresh_token(token: &str) -> Result<(), keyring::Error> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_VR_TOKEN)?;
    entry.set_password(token)
}

pub(super) fn get_vr_refresh_token() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_VR_TOKEN).ok()?;
    entry.get_password().ok()
}

pub(super) fn candidate_chromium_cookie_paths() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".config/microsoft-edge"));
        roots.push(home.join(".config/microsoft-edge-beta"));
        roots.push(home.join(".config/microsoft-edge-dev"));
        roots.push(home.join(".config/microsoft-edge-nightly"));
        roots.push(home.join(".config/google-chrome"));
        roots.push(home.join(".config/google-chrome-beta"));
        roots.push(home.join(".config/google-chrome-unstable"));
        roots.push(home.join(".config/chromium"));
        roots.push(home.join(".config/BraveSoftware/Brave-Browser"));
        roots.push(home.join(".config/vivaldi"));
        roots.push(home.join(".config/opera"));
        roots.push(home.join(".var/app/com.microsoft.Edge/config/microsoft-edge"));
        roots.push(home.join(".var/app/com.microsoft.Edge/config/microsoft-edge-beta"));
        roots.push(home.join(".var/app/com.microsoft.Edge/config/microsoft-edge-dev"));
        roots.push(home.join(".var/app/com.microsoft.Edge/config/microsoft-edge-nightly"));
        roots.push(home.join(".var/app/com.google.Chrome/config/google-chrome"));
        roots.push(home.join(".var/app/org.chromium.Chromium/config/chromium"));
    }

    let mut candidates = Vec::new();
    for root in roots {
        candidates.push(root.join("Default/Cookies"));
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("Profile "))
                {
                    candidates.push(path.join("Cookies"));
                }
            }
        }
    }

    candidates
}

pub(super) fn newest_existing_path(paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    paths
        .into_iter()
        .filter(|path| path.is_file())
        .max_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        })
}

pub(super) fn chromium_safe_storage_password() -> anyhow::Result<String> {
    let connection = ZbusConnection::session().context("Failed to open session bus")?;

    for schema in [
        "chrome_libsecret_os_crypt_password_v2",
        "chrome_libsecret_os_crypt_password_v1",
    ] {
        for application in [
            "chromium",
            "chrome",
            "google-chrome",
            "microsoft-edge",
            "Brave",
            "brave",
            "vivaldi",
            "opera",
            "Code",
        ] {
            let mut attrs = HashMap::<&str, &str>::new();
            attrs.insert("xdg:schema", schema);
            attrs.insert("application", application);

            let search = connection.call_method(
                Some("org.freedesktop.secrets"),
                "/org/freedesktop/secrets",
                Some("org.freedesktop.Secret.Service"),
                "SearchItems",
                &attrs,
            )?;
            let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
                search.body().deserialize()?;

            let object_path = if let Some(path) = unlocked.first() {
                path.clone()
            } else if let Some(path) = locked.first() {
                let unlock = connection.call_method(
                    Some("org.freedesktop.secrets"),
                    "/org/freedesktop/secrets",
                    Some("org.freedesktop.Secret.Service"),
                    "Unlock",
                    &(vec![path.clone()]),
                )?;
                let (unlocked_paths, _prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
                    unlock.body().deserialize()?;
                if let Some(unlocked_path) = unlocked_paths.first() {
                    unlocked_path.clone()
                } else {
                    continue;
                }
            } else {
                continue;
            };

            let open_session = connection.call_method(
                Some("org.freedesktop.secrets"),
                "/org/freedesktop/secrets",
                Some("org.freedesktop.Secret.Service"),
                "OpenSession",
                &("plain", Value::new("")),
            )?;
            let (_output, session): (OwnedValue, OwnedObjectPath) =
                open_session.body().deserialize()?;

            let secrets = connection.call_method(
                Some("org.freedesktop.secrets"),
                "/org/freedesktop/secrets",
                Some("org.freedesktop.Secret.Service"),
                "GetSecrets",
                &(vec![object_path.clone()], session),
            )?;
            let reply: HashMap<OwnedObjectPath, SecretResponse> = secrets.body().deserialize()?;
            if let Some(secret) = reply.get(&object_path).map(|entry| &entry.2) {
                return String::from_utf8(secret.clone())
                    .context("Invalid Chromium secret encoding");
            }
        }
    }

    Err(anyhow::anyhow!("No Chromium safe-storage secret found"))
}

pub(super) fn chromium_cookie_key(password: &str) -> [u8; 16] {
    let mut output = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), b"saltysalt", 1, &mut output);
    output
}

pub(super) fn decrypt_chromium_cookie(
    value: String,
    encrypted_value: Vec<u8>,
    keys: &[[u8; 16]],
) -> Option<String> {
    if !value.is_empty() {
        return Some(value);
    }
    if encrypted_value.len() < 4 {
        return None;
    }

    let prefix = &encrypted_value[..3];
    if prefix != b"v10" && prefix != b"v11" && prefix != b"v20" {
        return None;
    }

    let payload = &encrypted_value[3..];
    let iv = [b' '; 16];

    for key in keys {
        let cipher = Aes128CbcDec::new(&(*key).into(), &iv.into());
        let mut buffer = payload.to_vec();
        if let Ok(plaintext) = cipher.decrypt_padded_mut::<Pkcs7>(&mut buffer) {
            if let Ok(text) = String::from_utf8(plaintext.to_vec()) {
                return Some(text);
            }
            if plaintext.len() > 32 {
                if let Ok(text) = String::from_utf8(plaintext[32..].to_vec()) {
                    return Some(text);
                }
            }
        }
    }

    None
}

pub(super) fn load_cookie_from_chromium_profile(
    settings: &AppSettings,
) -> anyhow::Result<Option<String>> {
    let db_path = if settings.chromium_cookie_path.trim().is_empty() {
        match newest_existing_path(candidate_chromium_cookie_paths()) {
            Some(path) => path,
            None => return Ok(None),
        }
    } else {
        let path = PathBuf::from(settings.chromium_cookie_path.trim());
        if !path.is_file() {
            return Ok(None);
        }
        path
    };

    let temp_db =
        std::env::temp_dir().join(format!("static-edge-cookies-{}.db", uuid::Uuid::new_v4()));
    std::fs::copy(&db_path, &temp_db)
        .with_context(|| format!("Failed to copy Edge cookie DB from {}", db_path.display()))?;

    let result = (|| -> anyhow::Result<Option<String>> {
        let password = chromium_safe_storage_password()?;
        let keys = [
            chromium_cookie_key(&password),
            chromium_cookie_key("peanuts"),
            chromium_cookie_key(""),
        ];

        let conn = SqliteConnection::open(&temp_db)?;
        let mut stmt = conn.prepare(
            "SELECT host_key, name, value, CAST(encrypted_value AS BLOB)
             FROM cookies
             WHERE host_key LIKE '%youtube.com%'
             ORDER BY CASE WHEN host_key LIKE '%music.youtube.com%' THEN 0 ELSE 1 END, host_key DESC, name ASC",
        )?;

        let mut rows = stmt.query([])?;
        let mut seen = HashSet::new();
        let mut cookie_pairs = Vec::new();

        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            let value: String = row.get(2)?;
            let encrypted_value: Vec<u8> = row.get(3)?;
            let Some(decrypted) = decrypt_chromium_cookie(value, encrypted_value, &keys) else {
                continue;
            };
            if decrypted.is_empty() || !seen.insert(name.clone()) {
                continue;
            }
            cookie_pairs.push((name, decrypted));
        }

        if cookie_pairs.is_empty() {
            return Ok(None);
        }

        Ok(Some(
            cookie_pairs
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; "),
        ))
    })();

    let _ = std::fs::remove_file(&temp_db);
    result
}

pub(super) async fn validate_cookie_and_build_client(
    cookie: String,
    source_label: &str,
) -> Option<Arc<YtMusicClient>> {
    let client = match YtMusicClient::new(&cookie) {
        Ok(client) => Arc::new(client),
        Err(err) => {
            eprintln!("[AUTH] {source_label}: invalid cookie: {err}");
            return None;
        }
    };

    let check_client = client.clone();
    let auth_result = gio::spawn_blocking(move || check_client.check_auth()).await;
    match auth_result {
        Ok(Ok(true)) => Some(client),
        Ok(Ok(false)) => {
            eprintln!("[AUTH] {source_label}: cookie is not authenticated");
            None
        }
        Ok(Err(err)) => {
            eprintln!("[AUTH] {source_label}: auth check failed: {err}");
            None
        }
        Err(err) => {
            eprintln!("[AUTH] {source_label}: auth worker failed: {err:?}");
            None
        }
    }
}

pub(super) async fn activate_authenticated_client(
    client: Arc<YtMusicClient>,
    state: &Arc<Mutex<AppState>>,
    stack: &Stack,
) {
    if let Ok(Some(vr_token)) = gio::spawn_blocking(get_vr_refresh_token).await {
        client.set_vr_refresh_token(&vr_token);
    }
    {
        let mut guard = state.lock().unwrap();
        guard.client = Some(client);
        guard.home_cache = None;
    }
    stack.set_visible_child_name("main");
}
