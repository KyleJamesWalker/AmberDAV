//! Optional `connection.json` sidecar: the live IP/port/password/URL written to
//! a configured path so external launchers and a future Decky plugin can show
//! the connection details without scraping stdout. Off unless a path is set.

use std::net::IpAddr;
use std::path::Path;

use serde::Serialize;

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
}
