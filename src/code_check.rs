//! Validasi draft teks `flow.mmd` + `flow.yaml` untuk editor kode GUI —
//! sengaja tanpa egui agar bisa di-unit-test. Hasilnya daftar diagnosa
//! per-baris (parse error) + validasi silang mmd↔yaml (step tak dikenal,
//! profil auth, format request, kondisi cabang, DAG).
//!
//! Juga helper teks mentah (`find_step_line`, `insert_step_skeleton`) —
//! editor kode menyimpan teks APA ADANYA (komentar/urutan terjaga), jadi
//! operasi apa pun di sini bekerja di level baris, bukan reserialize.

use std::collections::BTreeSet;

use crate::config::FlowConfig;
use crate::flow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Mmd,
    Yaml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diag {
    pub source: Source,
    pub severity: Severity,
    /// Baris 1-based bila diketahui.
    pub line: Option<usize>,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct Check {
    pub diags: Vec<Diag>,
    /// `flow.mmd` lolos parse sebagai flowchart/stateDiagram.
    pub mmd_ok: bool,
    /// `flow.yaml` lolos parse sebagai `FlowConfig`.
    pub yaml_ok: bool,
}

impl Check {
    /// Gagal parse (bukan sekadar validasi) — save ditolak.
    pub fn parse_failed(&self) -> bool {
        !self.mmd_ok || !self.yaml_ok
    }

    pub fn errors(&self) -> usize {
        self.diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }
}

fn diag(source: Source, severity: Severity, line: Option<usize>, message: String) -> Diag {
    Diag {
        source,
        severity,
        line,
        message,
    }
}

/// Teks yaml kosong (belum ada flow.yaml) = config default, bukan error.
pub fn parse_yaml(yaml: &str) -> Result<FlowConfig, (Option<usize>, String)> {
    if yaml.trim().is_empty() {
        return Ok(FlowConfig::default());
    }
    serde_yaml::from_str::<FlowConfig>(yaml).map_err(|e| (e.location().map(|l| l.line()), e.to_string()))
}

pub fn check(mmd: &str, yaml: &str) -> Check {
    let mut out = Check::default();

    let graph = match flowmaid::parser::parse_document(mmd) {
        Ok(flowmaid::model::Document::Flowchart(g)) | Ok(flowmaid::model::Document::State(g)) => {
            out.mmd_ok = true;
            Some(g)
        }
        Ok(_) => {
            out.diags.push(diag(
                Source::Mmd,
                Severity::Error,
                header_line(mmd),
                "diagram must be flowchart/stateDiagram".into(),
            ));
            None
        }
        Err(e) => {
            out.diags.push(diag(Source::Mmd, Severity::Error, Some(e.line), e.message));
            None
        }
    };

    let cfg = match parse_yaml(yaml) {
        Ok(c) => {
            out.yaml_ok = true;
            Some(c)
        }
        Err((line, msg)) => {
            out.diags.push(diag(Source::Yaml, Severity::Error, line, msg));
            None
        }
    };

    if let Some(g) = &graph {
        // Struktur graf (satu start, DAG) — pakai loader asli agar aturannya
        // persis sama dgn runner. Config default: step tak dikenal dilaporkan
        // terpisah di bawah lengkap dgn barisnya.
        if let Err(e) = flow::load_src(mmd.to_string(), FlowConfig::default()) {
            out.diags.push(diag(Source::Mmd, Severity::Error, None, format!("{e:#}")));
        }
        for e in &g.edges {
            let Some(label) = e.label.as_deref() else { continue };
            let t = label.trim();
            if t.is_empty() || t.eq_ignore_ascii_case("else") {
                continue;
            }
            if let Err(msg) = check_condition(t) {
                out.diags.push(diag(
                    Source::Mmd,
                    Severity::Error,
                    find_line_containing(mmd, label),
                    format!(
                        "edge {} → {}: {msg}",
                        g.nodes[e.from].id, g.nodes[e.to].id
                    ),
                ));
            }
        }
    }

    if let Some(cfg) = &cfg {
        let profiles: BTreeSet<&str> = cfg.auth_profiles.iter().map(String::as_str).collect();
        for (id, step) in &cfg.steps {
            let step_line = find_step_line(yaml, id);
            if let Some(g) = &graph
                && g.node_index(id).is_none()
            {
                out.diags.push(diag(
                    Source::Yaml,
                    Severity::Error,
                    step_line,
                    format!("steps.{id}: node '{id}' not found in flow.mmd"),
                ));
            }
            if let Some(a) = &step.auth
                && !profiles.contains(a.as_str())
            {
                let (severity, msg) = if profiles.is_empty() {
                    (
                        Severity::Warning,
                        format!("steps.{id}.auth '{a}': auth_profiles is empty — token isn't validated before the run"),
                    )
                } else {
                    (
                        Severity::Error,
                        format!("steps.{id}.auth '{a}' is not listed in auth_profiles"),
                    )
                };
                let line = find_key_in_step(yaml, id, "auth").or(step_line);
                out.diags.push(diag(Source::Yaml, severity, line, msg));
            }
            if let Some(r) = &step.request
                && let Err(msg) = check_request(r)
            {
                let line = find_key_in_step(yaml, id, "request").or(step_line);
                out.diags
                    .push(diag(Source::Yaml, Severity::Error, line, format!("steps.{id}.request: {msg}")));
            }
            if let Some(s) = &step.skip_if
                && let Err(msg) = check_condition(s)
            {
                let line = find_key_in_step(yaml, id, "skip_if").or(step_line);
                out.diags
                    .push(diag(Source::Yaml, Severity::Error, line, format!("steps.{id}.skip_if: {msg}")));
            }
        }
    }

    out
}

/// Bentuk yang diterima `engine::eval_skip_if`: `var == nilai` / `var != nilai`.
fn check_condition(expr: &str) -> Result<(), String> {
    let idx = expr
        .find("!=")
        .or_else(|| expr.find("=="))
        .ok_or_else(|| format!("condition must be `var == value`, `var != value` or `else`, got `{expr}`"))?;
    if expr[..idx].trim().is_empty() {
        return Err(format!("condition `{expr}` has no variable on the left side"));
    }
    Ok(())
}

/// Bentuk yang diterima `engine::execute`: `METHOD /path` (atau URL absolut).
fn check_request(r: &str) -> Result<(), String> {
    const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
    let Some((m, path)) = r.trim().split_once(' ') else {
        return Err(format!("must be `METHOD /path`, got `{r}`"));
    };
    if !METHODS.contains(&m) {
        return Err(format!("unknown HTTP method `{m}`"));
    }
    if path.trim().is_empty() {
        return Err("path is empty".into());
    }
    Ok(())
}

fn header_line(mmd: &str) -> Option<usize> {
    mmd.lines()
        .position(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("%%")
        })
        .map(|i| i + 1)
}

fn find_line_containing(text: &str, needle: &str) -> Option<usize> {
    text.lines().position(|l| l.contains(needle)).map(|i| i + 1)
}

fn indent_of(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// Baris kosong / komentar yaml — tak memengaruhi struktur blok.
fn is_blank(l: &str) -> bool {
    let t = l.trim();
    t.is_empty() || t.starts_with('#')
}

/// `key:` di awal baris (setelah indent), juga versi ber-quote.
fn line_has_key(l: &str, key: &str) -> bool {
    let t = l.trim_start();
    [key.to_string(), format!("\"{key}\""), format!("'{key}'")]
        .iter()
        .any(|k| t.strip_prefix(k.as_str()).is_some_and(|rest| rest.trim_start().starts_with(':')))
}

/// Indeks baris (0-based) `steps:` top-level.
fn steps_header(lines: &[&str]) -> Option<usize> {
    lines
        .iter()
        .position(|l| indent_of(l) == 0 && line_has_key(l, "steps"))
}

/// Rentang anak blok `steps:` → (indent anak, indeks baris anak terakhir yang
/// tidak kosong). `None` indent bila blok belum punya anak.
fn steps_block(lines: &[&str], header: usize) -> (Option<usize>, usize) {
    let mut child_indent = None;
    let mut last = header;
    for (i, l) in lines.iter().enumerate().skip(header + 1) {
        if is_blank(l) {
            continue;
        }
        let ind = indent_of(l);
        if ind == 0 {
            break;
        }
        child_indent.get_or_insert(ind);
        last = i;
    }
    (child_indent, last)
}

/// Baris (1-based) `<id>:` di bawah `steps:` — basis "lompat ke config node".
pub fn find_step_line(yaml: &str, id: &str) -> Option<usize> {
    let lines: Vec<&str> = yaml.lines().collect();
    let header = steps_header(&lines)?;
    let (ci, last) = steps_block(&lines, header);
    let ci = ci?;
    (header + 1..=last)
        .find(|&i| indent_of(lines[i]) == ci && line_has_key(lines[i], id))
        .map(|i| i + 1)
}

/// Baris (1-based) `<key>:` di dalam `steps.<id>`.
fn find_key_in_step(yaml: &str, id: &str, key: &str) -> Option<usize> {
    let lines: Vec<&str> = yaml.lines().collect();
    let start = find_step_line(yaml, id)? - 1;
    let step_indent = indent_of(lines[start]);
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        if is_blank(l) {
            continue;
        }
        if indent_of(l) <= step_indent {
            break;
        }
        if line_has_key(l, key) {
            return Some(i + 1);
        }
    }
    None
}

/// Tambahkan kerangka `steps.<id>` ke teks yaml mentah (komentar & format
/// lain tak tersentuh). → (teks baru, baris 1-based entry). `None` bila
/// `steps` ditulis inline non-kosong (`steps: { a: ... }`) — edit manual.
pub fn insert_step_skeleton(yaml: &str, id: &str) -> Option<(String, usize)> {
    let mut lines: Vec<String> = yaml.lines().map(str::to_string).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let (insert_at, ci) = match steps_header(&refs) {
        Some(h) => {
            let after = refs[h].split_once(':').map(|(_, r)| r.trim()).unwrap_or("");
            let after = after.split('#').next().unwrap_or("").trim();
            if after == "{}" {
                lines[h] = "steps:".into();
                (h + 1, 2)
            } else if !after.is_empty() {
                return None;
            } else {
                let (ci, last) = steps_block(&refs, h);
                (last + 1, ci.unwrap_or(2))
            }
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("steps:".into());
            (lines.len(), 2)
        }
    };
    let pad = " ".repeat(ci);
    let entry = [
        format!("{pad}{id}:"),
        format!("{pad}{pad}# request: GET /api/..."),
        format!("{pad}{pad}# auth: <profile>"),
        format!("{pad}{pad}note: TODO"),
    ];
    for (k, l) in entry.into_iter().enumerate() {
        lines.insert(insert_at + k, l);
    }
    let mut text = lines.join("\n");
    text.push('\n');
    Some((text, insert_at + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MMD: &str = "flowchart TD\n  %% komentar\n  a --> b\n  b -->|mode == x| c\n  b -->|else| d\n";
    const YAML: &str = "# komentar atas\nauth_profiles: [customer]\n\nsteps:\n  a:\n    auth: customer\n    request: GET /x\n\n  # komentar step\n  c:\n    request: POST /y\nvars: { mode: x }\n";

    #[test]
    fn valid_draft_has_no_diags() {
        let c = check(MMD, YAML);
        assert!(c.mmd_ok && c.yaml_ok);
        assert!(c.diags.is_empty(), "{:?}", c.diags);
    }

    #[test]
    fn empty_yaml_is_default_config() {
        let c = check(MMD, "  \n");
        assert!(c.yaml_ok && c.diags.is_empty(), "{:?}", c.diags);
    }

    #[test]
    fn mmd_parse_error_has_line() {
        let c = check("flowchart TD\n  a --> b\n  c[[[ \n", "");
        assert!(!c.mmd_ok && c.parse_failed());
        assert_eq!(c.diags[0].source, Source::Mmd);
        assert!(c.diags[0].line.is_some(), "{:?}", c.diags);
    }

    #[test]
    fn yaml_parse_error_has_line() {
        let c = check(MMD, "steps:\n  a:\n    request: [unclosed\n");
        assert!(!c.yaml_ok);
        let d = &c.diags[0];
        assert_eq!(d.source, Source::Yaml);
        assert!(d.line.is_some_and(|l| l >= 3), "{d:?}");
    }

    #[test]
    fn cross_checks_report_lines() {
        let yaml = "auth_profiles: [customer]\nsteps:\n  a:\n    auth: owner\n    request: FETCH /x\n  zz:\n    skip_if: mode\n";
        let c = check(MMD, yaml);
        assert!(!c.parse_failed());
        let find = |s: &str| c.diags.iter().find(|d| d.message.contains(s)).unwrap_or_else(|| panic!("{s}: {:?}", c.diags));
        assert_eq!(find("not listed in auth_profiles").line, Some(4));
        assert_eq!(find("unknown HTTP method").line, Some(5));
        assert_eq!(find("node 'zz' not found").line, Some(6));
        assert_eq!(find("skip_if").line, Some(7));
    }

    #[test]
    fn auth_without_profiles_is_warning() {
        let c = check(MMD, "steps:\n  a:\n    auth: customer\n");
        assert_eq!(c.diags.len(), 1);
        assert_eq!(c.diags[0].severity, Severity::Warning);
    }

    #[test]
    fn bad_edge_condition_and_cycle() {
        let c = check("flowchart TD\n  a -->|oops| b\n", "");
        let d = c.diags.iter().find(|d| d.message.contains("condition")).unwrap();
        assert_eq!(d.line, Some(2));
        let c = check("flowchart TD\n  a --> b\n  b --> c\n  c --> b\n", "");
        assert!(c.diags.iter().any(|d| d.message.contains("cyclic")), "{:?}", c.diags);
        assert!(!c.parse_failed()); // validasi gagal ≠ parse gagal
    }

    #[test]
    fn find_step_line_skips_comments_and_nested_keys() {
        assert_eq!(find_step_line(YAML, "a"), Some(5));
        assert_eq!(find_step_line(YAML, "c"), Some(10));
        assert_eq!(find_step_line(YAML, "request"), None);
        assert_eq!(find_step_line(YAML, "nope"), None);
    }

    #[test]
    fn skeleton_appends_inside_steps_and_keeps_comments() {
        let (t, line) = insert_step_skeleton(YAML, "d").unwrap();
        assert!(t.contains("# komentar atas") && t.contains("# komentar step"));
        assert_eq!(find_step_line(&t, "d"), Some(line));
        // tetap sebelum `vars:` (masih di dalam blok steps) & yaml tetap valid
        let cfg = parse_yaml(&t).unwrap();
        assert!(cfg.steps.contains_key("d"));
        assert_eq!(cfg.vars.len(), 1);
    }

    #[test]
    fn skeleton_handles_missing_and_empty_steps() {
        let (t, _) = insert_step_skeleton("auth_profiles: [x]\n", "a").unwrap();
        assert!(parse_yaml(&t).unwrap().steps.contains_key("a"));
        let (t, _) = insert_step_skeleton("steps: {}\n", "a").unwrap();
        assert!(parse_yaml(&t).unwrap().steps.contains_key("a"));
        assert!(insert_step_skeleton("steps: { b: {} }\n", "a").is_none());
    }
}
