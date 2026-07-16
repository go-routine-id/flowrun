# CONTEXT.md — handoff untuk melanjutkan pengembangan flowrun

> Dokumen ini ditulis agar sesi Claude (atau engineer) berikutnya bisa lanjut
> tanpa menggali dari nol. Update dokumen ini setiap kali ada keputusan/fitur besar.
> Terakhir diperbarui: 16 Jul 2026 (v0.2.0).

## Apa ini & filosofi

**flowrun** = framework flow-runner dinamis: definisikan alur API multi-langkah
sebagai graf, jalankan *next-next* (demo/manual) atau `--auto` (smoke-test/CI,
exit code ≠ 0 saat gagal). Lahir dari kebutuhan menguji alur order WACCA
(customer ↔ owner) end-to-end secara visual.

Tiga pemisahan yang DISENGAJA (jangan dicampur lagi):
1. **Visual** = mermaid murni (`flow.mmd`) — dirender [flowmaid](https://crates.io/crates/flowmaid) (crate Rust zero-dep). File `.mmd` harus tetap valid mermaid standar (bisa dirender GitHub/editor mana pun).
2. **Eksekusi** = sidecar `flow.yaml` (config per node-id: request/auth/capture/assert) + **env file** per-deployment (`dev.yaml`, gitignored — token/base_url TIDAK PERNAH di-commit).
3. **Engine** (`src/engine.rs`) terpisah dari UI — dipakai CLI & GUI lewat lib (`src/lib.rs`).

## Status saat ini (v0.2.0, tag ada di repo)

| Fitur | Status |
|---|---|
| CLI `flowrun run` interaktif (Enter/s/r/vars/q) + `--auto` (stop-on-fail, exit≠0) + `render` | ✅ |
| Templating `{{var}}`, capture dot-path, assert (status class + equality), `skip_if` | ✅ |
| SVG status live (`--svg`) + preview server mini (`--serve`) | ✅ |
| **Desktop egui** (`flowrun-gui`, feature `gui`): picker file + recent + editor koneksi/token/vars di UI, canvas pan/zoom/follow, panah, label wrap+fit, pulse node aktif, badge role, log panel, JSON tree, copy, "⟳ Hit node ini" (re-run per node) | ✅ |
| Layout: 🐍 **Ular/serpentine** (default utk rantai linear — flow panjang muat layar), ⇉ LR, ⇊ TD | ✅ |
| **Percabangan (v0.2)**: kondisi = label edge mermaid (`-->\|pay_mode == cod\|`, `\|else\|` = fallback); GUI modal pilih cabang saat ambigu; `--auto` gagal deterministik; node tak dilalui diredupkan | ✅ |
| Unit test 15/15 (parser, engine, cabang, mock HTTP via std TcpListener) | ✅ |

**Belum pernah diuji ke backend nyata** — semua demo memakai mock
(`tools/wacca_mock.py`). Blocker: butuh 2 JWT (customer + owner) environment dev
WACCA; owner login via Google OAuth sehingga tak bisa diambil otomatis.
Base URL dev WACCA bisa dilihat di `.env.dev` repo wacca-mobile (via VPN internal).

## Struktur repo

```
src/lib.rs            # re-export modul core (dipakai kedua bin)
src/config.rs         # schema flow.yaml + env file (serde_yaml)
src/flow.rs           # parse .mmd via flowmaid → DAG (steps topo-order, edges+cond, start, linear)
src/engine.rs         # runner: template/capture/assert/skip_if + choose_next (cabang)
src/diagram.rs        # regen mermaid + classDef status → render_svg; preview server
src/runner_ui.rs      # state UI CLI (SVG live + print report)
src/interactive.rs    # loop next-next CLI (graph-walk, tanya cabang bernomor)
src/main.rs           # CLI clap (run/render, --auto deterministik)
src/bin/flowrun_gui.rs# desktop egui (semua UI; worker thread graph-walker)
examples/wacca-order/          # contoh linear 14 langkah (+ mock-env.yaml utk mock)
examples/wacca-order-branched/ # contoh bercabang (keputusan/terima-tolak, cod/digital)
tools/wacca_mock.py            # mock backend wacca (python stdlib) utk dev UI/demo
```

## Cara kerja cepat (dev)

```sh
cargo test                                   # 15 unit test
python3 tools/wacca_mock.py 18923 &          # mock backend (BUGGY=1 = simulasi regresi)
cargo run -- run -f examples/wacca-order/flow.mmd -c examples/wacca-order/flow.yaml \
  -e examples/wacca-order/mock-env.yaml --auto            # CLI e2e vs mock
cargo run --features gui --bin flowrun-gui -- \
  -f examples/wacca-order-branched/flow.mmd -c examples/wacca-order-branched/flow.yaml \
  -e examples/wacca-order/mock-env.yaml                   # desktop
```
Jalur cabang diganti tanpa mengubah graf: `--var keputusan=tolak`, `--var pay_mode=digital`.

## Keputusan desain penting (dan alasannya — jangan diulang debatnya)

- **Rust native, bukan web**: keputusan owner. Bonus: tanpa CORS (bukan fetch browser), `--auto` = test CI satu binary.
- **flowmaid sebagai library**: parse (`parser::parse_document` → `Document::Flowchart(Graph)`), layout+geometri (`scene::scene(&Graph)` → `SceneNode{x,y,w,h}`, `SceneEdge{bezier/waypoints}`), `render_svg`. `Graph.direction` bisa dioverride (LR/TD) sebelum `scene()`. Versi terpasang 0.17.0.
- **Lisensi GPL-3.0-or-later** — konsekuensi link ke flowmaid (GPL). Sadari ini bila mau relicense.
- **Layout Ular** = milik flowrun, BUKAN flowmaid: layout berlapis (Sugiyama — flowmaid & mermaid.js sama) merender rantai linear sebagai pita panjang; serpentine melipatnya agar muat layar. Hanya valid utk graf linear (`Flow.linear`) — otomatis disembunyikan utk graf bercabang.
- **Semantik cabang**: kondisi di label edge (`var == nilai` / `!=`; `else`/kosong = fallback). Tepat satu true → jalan; ambigu → GUI bertanya, `--auto` FAIL (test tak boleh menebak). Satu jalur aktif; **fan-out paralel sengaja ditunda** (merge context vars ribet, kebutuhan belum nyata).
- **Graf wajib DAG** — siklus ditolak saat load (retry/loop = ranah engine, bukan graf).
- **Fail-fast token kosong** — token profil kosong dulunya lolos diam-diam sbg `Bearer ` (ditemukan saat demo); kini error sebelum flow jalan.
- **Node tanpa entry di flow.yaml** = langkah manual/visual-only (pause di interaktif, dilewati di auto). Dipakai utk langkah eksternal (mis. "Bayar QR/VA di gateway").

## Gotcha teknis (sudah kejeblos, jangan ulang)

- egui 0.29: `id_salt` (bukan `id_source`), `Rounding::same`, `smooth_scroll_delta`. eframe default-features off + `glow`,`default_fonts`. rfd 0.15 utk file dialog (ikut feature `gui`).
- reqwest 0.13: fitur TLS berubah — cukup `blocking,json` (rustls default). Blocking client TIDAK boleh dalam runtime tokio (kita tak pakai tokio sama sekali).
- Font label GUI HARUS mengikuti zoom penuh + fit-check (jangan kasih floor px — pernah bikin teks luber keluar node saat zoom-out).
- Test file temp wajib nama unik per test (cargo test paralel satu proses).
- `LayoutJob::simple` + `wrap.max_rows` + `halign=Center`, gambar via `painter.galley(pos, galley, color)` dgn pos = center − size/2.
- flowmaid `scene.nodes[i]` SEJAJAR `graph.nodes[i]` (map id → step via indeks).

## Backlog (urutan saran)

1. **Bundle .app macOS** (icon + `cargo bundle`/skrip) — DITUNDA eksplisit oleh owner, kerjakan bila diminta.
2. **Uji ke backend nyata** — tinggal isi env (2 JWT + uuid seed); `flow.yaml` wacca sudah sesuai kontrak backend (diverifikasi dari source wacca-service Jul 2026).
3. Report **junit/json** untuk CI.
4. Step type **webhook-sim** (HMAC payment.paid — simulasi pelunasan digital).
5. Kondisi lebih kaya (`>`, `<`, `contains`, dot-path di kiri), capture dari header/status.
6. Loop/retry (polling sampai kondisi — mis. tunggu `paid`) tanpa merusak DAG visual.
7. Editor graf drag-and-drop (flowmaid scene API sudah menyediakan geometri interaktif).

## Konvensi repo

- Commit message: pendek, fokus **kenapa** (bukan apa). Tanpa footer Co-Authored-By.
- Jangan pernah commit token/host internal: env file (`dev.yaml`, `*-env.yaml` kecuali `env.sample.yaml`/`mock-env.yaml` dummy) sudah di-.gitignore — pertahankan.
- `cargo test` wajib hijau sebelum push; fitur GUI di belakang feature flag `gui` agar CLI tetap ramping.
