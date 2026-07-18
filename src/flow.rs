//! Gabungkan graf `.mmd` (via flowmaid) dengan sidecar `flow.yaml` menjadi
//! graf langkah tereksekusi (DAG). Visual dan runner sengaja dipisah: `.mmd`
//! murni mermaid (bisa dirender di mana pun), config eksekusi di sidecar.
//!
//! v0.2: mendukung PERCABANGAN. Kondisi cabang ditulis sebagai label edge
//! mermaid — `a -->|pay_mode == cod| b` — dievaluasi engine atas context vars.
//! Label `else` / tanpa label = fallback. Runner menyusuri SATU jalur aktif.

use std::path::Path;

use anyhow::{Context, Result, bail};
use flowmaid::model::Document;

use crate::config::{FlowConfig, StepConfig};

/// Satu langkah tereksekusi.
#[derive(Debug, Clone)]
pub struct FlowStep {
    pub node_id: String,
    pub title: String,
    pub cfg: StepConfig,
    /// true bila node tak punya entry di sidecar → langkah manual/visual-only.
    pub unconfigured: bool,
}

/// Edge graf. `cond=None` = tanpa syarat / fallback (`|else|` juga jadi None).
#[derive(Debug, Clone)]
pub struct FlowEdge {
    pub from: usize,
    pub to: usize,
    pub cond: Option<String>,
    /// Label asli utk ditampilkan (kondisi atau "else").
    pub label: Option<String>,
}

#[derive(Debug)]
pub struct Flow {
    /// Teks mermaid asli (dipakai renderer untuk regen + pewarnaan status).
    pub mermaid_src: String,
    /// Semua node, urut topologis (stabil untuk tampilan).
    pub steps: Vec<FlowStep>,
    pub edges: Vec<FlowEdge>,
    /// Indeks node awal (indegree 0, tunggal).
    pub start: usize,
    /// true bila tiap node maksimal satu outgoing edge (rantai murni) —
    /// menentukan kelayakan layout serpentine dsb.
    pub linear: bool,
}

impl Flow {
    pub fn outgoing(&self, i: usize) -> Vec<&FlowEdge> {
        self.edges.iter().filter(|e| e.from == i).collect()
    }
}

/// Parse `.mmd` → DAG + urutan topologis → merge sidecar.
pub fn load(mmd_path: &Path, cfg: FlowConfig) -> Result<Flow> {
    let src = std::fs::read_to_string(mmd_path)
        .with_context(|| format!("read {}", mmd_path.display()))?;
    let graph = match flowmaid::parser::parse_document(&src)
        .map_err(|e| anyhow::anyhow!("parse mermaid {}: {e:?}", mmd_path.display()))?
    {
        Document::Flowchart(g) | Document::State(g) => g,
        other => bail!(
            "diagram must be flowchart/stateDiagram, got {:?}",
            std::mem::discriminant(&other)
        ),
    };
    if graph.nodes.is_empty() {
        bail!("empty flow: no nodes in {}", mmd_path.display());
    }

    let n = graph.nodes.len();
    let mut indegree = vec![0usize; n];
    for e in &graph.edges {
        indegree[e.to] += 1;
    }
    let starts: Vec<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    let start_g = match starts.as_slice() {
        [s] => *s,
        [] => bail!("cyclic graph: no start node (all have an incoming edge)"),
        many => bail!(
            "more than one start node ({}) — a flow needs a single entry point",
            many.iter()
                .map(|&i| graph.nodes[i].id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    // Urutan topologis (Kahn) — stabil utk penomoran tampilan; sekaligus
    // deteksi siklus (runner v0.2 = DAG; loop/retry visual belum didukung).
    let mut indeg = indegree.clone();
    let mut queue: Vec<usize> = vec![start_g];
    let mut topo: Vec<usize> = Vec::with_capacity(n);
    while let Some(u) = queue.pop() {
        topo.push(u);
        for e in graph.edges.iter().filter(|e| e.from == u) {
            indeg[e.to] -= 1;
            if indeg[e.to] == 0 {
                queue.insert(0, e.to);
            }
        }
    }
    if topo.len() != n {
        let stuck: Vec<&str> = (0..n)
            .filter(|i| !topo.contains(i))
            .map(|i| graph.nodes[i].id.as_str())
            .collect();
        bail!(
            "cyclic graph detected (node: {}) — only DAGs are supported",
            stuck.join(", ")
        );
    }

    // Validasi sidecar: semua step di yaml harus ada di graf.
    let unknown: Vec<&String> = cfg
        .steps
        .keys()
        .filter(|id| graph.node_index(id).is_none())
        .collect();
    if !unknown.is_empty() {
        bail!(
            "step in flow.yaml not found in flow.mmd: {}",
            unknown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // graph-index → steps-index (posisi topologis).
    let mut pos = vec![0usize; n];
    for (k, &gi) in topo.iter().enumerate() {
        pos[gi] = k;
    }

    let steps: Vec<FlowStep> = topo
        .iter()
        .map(|&gi| {
            let node = &graph.nodes[gi];
            let (step_cfg, unconfigured) = match cfg.steps.get(&node.id) {
                Some(c) => (c.clone(), false),
                None => (
                    StepConfig {
                        manual: true,
                        ..Default::default()
                    },
                    true,
                ),
            };
            FlowStep {
                node_id: node.id.clone(),
                title: node.label.clone(),
                cfg: step_cfg,
                unconfigured,
            }
        })
        .collect();

    let edges: Vec<FlowEdge> = graph
        .edges
        .iter()
        .map(|e| {
            let label = e.label.clone();
            let cond = label.as_ref().and_then(|l| {
                let t = l.trim();
                if t.is_empty() || t.eq_ignore_ascii_case("else") {
                    None // fallback
                } else {
                    Some(t.to_string())
                }
            });
            FlowEdge {
                from: pos[e.from],
                to: pos[e.to],
                cond,
                label,
            }
        })
        .collect();

    let mut outdeg = vec![0usize; n];
    for e in &edges {
        outdeg[e.from] += 1;
    }
    let linear = outdeg.iter().all(|&d| d <= 1);

    Ok(Flow {
        mermaid_src: src,
        steps,
        edges,
        start: pos[start_g],
        linear,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        // Nama unik per test — test cargo jalan paralel dalam satu proses.
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
        cfg.steps.insert(
            "b".into(),
            StepConfig {
                request: Some("GET /x".into()),
                ..Default::default()
            },
        );
        let flow = load(&p, cfg).unwrap();
        std::fs::remove_file(&p).ok();
        let ids: Vec<&str> = flow.steps.iter().map(|s| s.node_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert!(flow.linear);
        assert_eq!(flow.start, 0);
        assert!(flow.steps[0].unconfigured && flow.steps[0].cfg.manual);
        assert_eq!(flow.steps[1].cfg.request.as_deref(), Some("GET /x"));
    }

    #[test]
    fn branching_loads_with_conditions() {
        let p = write_tmp(
            "branch",
            "flowchart TD\n  a --> b\n  b -->|mode == x| c\n  b -->|else| d\n  c --> e\n  d --> e\n",
        );
        let flow = load(&p, FlowConfig::default()).unwrap();
        std::fs::remove_file(&p).ok();
        assert!(!flow.linear);
        let b = flow.steps.iter().position(|s| s.node_id == "b").unwrap();
        let outs = flow.outgoing(b);
        assert_eq!(outs.len(), 2);
        let conds: Vec<Option<&str>> = outs.iter().map(|e| e.cond.as_deref()).collect();
        assert!(conds.contains(&Some("mode == x")));
        assert!(conds.contains(&None)); // else → fallback
    }

    #[test]
    fn rejects_cycle() {
        let p = write_tmp("cycle", "flowchart LR\n  a --> b\n  b --> c\n  c --> b\n");
        let err = load(&p, FlowConfig::default()).unwrap_err().to_string();
        std::fs::remove_file(&p).ok();
        assert!(err.contains("cyclic"), "{err}");
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
