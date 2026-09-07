use std::fs;
use std::path::{Path, PathBuf};

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

#[test]
fn internet_and_keyring_stay_inside_network() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");
    let mut files = Vec::new();
    collect_rust_files(&src_dir, &mut files);

    let mut violations = Vec::new();

    for path in files {
        let Ok(relative) = path.strip_prefix(manifest_dir) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");

        if relative == "src/network.rs"
            || relative.starts_with("src/network/")
            || relative.starts_with("src/tests/")
        {
            continue;
        }

        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };

        if contents.contains("reqwest::") {
            violations.push(format!("{relative}: found reqwest usage outside network"));
        }
        if contents.contains("keyring::") {
            violations.push(format!("{relative}: found keyring usage outside network"));
        }

        let allow_url_literals = relative == "src/ui/content_loading.rs"
            || relative == "src/ipc.rs"
            // These two modules host and display the loopback-only visualizer.
            // Remote audio downloads still go through src/network.
            || relative == "src/visualizer/server.rs"
            || relative == "src/visualizer/window.rs"
            || relative == "src/network_parser/shared_parser.rs";

        if !allow_url_literals && (contents.contains("https://") || contents.contains("http://")) {
            violations.push(format!("{relative}: found URL literal outside allowlist"));
        }
    }

    assert!(
        violations.is_empty(),
        "network boundary violations:\n{}",
        violations.join("\n")
    );
}

/// A reqwest client without an explicit timeout can hang forever on a
/// half-dead network (DNS resolves but packets are blackholed), which used to
/// freeze the daemon loop and stall playback until wifi was switched off.
/// Every client built inside src/network must opt into a timeout, and the
/// timeout-less `Client::new()` shortcut is banned there entirely.
#[test]
fn network_http_clients_always_set_timeouts() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let network_dir = manifest_dir.join("src").join("network");
    let mut files = Vec::new();
    collect_rust_files(&network_dir, &mut files);
    files.push(manifest_dir.join("src").join("network.rs"));

    let mut violations = Vec::new();

    for path in files {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let relative = path
            .strip_prefix(manifest_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if contents.contains("Client::new()") {
            violations.push(format!(
                "{relative}: reqwest Client::new() has no timeout; use a builder with .timeout()"
            ));
        }

        let mut search_from = 0;
        while let Some(offset) = contents[search_from..].find("Client::builder()") {
            let builder_start = search_from + offset;
            let chain_end = contents[builder_start..]
                .find(".build()")
                .map(|end| builder_start + end)
                .unwrap_or(contents.len());
            if !contents[builder_start..chain_end].contains(".timeout(") {
                let line = contents[..builder_start].lines().count();
                violations.push(format!(
                    "{relative}:{line}: reqwest client builder chain never calls .timeout()"
                ));
            }
            search_from = chain_end.max(builder_start + 1);
        }
    }

    assert!(
        violations.is_empty(),
        "un-timed-out HTTP clients:\n{}",
        violations.join("\n")
    );
}
