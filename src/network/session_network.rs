use std::collections::HashMap;
use std::path::PathBuf;

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context, anyhow, bail};
use pbkdf2::pbkdf2_hmac;
use rusqlite::Connection as SqliteConnection;
use sha1::Sha1;
use zbus::{
    blocking::{Connection as ZbusConnection, Proxy as ZbusProxy},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use crate::offline::AppSettings;

pub const KEYRING_SERVICE: &str = "com.static.music";
pub const KEYRING_USER: &str = "cookie";
pub const KEYRING_VR_TOKEN: &str = "vr_refresh_token";
pub const KEYRING_STORAGE_ROOT: &str = "storage_root";
pub type SecretStoreError = anyhow::Error;

const SECRET_SERVICE_NAME: &str = "org.freedesktop.secrets";
const SECRET_SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SECRET_SERVICE_INTERFACE: &str = "org.freedesktop.Secret.Service";
const SECRET_COLLECTION_INTERFACE: &str = "org.freedesktop.Secret.Collection";
const SECRET_ITEM_INTERFACE: &str = "org.freedesktop.Secret.Item";
const SECRET_PROMPT_INTERFACE: &str = "org.freedesktop.Secret.Prompt";
const SECRET_DEFAULT_COLLECTION_ALIAS: &str = "default";
const SECRET_SESSION_COLLECTION_ALIAS: &str = "session";
const SECRET_DEFAULT_TARGET: &str = "default";
const SECRET_LEGACY_APPLICATION: &str = "rust-keyring";
const SECRET_CONTENT_TYPE_TEXT: &str = "text/plain";
const SECRET_NO_PROMPT_PATH: &str = "/";
const SECRET_PROMPT_WINDOW_ID: &str = "";

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type SecretResponse = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);
type SecretPayload = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

pub fn store_cookie(cookie: &str) -> Result<(), SecretStoreError> {
    store_secret(KEYRING_USER, cookie)
}

pub fn get_cookie() -> Option<String> {
    get_secret(KEYRING_USER).ok().flatten()
}

pub fn delete_cookie() {
    let _ = delete_secret(KEYRING_USER);
}

pub fn store_vr_refresh_token(token: &str) -> Result<(), SecretStoreError> {
    store_secret(KEYRING_VR_TOKEN, token)
}

pub fn get_vr_refresh_token() -> Option<String> {
    get_secret(KEYRING_VR_TOKEN).ok().flatten()
}

pub fn store_storage_root(root: &str) -> Result<(), SecretStoreError> {
    store_secret(KEYRING_STORAGE_ROOT, root)
}

pub fn get_storage_root() -> Option<String> {
    get_secret(KEYRING_STORAGE_ROOT).ok().flatten()
}

pub fn delete_storage_root() {
    let _ = delete_secret(KEYRING_STORAGE_ROOT);
}

// Keep Secret Service access on raw zbus to avoid the dbus-secret-service/keyring
// disconnect path while remaining compatible with items that crate already wrote.
fn secret_service_connection() -> anyhow::Result<ZbusConnection> {
    ZbusConnection::session().context("Failed to open a Secret Service session bus connection")
}

fn secret_service_proxy(connection: &ZbusConnection) -> anyhow::Result<ZbusProxy<'_>> {
    ZbusProxy::new(
        connection,
        SECRET_SERVICE_NAME,
        SECRET_SERVICE_PATH,
        SECRET_SERVICE_INTERFACE,
    )
    .context("Failed to create Secret Service proxy")
}

fn secret_collection_proxy<'a>(
    connection: &'a ZbusConnection,
    collection_path: &'a OwnedObjectPath,
) -> anyhow::Result<ZbusProxy<'a>> {
    ZbusProxy::new(
        connection,
        SECRET_SERVICE_NAME,
        collection_path.as_str(),
        SECRET_COLLECTION_INTERFACE,
    )
    .with_context(|| format!("Failed to create Secret Service collection proxy {collection_path}"))
}

fn secret_item_proxy<'a>(
    connection: &'a ZbusConnection,
    item_path: &'a OwnedObjectPath,
) -> anyhow::Result<ZbusProxy<'a>> {
    ZbusProxy::new(
        connection,
        SECRET_SERVICE_NAME,
        item_path.as_str(),
        SECRET_ITEM_INTERFACE,
    )
    .with_context(|| format!("Failed to create Secret Service item proxy {item_path}"))
}

fn secret_prompt_proxy<'a>(
    connection: &'a ZbusConnection,
    prompt_path: &'a OwnedObjectPath,
) -> anyhow::Result<ZbusProxy<'a>> {
    ZbusProxy::new(
        connection,
        SECRET_SERVICE_NAME,
        prompt_path.as_str(),
        SECRET_PROMPT_INTERFACE,
    )
    .with_context(|| format!("Failed to create Secret Service prompt proxy {prompt_path}"))
}

fn is_no_prompt_path(path: &OwnedObjectPath) -> bool {
    path.as_str() == SECRET_NO_PROMPT_PATH
}

fn wait_for_secret_prompt(
    connection: &ZbusConnection,
    prompt_path: &OwnedObjectPath,
) -> anyhow::Result<()> {
    if is_no_prompt_path(prompt_path) {
        return Ok(());
    }

    let prompt_proxy = secret_prompt_proxy(connection, prompt_path)?;
    let mut completed = prompt_proxy
        .receive_signal("Completed")
        .context("Failed to subscribe to Secret Service prompt completion")?;
    prompt_proxy
        .call::<_, _, ()>("Prompt", &(SECRET_PROMPT_WINDOW_ID,))
        .context("Failed to trigger Secret Service prompt")?;

    let Some(message) = completed.next() else {
        bail!("Secret Service prompt ended unexpectedly");
    };
    let (dismissed, _result): (bool, OwnedValue) = message
        .body()
        .deserialize()
        .context("Failed to decode Secret Service prompt completion")?;
    if dismissed {
        bail!("Secret Service prompt was dismissed");
    }

    Ok(())
}

fn open_secret_session(connection: &ZbusConnection) -> anyhow::Result<OwnedObjectPath> {
    let proxy = secret_service_proxy(connection)?;
    let (_output, session): (OwnedValue, OwnedObjectPath) = proxy
        .call("OpenSession", &("plain", Value::new("")))
        .context("Failed to open a Secret Service session")?;
    Ok(session)
}

fn unlock_secret_objects(
    connection: &ZbusConnection,
    objects: &[OwnedObjectPath],
) -> anyhow::Result<()> {
    if objects.is_empty() {
        return Ok(());
    }

    let proxy = secret_service_proxy(connection)?;
    let (_unlocked, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) = proxy
        .call("Unlock", &(objects.to_vec(),))
        .context("Failed to unlock Secret Service objects")?;
    if !is_no_prompt_path(&prompt) {
        wait_for_secret_prompt(connection, &prompt)?;
    }

    Ok(())
}

fn secret_search_attributes(username: &str, include_target: bool) -> HashMap<&str, &str> {
    let mut attributes = HashMap::new();
    attributes.insert("service", KEYRING_SERVICE);
    attributes.insert("username", username);
    if include_target {
        attributes.insert("target", SECRET_DEFAULT_TARGET);
    }
    attributes
}

fn dedupe_secret_paths(paths: impl IntoIterator<Item = OwnedObjectPath>) -> Vec<OwnedObjectPath> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();

    for path in paths {
        if seen.insert(path.to_string()) {
            deduped.push(path);
        }
    }

    deduped
}

fn unique_secret_match(
    username: &str,
    paths: impl IntoIterator<Item = OwnedObjectPath>,
    scope: &str,
) -> anyhow::Result<Option<OwnedObjectPath>> {
    let mut matches = dedupe_secret_paths(paths);
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        count => Err(anyhow!(
            "Found {count} Secret Service items for {username} during {scope} search"
        )),
    }
}

fn search_secret_service_items(
    connection: &ZbusConnection,
    attributes: &HashMap<&str, &str>,
) -> anyhow::Result<Vec<OwnedObjectPath>> {
    let proxy = secret_service_proxy(connection)?;
    let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = proxy
        .call("SearchItems", &(attributes,))
        .context("Failed to search Secret Service items")?;
    Ok(dedupe_secret_paths(unlocked.into_iter().chain(locked)))
}

fn search_secret_collection_items(
    connection: &ZbusConnection,
    collection_path: &OwnedObjectPath,
    attributes: &HashMap<&str, &str>,
) -> anyhow::Result<Vec<OwnedObjectPath>> {
    let proxy = secret_collection_proxy(connection, collection_path)?;
    let items: Vec<OwnedObjectPath> = proxy
        .call("SearchItems", &(attributes,))
        .with_context(|| format!("Failed to search Secret Service collection {collection_path}"))?;
    Ok(dedupe_secret_paths(items))
}

fn secret_collection_path(connection: &ZbusConnection) -> anyhow::Result<OwnedObjectPath> {
    let proxy = secret_service_proxy(connection)?;

    for alias in [
        SECRET_DEFAULT_COLLECTION_ALIAS,
        SECRET_SESSION_COLLECTION_ALIAS,
    ] {
        let path: OwnedObjectPath = proxy
            .call("ReadAlias", &(alias,))
            .with_context(|| format!("Failed to read Secret Service collection alias {alias}"))?;
        if !is_no_prompt_path(&path) {
            return Ok(path);
        }
    }

    let collections: Vec<OwnedObjectPath> = proxy
        .get_property("Collections")
        .context("Failed to list Secret Service collections")?;
    collections
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No Secret Service collection is available"))
}

fn find_secret_item_path(
    connection: &ZbusConnection,
    username: &str,
) -> anyhow::Result<Option<OwnedObjectPath>> {
    let primary_attributes = secret_search_attributes(username, true);
    let primary_matches = search_secret_service_items(connection, &primary_attributes)?;
    if !primary_matches.is_empty() {
        return unique_secret_match(username, primary_matches, "service-wide");
    }

    let default_collection: OwnedObjectPath = secret_service_proxy(connection)?
        .call("ReadAlias", &(SECRET_DEFAULT_COLLECTION_ALIAS,))
        .context("Failed to read Secret Service default collection alias")?;
    if is_no_prompt_path(&default_collection) {
        return Ok(None);
    }

    let legacy_attributes = secret_search_attributes(username, false);
    let legacy_matches =
        search_secret_collection_items(connection, &default_collection, &legacy_attributes)?;
    unique_secret_match(username, legacy_matches, "legacy default-collection")
}

fn secret_item_label(username: &str) -> String {
    format!(
        "{username}@{KEYRING_SERVICE}:{SECRET_DEFAULT_TARGET} (static via zbus)"
    )
}

fn secret_item_properties(username: &str) -> HashMap<String, OwnedValue> {
    let attributes = HashMap::from([
        ("service".to_string(), KEYRING_SERVICE.to_string()),
        ("username".to_string(), username.to_string()),
        ("target".to_string(), SECRET_DEFAULT_TARGET.to_string()),
        (
            "application".to_string(),
            SECRET_LEGACY_APPLICATION.to_string(),
        ),
    ]);

    HashMap::from([
        (
            "org.freedesktop.Secret.Item.Label".to_string(),
            OwnedValue::try_from(Value::new(secret_item_label(username)))
                .expect("Secret Service label should serialize"),
        ),
        (
            "org.freedesktop.Secret.Item.Attributes".to_string(),
            attributes.into(),
        ),
    ])
}

fn secret_payload(session: OwnedObjectPath, secret: &str) -> SecretPayload {
    (
        session,
        Vec::new(),
        secret.as_bytes().to_vec(),
        SECRET_CONTENT_TYPE_TEXT.to_string(),
    )
}

fn store_secret(username: &str, secret: &str) -> anyhow::Result<()> {
    let connection = secret_service_connection()?;

    if let Some(item_path) = find_secret_item_path(&connection, username)? {
        unlock_secret_objects(&connection, std::slice::from_ref(&item_path))?;
        let item_proxy = secret_item_proxy(&connection, &item_path)?;
        let payload = secret_payload(open_secret_session(&connection)?, secret);
        item_proxy
            .call::<_, _, ()>("SetSecret", &(payload,))
            .with_context(|| format!("Failed to update Secret Service item for {username}"))?;
        return Ok(());
    }

    let collection_path = secret_collection_path(&connection)?;
    unlock_secret_objects(&connection, std::slice::from_ref(&collection_path))?;

    let collection_proxy = secret_collection_proxy(&connection, &collection_path)?;
    let properties = secret_item_properties(username);
    let payload = secret_payload(open_secret_session(&connection)?, secret);
    let (_item, prompt): (OwnedObjectPath, OwnedObjectPath) = collection_proxy
        .call("CreateItem", &(properties, payload, true))
        .with_context(|| format!("Failed to create Secret Service item for {username}"))?;
    if !is_no_prompt_path(&prompt) {
        wait_for_secret_prompt(&connection, &prompt)?;
    }

    Ok(())
}

fn get_secret(username: &str) -> anyhow::Result<Option<String>> {
    let connection = secret_service_connection()?;
    let Some(item_path) = find_secret_item_path(&connection, username)? else {
        return Ok(None);
    };

    unlock_secret_objects(&connection, std::slice::from_ref(&item_path))?;
    let item_proxy = secret_item_proxy(&connection, &item_path)?;
    let secret: SecretResponse = item_proxy
        .call("GetSecret", &(open_secret_session(&connection)?,))
        .with_context(|| format!("Failed to read Secret Service item for {username}"))?;
    let (_, _, secret_bytes, _) = secret;
    let value = String::from_utf8(secret_bytes)
        .with_context(|| format!("Secret Service item for {username} is not valid UTF-8"))?;
    Ok(Some(value))
}

fn delete_secret(username: &str) -> anyhow::Result<()> {
    let connection = secret_service_connection()?;
    let Some(item_path) = find_secret_item_path(&connection, username)? else {
        return Ok(());
    };

    unlock_secret_objects(&connection, std::slice::from_ref(&item_path))?;
    let item_proxy = secret_item_proxy(&connection, &item_path)?;
    let prompt: OwnedObjectPath = item_proxy
        .call("Delete", &())
        .with_context(|| format!("Failed to delete Secret Service item for {username}"))?;
    if !is_no_prompt_path(&prompt) {
        wait_for_secret_prompt(&connection, &prompt)?;
    }

    Ok(())
}

pub fn candidate_chromium_cookie_paths() -> Vec<PathBuf> {
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

pub fn newest_existing_path(paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    paths
        .into_iter()
        .filter(|path| path.is_file())
        .max_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        })
}

pub fn chromium_safe_storage_password() -> anyhow::Result<String> {
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

pub fn chromium_cookie_key(password: &str) -> [u8; 16] {
    let mut output = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), b"saltysalt", 1, &mut output);
    output
}

pub fn decrypt_chromium_cookie(
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

pub fn load_cookie_from_chromium_profile(
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
        let mut seen = std::collections::HashSet::new();
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
