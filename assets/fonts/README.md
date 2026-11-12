# Fonts shipped with paneld

BDF fonts loaded through `[[font]]` in the config (see the README's Fonts section).
They are copied unmodified from the upstream release archive.

| File | Font | Version | Licence |
|---|---|---|---|
| `spleen-12x24.bdf` | Spleen 12x24 | 2.2.0 | BSD-2-Clause |
| `spleen-16x32.bdf` | Spleen 16x32 | 2.2.0 | BSD-2-Clause |
| `spleen-32x64.bdf` | Spleen 32x64 | 2.2.0 | BSD-2-Clause |

- Author: Frederic Cambus, copyright 2018-2026.
- Source: https://github.com/fcambus/spleen/releases/download/2.2.0/spleen-2.2.0.tar.gz
  (project page: https://github.com/fcambus/spleen).
- Licence text: `LICENSE-spleen.txt` (the `LICENSE` file from that archive). It must stay next to
  the fonts when they are redistributed.

The 32x64 file is the biggest (about 650 KB); 16x32 and 32x64 cover the digit sizes that suit a
172x86 panel (a `%H:%M` clock is 5 characters, 80 px wide at 16x32 and 160 px at 32x64).
