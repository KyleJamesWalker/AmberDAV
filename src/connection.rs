//! Optional `connection.json` sidecar: the live IP/port/password/URL written to
//! a configured path so external launchers and a future Decky plugin can show
//! the connection details without scraping stdout. Off unless a path is set.
//!
//! The file is written at startup and then kept fresh by a small background
//! task: Wi-Fi often associates *after* launch (the device screen re-queries
//! the IP every paint for exactly this reason), so the boot-time value can be
//! `0.0.0.0` — without the refresher, launchers would read that forever
//! (issue #48).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

#[derive(Serialize)]
pub struct ConnectionInfo {
    pub ip: IpAddr,
    pub port: u16,
    pub user: &'static str,
    pub password: Option<String>,
    pub url: String,
}

impl ConnectionInfo {
    pub fn new(ip: IpAddr, port: u16, password: Option<String>) -> ConnectionInfo {
        ConnectionInfo {
            ip,
            port,
            user: "anything",
            password,
            url: format!("http://{ip}:{port}/"),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Write atomically (write a temp file then rename) so readers never see a
    /// partial file. Best-effort: logs and returns on error rather than killing
    /// the app.
    pub fn write(&self, path: &Path) {
        // Append a `.tmp` suffix (don't replace the extension) so the temp file is
        // <basename>.tmp for any configured path, then atomically rename into place.
        let tmp_name = format!(
            "{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        let tmp = path.with_file_name(tmp_name);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if let Err(e) = std::fs::write(&tmp, self.to_json()) {
            tracing::warn!("cannot write {}: {e}", tmp.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::warn!("cannot rename into {}: {e}", path.display());
        }
    }
}

/// How often the refresher re-checks the local IP. `current_ip()` is a
/// cheap connect-less UDP-socket trick (no packets), and the device screen
/// already re-queries it every couple of seconds — this is negligible.
const REFRESH_EVERY: Duration = Duration::from_secs(3);

/// Write the sidecar now and keep it fresh in the background: rewrite
/// (atomically, via the same temp+rename) whenever the local IP changes —
/// the late-Wi-Fi fix for `"ip": "0.0.0.0"` persisting forever (issue #48).
/// The port and password can't change at runtime, so the IP is the only live
/// field. The task ends when `shutdown` fires.
pub fn spawn_refresher(
    path: PathBuf,
    port: u16,
    password: Option<String>,
    shutdown: CancellationToken,
) {
    let ip = crate::state::current_ip();
    ConnectionInfo::new(ip, port, password.clone()).write(&path);
    tokio::spawn(refresh_loop(
        path,
        port,
        password,
        shutdown,
        ip,
        crate::state::current_ip,
        REFRESH_EVERY,
    ));
}

/// The refresher proper, with the IP source and interval injected so the
/// rewrite-on-change and stop-on-shutdown behavior is testable without
/// real network state.
async fn refresh_loop(
    path: PathBuf,
    port: u16,
    password: Option<String>,
    shutdown: CancellationToken,
    mut last: IpAddr,
    ip_source: impl Fn() -> IpAddr + Send + 'static,
    every: Duration,
) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the interval's immediate first tick
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        let ip = ip_source();
        if ip == last {
            continue;
        }
        tracing::info!("ip changed {last} -> {ip}; rewriting {}", path.display());
        ConnectionInfo::new(ip, port, password.clone()).write(&path);
        last = ip;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn serializes_expected_fields() {
        let info = ConnectionInfo {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7)),
            port: 8080,
            user: "anything",
            password: Some("ab12".into()),
            url: "http://10.0.0.7:8080/".into(),
        };
        let json = info.to_json();
        assert!(json.contains("\"ip\": \"10.0.0.7\""));
        assert!(json.contains("\"port\": 8080"));
        assert!(json.contains("\"password\": \"ab12\""));
        assert!(json.contains("\"url\": \"http://10.0.0.7:8080/\""));
    }

    #[test]
    fn hidden_password_serializes_null() {
        let info = ConnectionInfo {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7)),
            port: 8080,
            user: "anything",
            password: None,
            url: "http://10.0.0.7:8080/".into(),
        };
        assert!(info.to_json().contains("\"password\": null"));
    }

    // The late-Wi-Fi scenario end to end (issue #48): the loop starts on the
    // boot-time 0.0.0.0, rewrites the file once the injected IP source flips
    // to a real address, and exits promptly when shutdown fires. A 1s
    // timeout turns a leaked task into a clean failure.
    #[tokio::test]
    async fn refresher_rewrites_on_ip_change_and_stops_on_shutdown() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("amberdav-conn-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connection.json");

        let boot_ip = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
        ConnectionInfo::new(boot_ip, 8080, Some("pw".into())).write(&path);
        assert!(std::fs::read_to_string(&path).unwrap().contains("0.0.0.0"));

        let associated = Arc::new(AtomicBool::new(false));
        let flag = associated.clone();
        let source = move || -> IpAddr {
            if flag.load(Ordering::SeqCst) {
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7))
            } else {
                boot_ip
            }
        };

        let shutdown = CancellationToken::new();
        let task = tokio::spawn(refresh_loop(
            path.clone(),
            8080,
            Some("pw".into()),
            shutdown.clone(),
            boot_ip,
            source,
            Duration::from_millis(5),
        ));

        // "Wi-Fi associates" — the file must follow within a few ticks.
        associated.store(true, Ordering::SeqCst);
        let mut updated = String::new();
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            updated = std::fs::read_to_string(&path).unwrap();
            if updated.contains("10.0.0.7") {
                break;
            }
        }
        assert!(updated.contains("\"ip\": \"10.0.0.7\""), "{updated}");
        assert!(updated.contains("\"url\": \"http://10.0.0.7:8080/\""));

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("refresh loop must end when shutdown fires")
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
