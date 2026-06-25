//! Browser native-messaging host bridging the `linux-entra-sso` browser
//! extension to the Microsoft Identity Broker over D-Bus.
//!
//! This is a Rust replacement for the upstream Python `linux-entra-sso.py`
//! native host. The browser (Firefox) spawns this binary and talks to it over
//! stdin/stdout using the WebExtension native-messaging framing:
//!
//! * **stdin**: 4 bytes of message length in native byte order, followed by
//!   that many bytes of UTF-8 JSON.
//! * **stdout**: 4 bytes of length in native byte order, followed by UTF-8
//!   JSON. We flush after every message.
//!
//! Each request JSON carries a `"command"` field. We answer with
//! `{"command": <cmd>, "message": <result>}`.
//!
//! Every request is translated into a D-Bus method call on the Microsoft
//! Identity Broker (`com.microsoft.identity.broker1`), which runs *inside* the
//! container. We reach it through the container's session bus socket, which is
//! exposed on the host at a path passed to [`run`].
//!
//! IMPORTANT: stdout is reserved for the native-messaging protocol. All logging
//! goes to stderr via `tracing` — never log to stdout.

use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, info, warn};

use zbus::Connection;

/// Native-messaging protocol version reported to / expected by the broker.
const PROTOCOL_VERSION: &str = "0.0";

/// The Microsoft Edge browser client id the broker recognises.
const EDGE_BROWSER_CLIENT_ID: &str = "d7b530a4-7680-4c23-a8bf-c52c121d2e87";

/// Default SSO URL used when the extension does not supply one.
const SSO_URL_DEFAULT: &str = "https://login.microsoftonline.com/";

/// Default scopes requested when the extension does not supply any.
const GRAPH_SCOPES: [&str; 1] = ["https://graph.microsoft.com/.default"];

/// The well-known D-Bus name the broker is published under (on the container bus).
const BROKER_NAME: &str = "com.microsoft.identity.broker1";

/// The broker's object path.
const BROKER_PATH: &str = "/com/microsoft/identity/broker1";

/// The broker's interface name.
const BROKER_INTERFACE: &str = "com.microsoft.identity.Broker1";

/// How long to wait for the container bus socket to appear before bailing out.
const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time to wait for a single broker D-Bus reply. Guards against a hung
/// broker (a GTK app that deadlocks) wedging the whole single-threaded message
/// loop and stalling browser SSO for the rest of the session.
const BROKER_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Run the native messaging host loop, bridging stdin/stdout to the broker
/// reachable on the given container bus socket. Returns when stdin closes.
pub async fn run(bus_socket: &Path) -> Result<()> {
    let session_id = new_session_uuid();
    info!(session_id = %session_id, "starting native messaging host");

    // Try to connect to the broker. If it isn't reachable, we still continue —
    // we reconnect lazily per request and report state to the extension.
    let mut connection = connect_broker(bus_socket).await.ok();
    if connection.is_some() {
        info!("connected to container broker bus");
    } else {
        warn!("broker not reachable at startup; will retry on demand");
    }

    // Send the initial broker state to the extension.
    let mut online = connection.is_some();
    write_message(&broker_state_message(online))
        .await
        .context("failed to send initial broker state")?;

    let mut stdin = tokio::io::stdin();

    loop {
        // Read the 4-byte length prefix (native byte order).
        let mut len_buf = [0u8; 4];
        match stdin.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                info!("stdin closed; shutting down native messaging host");
                break;
            }
            Err(e) => return Err(e).context("failed to read message length from stdin"),
        }
        let len = u32::from_ne_bytes(len_buf) as usize;
        debug!(len, "reading native messaging request");

        // Read the JSON body.
        let mut body = vec![0u8; len];
        stdin
            .read_exact(&mut body)
            .await
            .context("failed to read message body from stdin")?;

        let request: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                error!("failed to parse request JSON: {e}");
                continue;
            }
        };

        let command = request
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        debug!(command = %command, "handling request");

        // (Re)connect lazily: the broker may not have been up at startup, or the
        // container may have restarted since (the bind-mounted bus socket is
        // recreated, leaving our old connection stale).
        if connection.is_none() {
            connection = connect_broker(bus_socket).await.ok();
        }

        let mut result = handle_command(connection.as_ref(), &session_id, &command, &request).await;

        // A transport-level failure means the connection is stale (e.g. the
        // container was restarted). Reconnect once and retry before giving up,
        // so a single blip doesn't break SSO for the rest of the session.
        if matches!(&result, Err(e) if is_transport_error(e)) {
            warn!(command = %command, "broker call failed at transport level; reconnecting");
            connection = connect_broker(bus_socket).await.ok();
            if connection.is_some() {
                result = handle_command(connection.as_ref(), &session_id, &command, &request).await;
            }
        }

        let response = match result {
            Ok(message) => json!({ "command": command, "message": message }),
            Err(e) => {
                error!(command = %command, "command failed: {e:#}");
                json!({
                    "command": command,
                    "message": { "error": format!("{e:#}") },
                })
            }
        };

        // Notify the extension if the broker's reachability changed.
        let now_online = connection.is_some();
        if now_online != online {
            online = now_online;
            write_message(&broker_state_message(online))
                .await
                .context("failed to send broker state change")?;
        }

        write_message(&response)
            .await
            .context("failed to write response to stdout")?;
    }

    Ok(())
}

/// Blocking entry point (sets up a tokio runtime).
pub fn run_blocking(bus_socket: &Path) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(run(bus_socket))
}

/// Diagnostic mode: directly query the broker and print raw responses.
/// This is what the browser extension sees — use it to debug "Device unknown".
pub fn test_blocking(bus_socket: &Path) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(test(bus_socket))
}

/// The signed-in account, surfaced to the GUI (a subset of the broker's
/// `getAccounts` reply).
#[derive(Clone, Serialize, Deserialize)]
pub struct AccountInfo {
    pub name: String,
    pub username: String,
    pub tenant: String,
}

/// Query the broker for the first signed-in account. `Ok(None)` means no account
/// is registered (not yet enrolled / keyring locked). Runs **inside** the
/// container against the broker's session bus.
pub fn accounts_blocking(bus_socket: &Path) -> Result<Option<AccountInfo>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(async {
        let session_id = new_session_uuid();
        let conn = connect_broker(bus_socket)
            .await
            .context("could not connect to the container broker bus")?;
        let reply = handle_command(Some(&conn), &session_id, "getAccounts", &json!({})).await?;
        let account = reply
            .get("accounts")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first());
        Ok(account.map(|a| {
            let field = |k: &str| {
                a.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            AccountInfo {
                name: field("name"),
                username: field("username"),
                tenant: field("realm"),
            }
        }))
    })
}

async fn test(bus_socket: &Path) -> Result<()> {
    let session_id = new_session_uuid();
    println!("Connecting to broker at {} ...", bus_socket.display());
    let conn = connect_broker(bus_socket)
        .await
        .context("could not connect to the container broker bus")?;
    println!("✓ Connected to broker bus\n");

    // 1. Broker version
    match handle_command(Some(&conn), &session_id, "getVersion", &json!({})).await {
        Ok(v) => println!("getVersion:\n{}\n", pretty(&v)),
        Err(e) => println!("getVersion FAILED: {e:#}\n"),
    }

    // 2. Accounts
    let accounts = match handle_command(Some(&conn), &session_id, "getAccounts", &json!({})).await {
        Ok(v) => {
            println!("getAccounts:\n{}\n", pretty(&v));
            v
        }
        Err(e) => {
            println!("getAccounts FAILED: {e:#}");
            println!("\n→ The broker can't read accounts. Keyring locked, or no enrollment.");
            return Ok(());
        }
    };

    // Extract the first account
    let first = accounts
        .get("accounts")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .cloned();

    let account = match first {
        Some(a) => a,
        None => {
            println!("→ No accounts registered. Sign in via: intune-container enroll");
            return Ok(());
        }
    };

    // 3. The critical call: acquire a PRT SSO cookie (this is what makes the
    //    browser see a *registered device*). If this fails or returns no
    //    cookie, that's why Teams shows "Device: Unregistered".
    //    Params go under "message", exactly like the browser extension sends.
    let req = json!({
        "message": {
            "account": account,
            "ssoUrl": "https://login.microsoftonline.com/",
        }
    });
    println!("acquirePrtSsoCookie (the device-bound cookie Teams needs):");
    match handle_command(Some(&conn), &session_id, "acquirePrtSsoCookie", &req).await {
        Ok(v) => {
            println!("{}\n", pretty(&v));

            // The cookie lives under cookieItems[].cookieContent. Older/other
            // shapes may expose a top-level cookieContent, so accept either.
            let has_cookie = v
                .get("cookieItems")
                .and_then(|c| c.as_array())
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.get("cookieContent")
                            .and_then(|c| c.as_str())
                            .is_some_and(|s| !s.is_empty())
                    })
                })
                || v.get("cookieContent")
                    .and_then(|c| c.as_str())
                    .is_some_and(|s| !s.is_empty());

            let succeeded = v
                .get("telemetry")
                .and_then(|t| t.get("is_successful"))
                .and_then(|s| s.as_str())
                .is_some_and(|s| s == "true");

            if v.get("error").is_some() || (!has_cookie && !succeeded) {
                println!("→ No valid PRT cookie. The device isn't Entra-registered (Workplace");
                println!("  Join). Intune MDM 'compliant' is separate from device registration.");
            } else {
                println!("✓ Got a PRT cookie — SSO should work. If Teams still fails, enable");
                println!("  'Background SSO' for teams.microsoft.com in the extension.");
            }
        }
        Err(e) => println!("acquirePrtSsoCookie FAILED: {e:#}"),
    }

    // 4. Device status — this is exactly what the extension popup's
    //    "Device ( ● ... )" indicator computes (see linux-entra-sso
    //    src/device.js). It is INDEPENDENT of the PRT cookie above:
    //      a) acquire a Graph token silently,
    //      b) read the `deviceid` claim from that access token's JWT,
    //      c) query Graph for the device's compliance.
    //    If any step fails, the extension shows "Device unknown".
    println!("\nDevice status (what the extension's 'Device' indicator checks):");
    let token_req = json!({ "message": { "account": account } });
    match handle_command(Some(&conn), &session_id, "acquireTokenSilently", &token_req).await {
        Ok(v) => {
            let access_token = v
                .get("brokerTokenResponse")
                .and_then(|r| r.get("accessToken"))
                .and_then(Value::as_str);
            match access_token {
                None => {
                    println!("  ✗ No Graph access token returned by the broker.");
                    println!("    → Extension shows 'Device unknown' (cannot read deviceid).");
                }
                Some(token) => match jwt_payload(token) {
                    Err(e) => {
                        println!("  ✗ Could not decode the access token JWT: {e:#}");
                        println!("    → Extension shows 'Device unknown'.");
                    }
                    Ok(claims) => match claims.get("deviceid").and_then(Value::as_str) {
                        None => {
                            println!("  ✗ Graph token has NO 'deviceid' claim.");
                            println!("    → The device is enrolled in Intune (MDM) but is NOT");
                            println!("      Entra-registered (Workplace Join), so the token isn't");
                            println!("      device-bound. The extension shows 'Device unknown',");
                            println!("      and device-based Conditional Access (Teams error");
                            println!("      53003) will fail. Re-run enrollment / sign in through");
                            println!("      the portal so the broker performs Workplace Join.");
                        }
                        Some(device_id) => {
                            println!("  ✓ Graph token carries deviceid={device_id}");
                            println!("    The device IS Entra-registered (Workplace Join OK).");
                            report_device_query(token, device_id).await;
                        }
                    },
                },
            }
        }
        Err(e) => {
            println!("  ✗ acquireTokenSilently FAILED: {e:#}");
            println!("    → Extension shows 'Device unknown'.");
        }
    }

    Ok(())
}

/// Replicate the final step of the extension's device check: query Microsoft
/// Graph for the device object (compliance + display name), using the same
/// Graph token. Reports the exact HTTP outcome so the user can tell whether
/// "Device unknown" is a tenant permission issue (403) or a sync delay (404)
/// rather than a bug. Uses an in-process HTTPS client so it works inside the
/// minimal container (no `curl`).
async fn report_device_query(token: &str, device_id: &str) {
    let url = format!(
        "https://graph.microsoft.com/v1.0/devices(deviceId='{{{device_id}}}')?$select=isCompliant,displayName"
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            println!("    (could not build HTTP client to verify Graph query: {e})");
            return;
        }
    };
    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            println!("    (could not reach Microsoft Graph to verify: {e})");
            return;
        }
    };
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    match status {
        200 => {
            let parsed: Option<Value> = serde_json::from_str(&body).ok();
            let name = parsed
                .as_ref()
                .and_then(|v| v.get("displayName"))
                .and_then(Value::as_str)
                .unwrap_or("(unknown)");
            let compliant = parsed
                .as_ref()
                .and_then(|v| v.get("isCompliant"))
                .and_then(Value::as_bool);
            println!("  ✓ Graph /devices query OK — name=\"{name}\", compliant={compliant:?}");
            println!("    The extension SHOULD show a real device status (not 'unknown').");
            println!("    If it still says 'unknown', reopen the popup to force a refresh.");
        }
        403 => {
            println!("  ! Graph /devices query returned 403 Forbidden.");
            println!("    → THIS is why the extension shows 'Device unknown'. Your account");
            println!("      isn't allowed to read device objects from the directory (a tenant");
            println!("      policy). It is purely cosmetic: the device is registered and SSO");
            println!("      works. Nothing to fix on this machine.");
        }
        404 => {
            println!("  ! Graph /devices query returned 404 (device object not found).");
            println!("    → The deviceid isn't visible in the directory yet. Usually resolves");
            println!("      after the device fully syncs; SSO is unaffected. Re-check later.");
        }
        other => {
            println!("  ! Graph /devices query returned HTTP {other}.");
            if !body.is_empty() {
                let snippet: String = body.chars().take(200).collect();
                println!("    Response: {snippet}");
            }
            println!("    → The extension shows 'Device unknown' when this call isn't 200.");
        }
    }
}

/// Decode the payload (claims) section of a JWT without verifying the
/// signature. Used only for diagnostics.
fn jwt_payload(token: &str) -> Result<Value> {
    let payload_b64 = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("not a JWT (no payload segment)"))?;
    let bytes = base64url_decode(payload_b64)?;
    serde_json::from_slice(&bytes).context("JWT payload is not valid JSON")
}

/// Minimal base64url (RFC 4648 §5, no padding) decoder. Sufficient for
/// decoding a JWT payload for diagnostics; avoids pulling in a dependency.
fn base64url_decode(input: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Result<u8> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(anyhow!("invalid base64url character")),
        })
    }

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        if c == b'=' {
            break;
        }
        buf = (buf << 6) | u32::from(val(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Look up a request parameter, accepting it either at the top level (how the
/// browser extension sends it) or nested under a `message` object (how our own
/// `sso-test` diagnostic sends it).
fn param<'a>(request: &'a Value, key: &str) -> Option<&'a Value> {
    request
        .get(key)
        .or_else(|| request.get("message").and_then(|m| m.get(key)))
}

/// Dispatch a single command, returning the `message` payload on success.
async fn handle_command(
    connection: Option<&Connection>,
    session_id: &str,
    command: &str,
    request: &Value,
) -> Result<Value> {
    let connection = connection.ok_or_else(|| anyhow!("broker is not connected"))?;

    match command {
        "getVersion" => {
            let request_json =
                serde_json::to_string(&json!({ "msalCppVersion": "1.0.0-intune-container" }))?;
            let reply = call_broker(
                connection,
                "getLinuxBrokerVersion",
                session_id,
                &request_json,
            )
            .await?;
            let mut version: Value = serde_json::from_str(&reply)
                .context("failed to parse getLinuxBrokerVersion reply")?;
            if let Some(obj) = version.as_object_mut() {
                obj.insert("native".to_string(), json!("intune-container"));
            }
            Ok(version)
        }
        "getAccounts" => {
            let context = json!({
                "clientId": EDGE_BROWSER_CLIENT_ID,
                "redirectUri": session_id,
            });
            let context_str = serde_json::to_string(&context)?;
            let reply = call_broker(connection, "getAccounts", session_id, &context_str).await?;
            serde_json::from_str(&reply).context("failed to parse getAccounts reply")
        }
        "acquirePrtSsoCookie" => {
            // The browser extension sends `account`/`ssoUrl` at the TOP level
            // (see linux-entra-sso broker.js). `param()` also accepts a nested
            // `message` object so our own `sso-test` keeps working.
            let account = param(request, "account")
                .cloned()
                .ok_or_else(|| anyhow!("missing account field"))?;
            let sso_url = param(request, "ssoUrl")
                .and_then(Value::as_str)
                .unwrap_or(SSO_URL_DEFAULT)
                .to_string();

            let scopes: Vec<String> = GRAPH_SCOPES.iter().map(|s| s.to_string()).collect();
            let request_obj = json!({
                "account": account,
                "authParameters": auth_parameters(&account, &scopes, Some(&sso_url)),
                "mamEnrollment": false,
                "ssoUrl": sso_url,
            });
            let request_str = serde_json::to_string(&request_obj)?;
            let reply =
                call_broker(connection, "acquirePrtSsoCookie", session_id, &request_str).await?;
            serde_json::from_str(&reply).context("failed to parse acquirePrtSsoCookie reply")
        }
        "acquireTokenSilently" => {
            let account = param(request, "account")
                .cloned()
                .ok_or_else(|| anyhow!("missing account field"))?;
            let scopes: Vec<String> = match param(request, "scopes").and_then(Value::as_array) {
                Some(arr) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect(),
                None => GRAPH_SCOPES.iter().map(|s| s.to_string()).collect(),
            };

            let request_obj = json!({
                "authParameters": auth_parameters(&account, &scopes, None),
            });
            let request_str = serde_json::to_string(&request_obj)?;
            let reply =
                call_broker(connection, "acquireTokenSilently", session_id, &request_str).await?;
            serde_json::from_str(&reply).context("failed to parse acquireTokenSilently reply")
        }
        other => Err(anyhow!("unknown command: {other}")),
    }
}

/// Build the `authParameters` object the broker expects.
fn auth_parameters(account: &Value, scopes: &[String], sso_url: Option<&str>) -> Value {
    let mut params = json!({
        "account": account,
        "additionalQueryParametersForAuthorization": {},
        "authority": "https://login.microsoftonline.com/common",
        "authorizationType": if sso_url.is_some() { 8 } else { 1 },
        "clientId": EDGE_BROWSER_CLIENT_ID,
        "redirectUri": "https://login.microsoftonline.com/common/oauth2/nativeclient",
        "requestedScopes": scopes,
        "username": account.get("username").cloned().unwrap_or(Value::Null),
        "uxContextHandle": -1,
    });

    if let Some(sso_url) = sso_url {
        if let Some(obj) = params.as_object_mut() {
            obj.insert("ssoUrl".to_string(), json!(sso_url));
        }
    }

    params
}

/// Invoke a broker method that takes `(protocol_version, correlation_id,
/// request_json)` and returns a single JSON string.
async fn call_broker(
    connection: &Connection,
    method: &str,
    session_id: &str,
    request_json: &str,
) -> Result<String> {
    debug!(method, "calling broker");
    let args = (PROTOCOL_VERSION, session_id, request_json);
    let call = connection.call_method(
        Some(BROKER_NAME),
        BROKER_PATH,
        Some(BROKER_INTERFACE),
        method,
        &args,
    );
    let reply = tokio::time::timeout(BROKER_CALL_TIMEOUT, call)
        .await
        .map_err(|_| {
            anyhow!(
                "broker method {method} timed out after {}s",
                BROKER_CALL_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("broker method {method} failed"))?;

    let body = reply.body();
    let result: String = body
        .deserialize()
        .with_context(|| format!("failed to read string reply from {method}"))?;
    Ok(result)
}

/// Build a `brokerStateChanged` message for the extension.
fn broker_state_message(online: bool) -> Value {
    json!({
        "command": "brokerStateChanged",
        "message": if online { "online" } else { "offline" },
    })
}

/// Whether an error originates from the D-Bus transport (a dead/stale
/// connection) rather than application logic. Used to decide whether a
/// reconnect-and-retry is worth attempting.
fn is_transport_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<zbus::Error>().is_some())
}

/// Connect to the broker on the container session bus, waiting for its socket.
async fn connect_broker(bus_socket: &Path) -> Result<Connection> {
    wait_for_socket(bus_socket).await?;

    let addr = format!("unix:path={}", bus_socket.display());
    let conn = zbus::connection::Builder::address(addr.as_str())
        .context("invalid container bus address")?
        .build()
        .await
        .with_context(|| format!("failed to connect to container bus at {addr}"))?;
    Ok(conn)
}

/// Poll until the container bus socket exists (or we time out).
async fn wait_for_socket(path: &Path) -> Result<()> {
    let deadline = Instant::now() + SOCKET_WAIT_TIMEOUT;
    loop {
        if path.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "container bus socket {} did not appear within {}s",
                path.display(),
                SOCKET_WAIT_TIMEOUT.as_secs()
            ));
        }
        debug!(socket = %path.display(), "waiting for container bus socket");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Write a single message to stdout using native-messaging framing.
async fn write_message(value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value).context("failed to serialise response")?;
    let len = u32::try_from(body.len()).context("response too large for native messaging")?;

    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(&len.to_ne_bytes())
        .await
        .context("failed to write length prefix")?;
    stdout
        .write_all(&body)
        .await
        .context("failed to write message body")?;
    stdout.flush().await.context("failed to flush stdout")?;
    Ok(())
}

/// Build a random UUIDv4 string from `/dev/urandom` without pulling in a crate.
fn new_session_uuid() -> String {
    let mut bytes = [0u8; 16];
    if let Err(e) = read_random(&mut bytes) {
        // Fall back to a time-derived seed; uniqueness here is best-effort and
        // only used as a per-session correlation id.
        warn!("failed to read /dev/urandom: {e}; using fallback session id");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((nanos >> (i * 4)) & 0xff) as u8;
        }
    }

    // Set the version (4) and variant (10xx) bits per RFC 4122.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// Fill `buf` with random bytes from `/dev/urandom`.
fn read_random(buf: &mut [u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the field-location bug: the real `linux-entra-sso`
    /// browser extension sends `account`/`ssoUrl` at the TOP level of the
    /// request (see broker.js `postMessage({command, account, ssoUrl})`),
    /// NOT nested under a `message` object. The native host previously only
    /// looked under `message`, so every browser cookie request failed with
    /// "missing message field" and SSO silently fell back to a password
    /// prompt. `param()` must read the top-level field.
    #[test]
    fn param_reads_top_level_like_the_browser_extension() {
        let request = json!({
            "command": "acquirePrtSsoCookie",
            "account": { "username": "user@example.com" },
            "ssoUrl": "https://login.microsoftonline.com/",
        });

        assert_eq!(
            param(&request, "ssoUrl").and_then(Value::as_str),
            Some("https://login.microsoftonline.com/"),
        );
        assert_eq!(
            param(&request, "account")
                .and_then(|a| a.get("username"))
                .and_then(Value::as_str),
            Some("user@example.com"),
        );
    }

    /// Our own `sso-test` diagnostic wraps the parameters in a `message`
    /// object. `param()` must still find them via the fallback so both
    /// callers keep working.
    #[test]
    fn param_falls_back_to_nested_message() {
        let request = json!({
            "command": "acquirePrtSsoCookie",
            "message": {
                "account": { "username": "user@example.com" },
                "ssoUrl": "https://login.microsoftonline.com/",
            },
        });

        assert_eq!(
            param(&request, "ssoUrl").and_then(Value::as_str),
            Some("https://login.microsoftonline.com/"),
        );
        assert!(param(&request, "account").is_some());
    }

    /// A top-level field must win over a nested one (the browser shape is
    /// authoritative).
    #[test]
    fn param_prefers_top_level_over_nested() {
        let request = json!({
            "ssoUrl": "top",
            "message": { "ssoUrl": "nested" },
        });
        assert_eq!(
            param(&request, "ssoUrl").and_then(Value::as_str),
            Some("top")
        );
    }

    #[test]
    fn param_returns_none_when_absent() {
        let request = json!({ "command": "acquirePrtSsoCookie" });
        assert!(param(&request, "account").is_none());
    }
}
