# flowrun — instruksi untuk Claude

**Sebelum mengerjakan apa pun: BACA `CONTEXT.md` di root repo ini.** Itu handoff
lengkap dari sesi pengembangan sebelumnya — filosofi, keputusan desain (beserta
alasannya, jangan diulang debatnya), status fitur, gotcha teknis, dan backlog
terurut. Anggap CONTEXT.md sebagai sumber kebenaran; update dia setiap ada
keputusan/fitur besar.

Aturan kerja repo ini:
- `cargo test` wajib hijau sebelum commit/push. Fitur GUI di belakang feature
  flag `gui` (`cargo build --features gui --bin flowrun-gui`) — CLI tetap ramping.
- JANGAN pernah commit token/base_url internal. Env file (`dev.yaml`, dsb.)
  gitignored; hanya `env.sample.yaml` & `mock-env.yaml` (dummy) yang boleh masuk.
- Commit message: pendek, fokus *kenapa*. Tanpa footer Co-Authored-By.
- Demo/dev UI tanpa backend nyata: `python3 tools/wacca_mock.py 18923` lalu
  jalankan contoh di `examples/` dengan `examples/wacca-order/mock-env.yaml`.
- Lisensi GPL-3.0-or-later (mengikuti dependensi flowmaid) — jangan tambah
  dependensi yang tak kompatibel GPL.
