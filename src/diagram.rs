//! Visual: regen teks mermaid + kelas status per node → flowmaid render_svg.
//! Status runner menimpa warna role via `classDef` + `class` yang di-append —
//! file `.mmd` sumber tidak pernah diubah.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NodeState {
    Idle,
    Current,
    Ok,
    Fail,
    Skip,
    Manual,
}

impl NodeState {
    fn class(self) -> Option<&'static str> {
        match self {
            NodeState::Idle => None,
            NodeState::Current => Some("frCur"),
            NodeState::Ok => Some("frOk"),
            NodeState::Fail => Some("frFail"),
            NodeState::Skip => Some("frSkip"),
            NodeState::Manual => Some("frMan"),
        }
    }
}

const STATUS_CLASSDEFS: &str = "\
classDef frCur fill:#eab308,stroke:#a16207,color:#1c1400
classDef frOk fill:#22c55e,stroke:#15803d,color:#052e12
classDef frFail fill:#ef4444,stroke:#b91c1c,color:#fff
classDef frSkip fill:#94a3b8,stroke:#64748b,color:#111
classDef frMan fill:#a78bfa,stroke:#7c3aed,color:#1e1033";

/// Teks mermaid + overlay status (append classDef/class; sumber tak diubah).
pub fn mermaid_with_status(src: &str, states: &[(String, NodeState)]) -> String {
    let mut out = src.trim_end().to_string();
    out.push('\n');
    out.push_str(STATUS_CLASSDEFS);
    out.push('\n');
    for (id, st) in states {
        if let Some(cls) = st.class() {
            out.push_str(&format!("class {id} {cls}\n"));
        }
    }
    out
}

pub fn render_status_svg(src: &str, states: &[(String, NodeState)]) -> Result<String> {
    let mmd = mermaid_with_status(src, states);
    flowmaid::render_svg(&mmd).map_err(|e| anyhow::anyhow!("render SVG: {e:?}"))
}

pub fn write_svg(path: &Path, svg: &str) -> Result<()> {
    std::fs::write(path, svg).with_context(|| format!("tulis {}", path.display()))
}

/// Preview server mini (viewer saja, bukan UI kontrol): `/` = halaman
/// auto-refresh, `/flow.svg` = SVG terkini dari memori.
pub fn serve_preview(addr: &str, svg: Arc<Mutex<String>>) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("bind {addr}"))?;
    let shown = listener.local_addr()?;
    eprintln!("🔎 preview: http://{shown} (auto-refresh 1 dtk)");
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf);
            let first = String::from_utf8_lossy(&buf);
            let (ctype, body) = if first.starts_with("GET /flow.svg") {
                ("image/svg+xml", svg.lock().map(|s| s.clone()).unwrap_or_default())
            } else {
                (
                    "text/html; charset=utf-8",
                    "<!doctype html><meta http-equiv=refresh content=1>\
                     <body style=\"background:#14161b;margin:24px\">\
                     <img src=/flow.svg style=\"max-width:100%\"></body>"
                        .to_string(),
                )
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_appends_classes_and_renders() {
        let src = "flowchart LR\n  a[Satu]:::cust --> b[Dua]\n  classDef cust fill:#3b82f6\n";
        let states = vec![("a".to_string(), NodeState::Ok), ("b".to_string(), NodeState::Current)];
        let mmd = mermaid_with_status(src, &states);
        assert!(mmd.contains("class a frOk"));
        assert!(mmd.contains("class b frCur"));
        // Harus tetap valid untuk flowmaid (classDef ganda + class assignment).
        let svg = render_status_svg(src, &states).unwrap();
        assert!(svg.contains("<svg"), "output bukan SVG");
    }
}
