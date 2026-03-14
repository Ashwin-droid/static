use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Value, json};

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

const SOLVER_LIB: &str = include_str!("solver/yt.solver.bun.lib.js");
const SOLVER_CORE: &str = include_str!("solver/yt.solver.core.js");

/// Cached cipher context: holds the raw player JS and signatureTimestamp.
/// Solving is done via yt-dlp's solver scripts executed through bun.
#[derive(Clone)]
pub struct Cipher {
    /// Raw player JS source (base.js)
    player_js: String,
    /// The signatureTimestamp from the player JS (needed for the player API request).
    pub signature_timestamp: u64,
}

impl Cipher {
    /// Fetch the player JS from YouTube.
    pub async fn from_player(http: &reqwest::Client) -> Result<Self> {
        let player_url = find_player_url(http).await?;
        eprintln!("[CIPHER] player JS URL: {player_url}");

        let player_js = http
            .get(&player_url)
            .send()
            .await
            .context("Failed to fetch player JS")?
            .text()
            .await
            .context("Failed to read player JS")?;

        eprintln!("[CIPHER] fetched base.js ({} bytes)", player_js.len());

        // Extract signatureTimestamp from the player JS
        let sts_re = Regex::new(r"(?:signatureTimestamp|sts)[=:](\d{5})").unwrap();
        let signature_timestamp = sts_re
            .captures(&player_js)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        eprintln!("[CIPHER] signatureTimestamp: {signature_timestamp}");

        Ok(Self {
            player_js,
            signature_timestamp,
        })
    }

    /// Decipher a full signatureCipher string and return the playable URL.
    pub fn decipher_url(&self, signature_cipher: &str) -> Result<String> {
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(signature_cipher.as_bytes())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

        let encrypted_sig = params.get("s").context("No 's' param in signatureCipher")?;
        let sp = params
            .get("sp")
            .cloned()
            .unwrap_or_else(|| "sig".to_string());
        let base_url = params
            .get("url")
            .context("No 'url' param in signatureCipher")?;

        eprintln!(
            "[CIPHER] solving sig ({} chars) and n-transform...",
            encrypted_sig.len()
        );

        let n_val = extract_url_param(base_url, "n");
        let (deciphered_sig, new_n) = self.solve(encrypted_sig, n_val.as_deref())?;

        let mut final_url = format!(
            "{base_url}&{sp}={sig}",
            sig = urlencoding::encode(&deciphered_sig)
        );

        if let (Some(orig_n), Some(transformed_n)) = (&n_val, &new_n) {
            eprintln!("[CIPHER] n-transform: {orig_n} -> {transformed_n}");
            final_url = replace_url_param(&final_url, "n", transformed_n);
        }

        Ok(final_url)
    }

    /// Solve sig and n challenges by running the full solver pipeline through bun.
    /// This sends the raw player JS + challenges to jsc() in a single call,
    /// exactly mirroring what yt-dlp does.
    fn solve(&self, sig: &str, n: Option<&str>) -> Result<(String, Option<String>)> {
        let mut requests = vec![json!({"type": "sig", "challenges": [sig]})];
        if let Some(n_val) = n {
            requests.push(json!({"type": "n", "challenges": [n_val]}));
        }

        let data = json!({
            "type": "player",
            "player": self.player_js,
            "requests": requests,
        });

        let script = format!(
            "{SOLVER_LIB}\nObject.assign(globalThis, lib);\n{SOLVER_CORE}\nconsole.log(JSON.stringify(jsc({data})));",
            data = serde_json::to_string(&data)?
        );

        eprintln!("[CIPHER] running solver via bun (full pipeline)...");
        let output_str = run_bun_stdin(&script)?;
        let output: Value =
            serde_json::from_str(&output_str).context("Failed to parse solver output")?;

        if output["type"].as_str() == Some("error") {
            bail!("Solver error: {}", output["error"]);
        }

        // Parse sig response (always first)
        let sig_resp = &output["responses"][0];
        if sig_resp["type"].as_str() == Some("error") {
            bail!("Sig solve failed: {}", sig_resp["error"]);
        }
        let deciphered = sig_resp["data"][sig]
            .as_str()
            .context("No sig result in solver output")?
            .to_string();

        // Parse n response (second, if requested)
        let new_n = if n.is_some() {
            let n_resp = &output["responses"][1];
            if n_resp["type"].as_str() == Some("result") {
                n_resp["data"][n.unwrap()].as_str().map(|s| s.to_string())
            } else {
                eprintln!(
                    "[CIPHER] n-transform failed (may still work): {}",
                    n_resp["error"]
                );
                None
            }
        } else {
            None
        };

        Ok((deciphered, new_n))
    }
}

async fn find_player_url(http: &reqwest::Client) -> Result<String> {
    let page = http
        .get("https://music.youtube.com")
        .send()
        .await
        .context("Failed to fetch music.youtube.com")?
        .text()
        .await?;

    let re = Regex::new(r#"(/s/player/[a-zA-Z0-9]+/[^\s"']+?base\.js)"#).unwrap();

    let path = re
        .captures(&page)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .context("Could not find player JS URL in page")?;

    Ok(format!("https://music.youtube.com{path}"))
}

fn run_bun_stdin(script: &str) -> Result<String> {
    let mut child = Command::new("bun")
        .args([
            "--bun",
            "run",
            "--no-addons",
            "--prefer-offline",
            "--install=fallback",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn bun")?;

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .context("Failed to write to bun stdin")?;

    drop(child.stdin.take());

    let output = child.wait_with_output().context("Failed to wait for bun")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("[CIPHER] bun stderr: {}", &stderr[..stderr.len().min(500)]);
        bail!(
            "bun failed (exit {}): {}",
            output.status,
            &stderr[..stderr.len().min(200)]
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "bun returned empty output. stderr: {}",
            &stderr[..stderr.len().min(200)]
        );
    }
    Ok(stdout)
}

fn extract_url_param(url: &str, key: &str) -> Option<String> {
    url.split('?')
        .nth(1)
        .unwrap_or(url)
        .split('&')
        .find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            if k == key {
                Some(urlencoding::decode(v).ok()?.into_owned())
            } else {
                None
            }
        })
}

fn replace_url_param(url: &str, key: &str, new_val: &str) -> String {
    let encoded = urlencoding::encode(new_val);
    let search = format!("{key}=");
    if let Some(pos) = url.find(&search) {
        let after = &url[pos + search.len()..];
        let end = after.find('&').unwrap_or(after.len());
        format!("{}{search}{encoded}{}", &url[..pos], &after[end..])
    } else {
        format!("{url}&{key}={encoded}")
    }
}
