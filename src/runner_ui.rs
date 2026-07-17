//! State UI bersama (dipakai mode auto & interaktif): peta status node +
//! pembaruan SVG live (file dan/atau preview server in-memory).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::diagram::{NodeState, render_status_svg, write_svg};
use crate::engine::{Outcome, StepReport};
use crate::flow::Flow;

pub struct Ui {
    mermaid_src: String,
    states: Vec<(String, NodeState)>,
    svg_path: Option<PathBuf>,
    shared_svg: Option<Arc<Mutex<String>>>,
}

impl Ui {
    pub fn new(
        flow: &Flow,
        svg_path: Option<PathBuf>,
        shared_svg: Option<Arc<Mutex<String>>>,
    ) -> Result<Self> {
        let mut ui = Ui {
            mermaid_src: flow.mermaid_src.clone(),
            states: flow
                .steps
                .iter()
                .map(|s| (s.node_id.clone(), NodeState::Idle))
                .collect(),
            svg_path,
            shared_svg,
        };
        ui.refresh()?;
        Ok(ui)
    }

    fn set(&mut self, i: usize, st: NodeState) -> Result<()> {
        if let Some(e) = self.states.get_mut(i) {
            e.1 = st;
        }
        self.refresh()
    }

    pub fn set_current(&mut self, i: usize) -> Result<()> {
        self.set(i, NodeState::Current)
    }
    pub fn set_ok(&mut self, i: usize) -> Result<()> {
        self.set(i, NodeState::Ok)
    }
    pub fn set_fail(&mut self, i: usize) -> Result<()> {
        self.set(i, NodeState::Fail)
    }
    pub fn set_skip(&mut self, i: usize) -> Result<()> {
        self.set(i, NodeState::Skip)
    }
    pub fn set_manual(&mut self, i: usize) -> Result<()> {
        self.set(i, NodeState::Manual)
    }

    fn refresh(&mut self) -> Result<()> {
        if self.svg_path.is_none() && self.shared_svg.is_none() {
            return Ok(());
        }
        let svg = render_status_svg(&self.mermaid_src, &self.states)?;
        if let Some(p) = &self.svg_path {
            write_svg(p, &svg)?;
        }
        if let Some(shared) = &self.shared_svg
            && let Ok(mut s) = shared.lock()
        {
            *s = svg;
        }
        Ok(())
    }

    /// Ringkasan akhir: (ok, fail, skip+manual, idle).
    pub fn tally(&self) -> (usize, usize, usize, usize) {
        let mut t = (0, 0, 0, 0);
        for (_, st) in &self.states {
            match st {
                NodeState::Ok => t.0 += 1,
                NodeState::Fail | NodeState::Current => t.1 += 1,
                NodeState::Skip | NodeState::Manual => t.2 += 1,
                NodeState::Idle => t.3 += 1,
            }
        }
        t
    }
}

/// JSON pretty ter-indentasi utk terminal; dipangkas pada 40 baris.
fn pretty(v: &serde_json::Value, indent: &str) -> String {
    let p = serde_json::to_string_pretty(v).unwrap_or_default();
    let mut out = String::new();
    for (i, l) in p.lines().enumerate() {
        if i >= 40 {
            out.push_str(&format!("\n{indent}\u{2026} (dipangkas)"));
            break;
        }
        out.push('\n');
        out.push_str(indent);
        out.push_str(l);
    }
    out
}

pub fn print_report(rep: &StepReport) {
    if let Some(line) = &rep.request_line {
        println!("   \u{2192} {line}");
    }
    if let Some(a) = &rep.auth_info {
        println!("   \u{1f511} auth    : {a}");
    }
    if let Some(b) = &rep.request_body {
        println!("   \u{21e2} payload :{}", pretty(b, "      "));
    }
    if let Some(c) = &rep.curl {
        println!("   $ {c}");
    }
    if matches!(rep.outcome, Outcome::Passed)
        && let Some(b) = &rep.body
    {
        println!("   \u{21e0} response:{}", pretty(b, "      "));
    }
    match &rep.outcome {
        Outcome::Passed => println!(
            "   ✅ HTTP {} ({} ms)",
            rep.http_status.unwrap_or(0),
            rep.ms
        ),
        Outcome::Failed(msg) => {
            println!("   ❌ GAGAL: {msg}");
            if let Some(code) = rep.http_status {
                println!("      HTTP {code} ({} ms)", rep.ms);
            }
            if let Some(body) = &rep.body {
                let pretty = serde_json::to_string_pretty(body).unwrap_or_default();
                let trunc: String = pretty.chars().take(1500).collect();
                println!(
                    "      body: {trunc}{}",
                    if pretty.len() > 1500 { " …" } else { "" }
                );
            }
        }
        Outcome::Skipped(reason) => println!("   ⏭  skip ({reason})"),
        Outcome::Manual => println!("   ✋ langkah manual"),
    }
    for n in &rep.notes {
        println!("      {n}");
    }
}
