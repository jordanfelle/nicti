//! Writes `litter draft`'s output: `draft.json` (this pass's candidate grouping, per frame) plus
//! a self-contained local `label.html` contact sheet for the user to split/merge groups by hand
//! and export `labels.json` -- and reads that exported file back for `litter eval`.
//!
//! **Deliberately local, not a published Artifact**: the contact sheet embeds real photos of real
//! people (T0 preview thumbnails), which is both a privacy concern (uploading third-party photos)
//! and, at con scale, well past the 16MB artifact size limit. `label.html` needs no server --
//! thumbnails are plain `<img>` files next to it (which `file://` origins load fine, unlike
//! `fetch()`), and the export button builds a `Blob`/`URL.createObjectURL` download client-side.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftFrame {
    pub index: usize,
    pub filename: String,
    pub sha256: String,
    /// `"YYYY-MM-DD HH:MM:SS.mmm"`, human-readable in the contact sheet -- not reparsed by
    /// `litter eval`, which reads `labels.json`'s own `tight_id`/`set_id` only.
    pub capture_time: String,
    pub serial: Option<String>,
    pub gap_before_secs: f64,
    pub tight_group: usize,
    pub set_group: usize,
    /// Relative path from `label.html` to this frame's T0 thumbnail (e.g. `"thumbs/0007.jpg"`).
    pub thumb: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelRow {
    pub filename: String,
    pub sha256: String,
    pub tight_id: usize,
    pub set_id: usize,
}

pub fn write_draft(work_dir: &Path, frames: &[DraftFrame]) -> anyhow::Result<()> {
    fs::create_dir_all(work_dir)?;
    let draft_path = work_dir.join("draft.json");
    fs::write(&draft_path, serde_json::to_string_pretty(frames)?)?;

    // `<` is JSON-escaped to `<` before splicing into the `<script>` block below: a
    // filename or serial number containing `</script>` would otherwise terminate the script tag
    // early and let arbitrary HTML/JS run when `label.html` is opened -- caught by an adversarial
    // review. Low severity (a local, self-generated single-user file, no real trust boundary),
    // but a real defect in output this repo generates and the user opens in a browser.
    let data_json = serde_json::to_string(frames)?.replace('<', "\\u003c");
    let html = LABEL_HTML_TEMPLATE.replace("__DRAFT_JSON__", &data_json);
    fs::write(work_dir.join("label.html"), html)?;
    Ok(())
}

pub fn read_labels(path: &Path) -> anyhow::Result<Vec<LabelRow>> {
    let contents = fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading labels {}: {e}", path.display()))?;
    let rows: Vec<LabelRow> = serde_json::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("parsing labels {}: {e}", path.display()))?;
    Ok(rows)
}

const LABEL_HTML_TEMPLATE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>litter -- burst/duplicate grouping labels</title>
<style>
  body { font-family: system-ui, sans-serif; background: #1a1a1a; color: #eee; margin: 0; padding: 12px; }
  #toolbar { position: sticky; top: 0; background: #1a1a1a; padding: 8px 0; z-index: 10; }
  #toolbar button { font-size: 14px; padding: 6px 12px; margin-right: 8px; }
  #strip { display: flex; flex-wrap: wrap; align-items: flex-end; gap: 0; }
  .frame { display: flex; flex-direction: column; align-items: center; padding: 4px; border: 2px solid transparent; }
  .frame img { width: 120px; height: auto; display: block; background: #333; }
  .frame .cap { font-size: 10px; color: #aaa; max-width: 120px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .boundary { width: 10px; align-self: stretch; display: flex; align-items: center; justify-content: center; cursor: pointer; }
  .boundary .bar { width: 3px; height: 100%; min-height: 60px; background: #444; }
  .boundary.tight-linked .bar { background: transparent; }
  .boundary.set-linked .bar { width: 1px; }
  .boundary:not(.set-linked) .bar { background: #d9822b; width: 5px; }
  .boundary:hover .bar { background: #4da3ff; }
  .frame.even { background: #232323; }
  .frame.odd { background: #1a1a1a; }
  #status { color: #9c9; margin-left: 12px; }
</style>
</head>
<body>
<div id="toolbar">
  <button id="export">Export labels.json</button>
  <span>Click a gap to split/merge a tight group. Shift-click a gap to split/merge a set group (only where it's already a tight boundary).</span>
  <span id="status"></span>
</div>
<div id="strip"></div>
<script>
const DATA = __DRAFT_JSON__;

// tightLinked[i] / setLinked[i]: whether frame i is linked to frame i+1 (length n-1).
let tightLinked = DATA.slice(0, -1).map((f, i) => f.tight_group === DATA[i + 1].tight_group);
let setLinked = DATA.slice(0, -1).map((f, i) => f.set_group === DATA[i + 1].set_group);

function recomputeIds() {
  let tight = new Array(DATA.length);
  let set = new Array(DATA.length);
  let t = 0, s = 0;
  tight[0] = 0; set[0] = 0;
  for (let i = 1; i < DATA.length; i++) {
    if (!tightLinked[i - 1]) t++;
    tight[i] = t;
    if (!setLinked[i - 1]) s++;
    set[i] = s;
  }
  return { tight, set };
}

function render() {
  const { tight, set } = recomputeIds();
  const strip = document.getElementById('strip');
  strip.innerHTML = '';
  for (let i = 0; i < DATA.length; i++) {
    const f = DATA[i];
    const div = document.createElement('div');
    div.className = 'frame ' + (tight[i] % 2 === 0 ? 'even' : 'odd');
    // Built with DOM APIs, not innerHTML template interpolation: f.filename is a real camera
    // filename (attacker-uncontrolled in practice, but not a trust boundary this code should
    // lean on) -- a name like "<img src=x onerror=alert(1)>.NEF" would otherwise execute as HTML
    // when this locally-generated page opens. Flagged by CodeRabbit on this PR.
    const img = document.createElement('img');
    img.src = f.thumb;
    img.loading = 'lazy';
    const nameCap = document.createElement('div');
    nameCap.className = 'cap';
    nameCap.textContent = f.filename;
    const gapCap = document.createElement('div');
    gapCap.className = 'cap';
    gapCap.textContent = `+${f.gap_before_secs.toFixed(1)}s`;
    div.append(img, nameCap, gapCap);
    strip.appendChild(div);

    if (i < DATA.length - 1) {
      const b = document.createElement('div');
      b.className = 'boundary' + (tightLinked[i] ? ' tight-linked' : '') + (setLinked[i] ? ' set-linked' : '');
      b.innerHTML = '<div class="bar"></div>';
      const idx = i;
      b.onclick = (ev) => {
        if (ev.shiftKey) {
          // Set-level toggle only makes sense where this is already a tight boundary --
          // merging sets across a tight-linked position is a no-op (they're already one group).
          if (!tightLinked[idx]) {
            setLinked[idx] = !setLinked[idx];
          }
        } else {
          tightLinked[idx] = !tightLinked[idx];
          // Nesting invariant: a merged tight boundary is automatically a merged set boundary too.
          if (tightLinked[idx]) setLinked[idx] = true;
        }
        render();
      };
      strip.appendChild(b);
    }
  }
  document.getElementById('status').textContent =
    `${DATA.length} frames, ${tight[tight.length - 1] + 1} tight groups, ${set[set.length - 1] + 1} sets`;
}

document.getElementById('export').onclick = () => {
  const { tight, set } = recomputeIds();
  const labels = DATA.map((f, i) => ({
    filename: f.filename,
    sha256: f.sha256,
    tight_id: tight[i],
    set_id: set[i],
  }));
  const blob = new Blob([JSON.stringify(labels, null, 2)], { type: 'application/json' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = 'labels.json';
  a.click();
  URL.revokeObjectURL(url);
};

render();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(i: usize, tight: usize, set: usize) -> DraftFrame {
        DraftFrame {
            index: i,
            filename: format!("DSC_{i:04}.NEF"),
            sha256: format!("{i:064x}"),
            capture_time: "2026-07-03 12:00:00.000".to_string(),
            serial: Some("3037771".to_string()),
            gap_before_secs: 1.0,
            tight_group: tight,
            set_group: set,
            thumb: format!("thumbs/{i:04}.jpg"),
        }
    }

    #[test]
    fn write_draft_produces_json_and_html() {
        let dir = tempfile::tempdir().unwrap();
        let frames = vec![frame(0, 0, 0), frame(1, 0, 0), frame(2, 1, 0)];
        write_draft(dir.path(), &frames).unwrap();

        assert!(dir.path().join("draft.json").exists());
        let html = fs::read_to_string(dir.path().join("label.html")).unwrap();
        assert!(html.contains("DSC_0000.NEF"));
        assert!(
            !html.contains("__DRAFT_JSON__"),
            "placeholder must be substituted"
        );
    }

    #[test]
    fn write_draft_escapes_a_filename_that_would_close_the_script_tag() {
        // Regression test for a review-caught bug: a filename containing a literal `</script>`
        // used to terminate the embedded JSON's script tag early, letting arbitrary HTML/JS in
        // the filename execute when label.html is opened.
        let dir = tempfile::tempdir().unwrap();
        let mut evil = frame(0, 0, 0);
        evil.filename = "</script><img src=x onerror=alert(1)>".to_string();
        write_draft(dir.path(), &[evil]).unwrap();

        let html = fs::read_to_string(dir.path().join("label.html")).unwrap();
        assert!(
            !html.contains("</script><img"),
            "a raw </script> must not appear inside the embedded JSON"
        );
        assert!(html.contains("\\u003c/script>"));
    }

    #[test]
    fn read_labels_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            LabelRow {
                filename: "a.nef".into(),
                sha256: "aa".into(),
                tight_id: 0,
                set_id: 0,
            },
            LabelRow {
                filename: "b.nef".into(),
                sha256: "bb".into(),
                tight_id: 1,
                set_id: 0,
            },
        ];
        let path = dir.path().join("labels.json");
        fs::write(&path, serde_json::to_string(&rows).unwrap()).unwrap();

        let read_back = read_labels(&path).unwrap();
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[1].tight_id, 1);
    }

    #[test]
    fn read_labels_reports_missing_file_cleanly() {
        let err = read_labels(Path::new("/nonexistent/labels.json")).unwrap_err();
        assert!(err.to_string().contains("reading labels"));
    }
}
