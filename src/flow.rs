//! Gabungkan graf `.mmd` (via flowmaid) dengan sidecar `flow.yaml` menjadi
//! urutan langkah tereksekusi. Visual dan runner sengaja dipisah: `.mmd` murni
//! mermaid (bisa dirender di mana pun), config eksekusi hidup di sidecar.

use std::path::Path;

use anyhow::{bail, Context, Result};
use flowmaid::model::Document;

use crate::config::{FlowConfig, StepConfig};

/// Satu langkah tereksekusi, urut sesuai jalur graf.
#[derive(Debug, Clone)]
pub struct FlowStep {
    pub node_id: String,
    pub title: String,
    pub cfg: StepConfig,
    /// true bila node tak punya entry di sidecar → langkah manual/visual-only.
    pub unconfigured: bool,
}

#[derive(Debug)]
pub struct Flow {
    /// Teks mermaid asli (dipakai diagram.rs untuk regen + pewarnaan status).
    pub mermaid_src: String,
    pub steps: Vec<FlowStep>,
}

/// Parse `.mmd` → jalur linear → merge sidecar.
///
/// MVP v0.1: jalur linear (tiap node maksimal satu outgoing edge). Branching /
/// edge kondisional menyusul di v0.2 — parser flowmaid sudah menyediakan graf
/// penuh, pembatasan ini murni di sisi flowrun.
pub fn load(mmd_path: &Path, cfg: FlowConfig) -> Result<Flow> {
    let src = std::fs::read_to_string(mmd_path)
        .with_context(|| format!("baca {}", mmd_path.display()))?;
    let graph = match flowmaid::parser::parse_document(&src)
        .map_err(|e| anyhow::anyhow!("parse mermaid {}: {e:?}", mmd_path.display()))?
    {
        Document::Flowchart(g) | Document::State(g) => g,
        other => bail!(
            "diagram harus flowchart/stateDiagram, dapat {:?}",
            std::mem::discriminant(&other)
        ),
    };
    if graph.nodes.is_empty() {
        bail!("flow kosong: tidak ada node di {}", mmd_path.display());
    }

    // Urutan eksekusi: mulai dari node tanpa incoming edge, ikuti outgoing.
    let n = graph.nodes.len();
    let mut indegree = vec![0usize; n];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in &graph.edges {
        indegree[e.to] += 1;
        out[e.from].push(e.to);
    }
    let starts: Vec<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    let start = match starts.as_slice() {
        [s] => *s,
        [] => bail!("graf siklik: tidak ada node awal (semua punya incoming edge)"),
        many => bail!(
            "lebih dari satu node awal ({}) — v0.1 butuh jalur linear tunggal",
            many.iter().map(|&i| graph.nodes[i].id.as_str()).collect::<Vec<_>>().join(", ")
        ),
    };

    let mut order = Vec::with_capacity(n);
    let mut cur = start;
    let mut visited = vec![false; n];
    loop {
        if visited[cur] {
            bail!("graf siklik terdeteksi di node '{}'", graph.nodes[cur].id);
        }
        visited[cur] = true;
        order.push(cur);
        match out[cur].as_slice() {
            [] => break,
            [next] => cur = *next,
            many => bail!(
                "node '{}' bercabang ke {} node — branching belum didukung v0.1",
                graph.nodes[cur].id,
                many.len()
            ),
        }
    }

    // Validasi sidecar: semua step di yaml harus ada di graf.
    let unknown: Vec<&String> = cfg
        .steps
        .keys()
        .filter(|id| graph.node_index(id).is_none())
        .collect();
    if !unknown.is_empty() {
        bail!(
            "step di flow.yaml tidak ada di flow.mmd: {}",
            unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        );
    }

    let steps = order
        .into_iter()
        .map(|i| {
            let node = &graph.nodes[i];
            let (step_cfg, unconfigured) = match cfg.steps.get(&node.id) {
                Some(c) => (c.clone(), false),
                None => (StepConfig { manual: true, ..Default::default() }, true),
            };
            FlowStep {
                node_id: node.id.clone(),
                title: node.label.clone(),
                cfg: step_cfg,
                unconfigured,
            }
        })
        .collect();

    Ok(Flow { mermaid_src: src, steps })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        // Nama unik per test — test cargo jalan paralel dalam satu proses,
        // jadi PID saja tidak cukup (kolisi tulis/hapus antar test).
        p.push(format!("flowrun-test-{}-{}.mmd", std::process::id(), name));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn linear_order_and_sidecar_merge() {
        let p = write_tmp(
            "linear",
            "flowchart LR\n  a[Langkah A]:::cust --> b[Langkah B]:::ownr\n  b --> c[Langkah C]\n  classDef cust fill:#3b82f6\n  classDef ownr fill:#f59e0b\n",
        );
        let mut cfg = FlowConfig::default();
        cfg.steps.insert("b".into(), StepConfig { request: Some("GET /x".into()), ..Default::default() });
        let flow = load(&p, cfg).unwrap();
        std::fs::remove_file(&p).ok();
        let ids: Vec<&str> = flow.steps.iter().map(|s| s.node_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert!(flow.steps[0].unconfigured && flow.steps[0].cfg.manual);
        assert!(!flow.steps[1].unconfigured);
        assert_eq!(flow.steps[1].cfg.request.as_deref(), Some("GET /x"));
        assert_eq!(flow.steps[0].title, "Langkah A");
    }

    #[test]
    fn rejects_branching() {
        let p = write_tmp("branching", "flowchart LR\n  a --> b\n  a --> c\n");
        let err = load(&p, FlowConfig::default()).unwrap_err().to_string();
        std::fs::remove_file(&p).ok();
        assert!(err.contains("bercabang"), "{err}");
    }

    #[test]
    fn rejects_unknown_sidecar_step() {
        let p = write_tmp("unknown-step", "flowchart LR\n  a --> b\n");
        let mut cfg = FlowConfig::default();
        cfg.steps.insert("zzz".into(), StepConfig::default());
        let err = load(&p, cfg).unwrap_err().to_string();
        std::fs::remove_file(&p).ok();
        assert!(err.contains("zzz"), "{err}");
    }
}
