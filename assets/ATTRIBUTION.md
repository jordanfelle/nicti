# Asset attribution

Every font, icon, image and model file tracked in this repository (outside `spikes/` and test
snapshots) needs a row here, enforced by `scripts/check-repo-policy.py` (#433). A row's `Path` cell
is the repo-relative path in backticks.

| Path | Source | License | License text |
|------|--------|---------|--------------|
| `crates/nicti-pelt/assets/fonts/Inter-Regular.ttf` | [Inter](https://github.com/rsms/inter) by The Inter Project Authors | SIL OFL 1.1 | `crates/nicti-pelt/assets/fonts/OFL-Inter.txt` |
| `crates/nicti-pelt/assets/fonts/Inter-SemiBold.ttf` | [Inter](https://github.com/rsms/inter) by The Inter Project Authors | SIL OFL 1.1 | `crates/nicti-pelt/assets/fonts/OFL-Inter.txt` |

Inter Regular is also `include_bytes!`'d by `crates/nicti-preen` for the text watermark (#338); it is the same file as above, not a second copy.
