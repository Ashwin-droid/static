use super::*;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn test_state() -> AppState {
    AppState {
        origin: "https://127.0.0.1:4321".into(),
        host: "127.0.0.1:4321".into(),
        token: "secret-session".into(),
        stopping: Arc::new(AtomicBool::new(false)),
        exports: Arc::new(Mutex::new(HashMap::new())),
        export_slot: Arc::new(Semaphore::new(1)),
        sampling_enabled: false,
        connections: Arc::new(Semaphore::new(4)),
    }
}

#[test]
fn websocket_auth_requires_token_host_and_exact_origin() {
    let state = test_state();
    let uri = Uri::from_static("/");
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, state.host.parse().unwrap());
    headers.insert(header::ORIGIN, state.origin.parse().unwrap());
    let auth = Auth {
        token: Some(state.token.clone()),
    };
    assert!(authenticated(&headers, &uri, &auth, &state, true));
    assert!(!authenticated(
        &headers,
        &uri,
        &Auth { token: None },
        &state,
        true
    ));
    headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
    assert!(!authenticated(&headers, &uri, &auth, &state, true));
    headers.insert(header::ORIGIN, state.origin.parse().unwrap());
    headers.insert(header::HOST, "evil.example".parse().unwrap());
    assert!(!authenticated(&headers, &uri, &auth, &state, true));
    headers.insert(header::HOST, state.host.parse().unwrap());
    headers.remove(header::ORIGIN);
    assert!(!authenticated(&headers, &uri, &auth, &state, true));
}

#[test]
fn authority_supports_http2_and_rejects_conflicting_host_values() {
    let state = test_state();
    let uri: Uri = format!("{}/assets/app.js", state.origin).parse().unwrap();
    let mut headers = HeaderMap::new();
    assert!(valid_host(&headers, &uri, &state));
    assert!(!valid_host(&headers, &Uri::from_static("/"), &state));
    headers.insert(header::HOST, state.host.parse().unwrap());
    assert!(valid_host(&headers, &uri, &state));
    headers.insert(header::HOST, "evil.example".parse().unwrap());
    assert!(!valid_host(&headers, &uri, &state));
    headers.insert(header::HOST, state.host.parse().unwrap());
    let evil = Uri::from_static("https://evil.example/");
    assert!(!valid_host(&headers, &evil, &state));
    headers.append(header::HOST, "evil.example".parse().unwrap());
    assert!(!valid_host(&headers, &uri, &state));
}

#[tokio::test]
async fn real_http2_tls_serves_assets_and_checks_download_auth() {
    let server = start_inner(false).unwrap();
    let mut url = url::Url::parse(&server.url).unwrap();
    let token = url
        .fragment()
        .unwrap()
        .strip_prefix("token=")
        .unwrap()
        .to_owned();
    url.set_fragment(None);
    let client = reqwest::Client::builder()
        .no_proxy()
        .https_only(true)
        .http2_prior_knowledge()
        .add_root_certificate(
            reqwest::Certificate::from_pem(server.certificate_pem.as_bytes()).unwrap(),
        )
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let response = client.get(url.clone()).send().await.unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(response.text().await.unwrap().contains("<html"));
    url.set_path("/cartridges/missing");
    let response = client.get(url.clone()).send().await.unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    url.set_query(Some(&format!("token={token}")));
    let response = client.get(url).send().await.unwrap();
    // Correct credentials pass host/authority validation and reach the registry.
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[test]
fn assets_never_resolve_traversal_or_unbundled_files() {
    assert_eq!(asset_path("/"), Some("index.html"));
    assert_eq!(asset_path("/assets/main.js"), Some("assets/main.js"));
    for path in [
        "/../Cargo.toml",
        "/assets/../../secret",
        "/%2e%2e/secret",
        "/assets\\secret",
        "//etc/passwd",
        "/assets/./secret",
    ] {
        assert!(asset_path(path).is_none(), "{path}");
    }
    assert!(
        asset_path("/Cargo.toml")
            .and_then(|path| ASSETS.get_file(path))
            .is_none()
    );
}

#[test]
fn commands_have_bounded_ranges_and_no_filesystem_arguments() {
    for bands in [0, 7, 129, u32::MAX] {
        assert!(validate_command(&Command::Configure { bands }).is_err());
    }
    for bands in [8, 64, 128] {
        assert!(validate_command(&Command::Configure { bands }).is_ok());
    }
    for value in [-0.1, 5.1, f64::NAN, f64::INFINITY] {
        assert!(validate_command(&Command::Volume { value }).is_err());
    }
    assert!(
        serde_json::from_str::<Command>(r#"{"type":"transport","action":"shutdown"}"#).is_err()
    );
    assert!(
        serde_json::from_str::<Command>(r#"{"type":"volume","value":1,"path":"/tmp/audio"}"#)
            .is_err()
    );
    let command = Command::Cartridge {
        id: "../bad".into(),
        video_id: "abc".into(),
        screenshot: "data:image/png;base64,".into(),
        metadata: json!({}),
    };
    assert!(validate_command(&command).is_err());
}

#[tokio::test]
async fn downloads_stream_only_registered_paths_with_session_auth() {
    let state = test_state();
    let path =
        std::env::temp_dir().join(format!("static-visualiser-test-{}", uuid::Uuid::new_v4()));
    let bytes = vec![0x5a; 128 * 1024];
    std::fs::write(&path, &bytes).unwrap();
    state.exports.lock().unwrap().insert(
        "opaque-id".into(),
        (
            crate::visualizer::cartridge::CartridgeStorage::Disk(path.clone()),
            "track.png".into(),
        ),
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, state.host.parse().unwrap());
    let response = download(
        State(state.clone()),
        Path("opaque-id".into()),
        Query(Auth { token: None }),
        headers.clone(),
        Uri::from_static("/"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = download(
        State(state.clone()),
        Path("../outside".into()),
        Query(Auth {
            token: Some(state.token.clone()),
        }),
        headers.clone(),
        Uri::from_static("/"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = download(
        State(state.clone()),
        Path("opaque-id".into()),
        Query(Auth {
            token: Some(state.token.clone()),
        }),
        headers,
        Uri::from_static("/"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"track.png\""
    );
    assert_eq!(
        &axum::body::to_bytes(response.into_body(), bytes.len())
            .await
            .unwrap()[..],
        &bytes
    );
    std::fs::remove_file(path).unwrap();
}

async fn tls_request(server: &Server, request: &str) -> String {
    let url = url::Url::parse(&server.url).unwrap();
    let pem_body: String = server
        .certificate_pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(pem_body)
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", url.port().unwrap()))
        .await
        .unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap(),
            tcp,
        )
        .await
        .unwrap();
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !response.ends_with(b"\r\n\r\n") && response.len() < 16384 {
            let mut byte = [0];
            if tls.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            response.push(byte[0]);
        }
    })
    .await
    .unwrap();
    String::from_utf8(response).unwrap()
}

#[tokio::test]
async fn real_tls_websocket_auth_and_static_routes_without_daemon_access() {
    // Sampling is disabled: successful handshakes must not touch the desktop daemon.
    let server = start_inner(false).unwrap();
    let url = url::Url::parse(&server.url).unwrap();
    let host = format!("127.0.0.1:{}", url.port().unwrap());
    let origin = format!("https://{host}");
    let token = url.fragment().unwrap().strip_prefix("token=").unwrap();
    let request = |token: &str, origin: &str| {
        format!(
            "GET /ws?token={token} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
    };
    assert!(
        tls_request(&server, &request("wrong", &origin))
            .await
            .starts_with("HTTP/1.1 403")
    );
    assert!(
        tls_request(&server, &request(token, "https://evil.example"))
            .await
            .starts_with("HTTP/1.1 403")
    );
    let accepted = tls_request(&server, &request(token, &origin)).await;
    assert!(accepted.starts_with("HTTP/1.1 101"), "{accepted}");
    assert!(
        accepted
            .to_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo=")
    );
    let asset = tls_request(
        &server,
        &format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(asset.starts_with("HTTP/1.1 200"), "{asset}");
    let missing = tls_request(
        &server,
        &format!("GET /Cargo.toml HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    let forbidden = tls_request(
        &server,
        &format!("GET /cartridges/anything HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(forbidden.starts_with("HTTP/1.1 403"), "{forbidden}");
}

#[tokio::test]
async fn native_export_has_no_inherited_tokio_runtime() {
    assert!(tokio::runtime::Handle::try_current().is_ok());
    let result = export_thread(|| {
        assert!(tokio::runtime::Handle::try_current().is_err());
        assert_eq!(std::thread::current().name(), Some("visualiser-export"));
        Ok(42)
    })
    .await
    .unwrap();
    assert_eq!(result, 42);
    let result: Result<()> = export_thread(|| anyhow::bail!("export fixture failed")).await;
    assert_eq!(result.unwrap_err().to_string(), "export fixture failed");
}

#[test]
fn memory_download_reads_bounded_ranges_and_stops_on_disconnect_or_missing_data() {
    let size = 3 * crate::visualizer::cartridge::STREAM_CHUNK_BYTES as u64 + 17;
    let mut offsets = Vec::new();
    let mut received = 0;
    stream_memory_chunks(
        size,
        |offset, count| {
            offsets.push(offset);
            assert!(count as usize <= crate::visualizer::cartridge::STREAM_CHUNK_BYTES);
            Ok(vec![0x5a; count as usize])
        },
        |chunk| {
            received += chunk.unwrap().len() as u64;
            true
        },
    );
    assert_eq!(received, size);
    assert_eq!(offsets.len(), 4);
    let mut calls = 0;
    stream_memory_chunks(
        size,
        |_, count| {
            calls += 1;
            Ok(vec![0; count as usize])
        },
        |_| false,
    );
    assert_eq!(calls, 1);
    let mut errors = 0;
    stream_memory_chunks(
        size,
        |_, _| Ok(Vec::new()),
        |chunk| {
            assert!(chunk.is_err());
            errors += 1;
            true
        },
    );
    assert_eq!(errors, 1);
}

#[tokio::test]
async fn cartridge_import_requires_local_auth_and_rejects_bad_images() {
    let state = test_state();
    let uri: Uri = format!("{}/cartridge/decode", state.origin)
        .parse()
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, state.host.parse().unwrap());
    headers.insert(header::ORIGIN, state.origin.parse().unwrap());
    let response = decode_cartridge(
        State(state.clone()),
        Query(Auth { token: None }),
        headers.clone(),
        uri.clone(),
        Bytes::from_static(b"not an image"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = decode_cartridge(
        State(state.clone()),
        Query(Auth {
            token: Some(state.token.clone()),
        }),
        headers.clone(),
        uri.clone(),
        Bytes::from_static(b"not an image"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let permit = state.export_slot.clone().acquire_owned().await.unwrap();
    let response = decode_cartridge(
        State(state.clone()),
        Query(Auth {
            token: Some(state.token.clone()),
        }),
        headers,
        uri,
        Bytes::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(permit);
    for id in ["../file.png", "https://evil", "short", "abcdefghij!"] {
        assert!(
            validate_command(&Command::PlayCartridge {
                video_id: id.into()
            })
            .is_err()
        );
    }
    assert!(
        validate_command(&Command::PlayCartridge {
            video_id: "dQw4w9WgXcQ".into()
        })
        .is_ok()
    );
}
