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
