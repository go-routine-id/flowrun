//! Mutasi struktural atas `flowmaid::model::Graph` yang belum disediakan
//! flowmaid sendiri (cuma ada `ensure_node`/`add_edge`, tidak ada remove).
//!
//! PENTING: `Graph` punya field privat `index: HashMap<String, usize>` yang
//! dipakai `ensure_node`/`node_index`. Fungsi di modul ini cuma bisa memutasi
//! field publik (`nodes`/`edges`) — sesudah `remove_node` menggeser posisi
//! node, `index` jadi basi dan `node_index()` akan salah. Caller WAJIB
//! serialize→reparse (`flowmaid::to_mermaid` lalu `parser::parse_document`)
//! sebelum lookup `node_index()` berikutnya — jangan pernah memanggilnya di
//! antara mutasi dan reparse.

use flowmaid::model::Graph;

/// Keyword mermaid yang bentrok dengan statement-leading (dicek
/// case-insensitive) — sama dengan yang dihindari `flowmaid::emit`, dijaga
/// ulang di sini supaya `.mmd` hasil edit tetap mermaid standar yang valid
/// dirender GitHub/editor lain (lihat CONTEXT.md).
const RESERVED_IDS: &[&str] = &[
    "end", "subgraph", "direction", "style", "classdef", "class", "flowchart", "graph", "linkstyle", "click",
];

/// Hapus node di index `idx`: buang semua edge yang menyentuhnya, lalu
/// geser index `from`/`to` edge yang tersisa. Lihat catatan modul soal
/// `index` privat — panggil reparse sebelum `node_index()` berikutnya.
pub fn remove_node(g: &mut Graph, idx: usize) {
    g.nodes.remove(idx);
    g.edges.retain(|e| e.from != idx && e.to != idx);
    for e in &mut g.edges {
        if e.from > idx {
            e.from -= 1;
        }
        if e.to > idx {
            e.to -= 1;
        }
    }
}

/// Hapus edge di index `idx`. Edge tidak direferensikan by-index oleh
/// struktur lain, jadi tidak perlu reindex apa pun.
pub fn remove_edge(g: &mut Graph, idx: usize) {
    g.edges.remove(idx);
}

/// Validasi id node baru/hasil rename: non-kosong, ASCII alnum + `_`/`-`,
/// bukan keyword reserved mermaid, dan tidak bentrok node lain (`exclude` =
/// index node yang sedang di-rename, supaya dia tidak dianggap bentrok
/// dengan dirinya sendiri).
pub fn validate_node_id(id: &str, g: &Graph, exclude: Option<usize>) -> anyhow::Result<()> {
    let id = id.trim();
    if id.is_empty() {
        anyhow::bail!("id node tidak boleh kosong");
    }
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        anyhow::bail!("id node '{id}' cuma boleh huruf/angka/underscore/hyphen");
    }
    if RESERVED_IDS.contains(&id.to_lowercase().as_str()) {
        anyhow::bail!("id node '{id}' adalah keyword mermaid reserved");
    }
    if let Some(i) = g.node_index(id) {
        if Some(i) != exclude {
            anyhow::bail!("id node '{id}' sudah dipakai node lain");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowmaid::model::{Document, EdgeKind};

    fn chain(n: usize) -> Graph {
        let mut g = Graph::default();
        for i in 0..n {
            g.ensure_node(&format!("n{i}"), None, None);
        }
        for i in 0..n.saturating_sub(1) {
            g.add_edge(i, i + 1, None, EdgeKind::Arrow);
        }
        g
    }

    fn ids(g: &Graph) -> Vec<&str> {
        g.nodes.iter().map(|n| n.id.as_str()).collect()
    }
    fn edges(g: &Graph) -> Vec<(usize, usize)> {
        g.edges.iter().map(|e| (e.from, e.to)).collect()
    }

    #[test]
    fn remove_start_node() {
        let mut g = chain(4); // n0->n1->n2->n3
        remove_node(&mut g, 0);
        assert_eq!(ids(&g), vec!["n1", "n2", "n3"]);
        assert_eq!(edges(&g), vec![(0, 1), (1, 2)]); // n1->n2->n3 reindexed
    }

    #[test]
    fn remove_last_node() {
        let mut g = chain(4);
        remove_node(&mut g, 3);
        assert_eq!(ids(&g), vec!["n0", "n1", "n2"]);
        assert_eq!(edges(&g), vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn remove_middle_node_with_in_and_out_edges() {
        let mut g = chain(4); // n0->n1->n2->n3
        remove_node(&mut g, 2); // n2 has in-edge (n1->n2) and out-edge (n2->n3)
        assert_eq!(ids(&g), vec!["n0", "n1", "n3"]);
        // n1->n2 and n2->n3 both vanish; n0->n1 survives, indices >2 shift down
        assert_eq!(edges(&g), vec![(0, 1)]);
    }

    #[test]
    fn remove_node_spanning_edge_unaffected_endpoints_reindexed() {
        // n0->n1->n2->n3, plus a "spanning" edge n0->n3 that doesn't touch n2.
        let mut g = chain(4);
        g.add_edge(0, 3, None, EdgeKind::Arrow);
        remove_node(&mut g, 2); // removes n1->n2, n2->n3; n0->n1 and n0->n3(->n2) survive & reindex
        assert_eq!(ids(&g), vec!["n0", "n1", "n3"]);
        assert_eq!(edges(&g), vec![(0, 1), (0, 2)]); // n0->n1 unchanged, n0->n3 becomes n0->n2 (index shifted)
    }

    #[test]
    fn remove_node_with_no_edges() {
        let mut g = chain(1);
        remove_node(&mut g, 0);
        assert!(ids(&g).is_empty());
        assert!(edges(&g).is_empty());
    }

    #[test]
    fn remove_node_twice_in_sequence() {
        let mut g = chain(4); // n0->n1->n2->n3
        remove_node(&mut g, 1); // remove n1: left n0, n2, n3; edges n2->n3 only (0,1 after shift... wait check)
        remove_node(&mut g, 1); // remove what is now index 1 again
        // After first removal: nodes = [n0, n2, n3], edges: n0->n1 and n1->n2 touched n1 -> gone;
        // n2->n3 survives, from=2,to=3 -> shift down by 1 (both >1) -> (1,2)
        assert_eq!(ids(&g), vec!["n0", "n3"]);
        // second removal at idx 1 (n3 in current list is at index... let's just assert no edges/panics
        assert!(edges(&g).is_empty());
    }

    #[test]
    fn remove_edge_at_start_middle_end() {
        let mut g = chain(4); // edges: (0,1) (1,2) (2,3)
        remove_edge(&mut g, 1); // remove (1,2)
        assert_eq!(edges(&g), vec![(0, 1), (2, 3)]);
        remove_edge(&mut g, 1); // remove (2,3) (now at index 1)
        assert_eq!(edges(&g), vec![(0, 1)]);
        remove_edge(&mut g, 0);
        assert!(edges(&g).is_empty());
        // nodes untouched by edge removal
        assert_eq!(ids(&g), vec!["n0", "n1", "n2", "n3"]);
    }

    #[test]
    fn remove_node_then_round_trip_does_not_panic() {
        let mut g = chain(5);
        remove_node(&mut g, 2);
        let mmd = flowmaid::to_mermaid(&g);
        let doc = flowmaid::parser::parse_document(&mmd).expect("reparse hasil to_mermaid");
        let reparsed = match doc {
            Document::Flowchart(g) | Document::State(g) => g,
            _ => panic!("harap flowchart"),
        };
        assert_eq!(reparsed.nodes.len(), g.nodes.len());
        assert_eq!(reparsed.edges.len(), g.edges.len());
        // index privat sudah dibangun ulang benar oleh reparse — lookup aman.
        assert!(reparsed.node_index("n0").is_some());
        assert!(reparsed.node_index("n2").is_none()); // node lama yg dihapus
    }

    #[test]
    fn validate_id_rules() {
        let g = chain(2); // n0, n1
        assert!(validate_node_id("n2", &g, None).is_ok());
        assert!(validate_node_id("", &g, None).is_err());
        assert!(validate_node_id("has space", &g, None).is_err());
        assert!(validate_node_id("End", &g, None).is_err()); // reserved, case-insensitive
        assert!(validate_node_id("n0", &g, None).is_err()); // dupe
        assert!(validate_node_id("n0", &g, Some(0)).is_ok()); // rename diri sendiri OK
    }
}
