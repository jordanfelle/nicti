//! Writes `squint draft`'s output: `draft.json` (one entry per frame, with candidate scores
//! already computed) plus a self-contained local `label.html` contact sheet for the user to tag
//! each frame's real ground truth by hand, and reads the exported `labels.json` back for
//! `squint eval --labels`.
//!
//! **Deliberately local, not a published Artifact** -- same reasoning as `spikes/litter`'s own
//! `label.html`: real photos of real people/fursuiters, and past a published Artifact's size limit
//! at con scale. No server needed; thumbnails are plain `<img>` files loaded via a relative path
//! (works under a `file://` origin, unlike `fetch()`), and export builds a
//! `Blob`/`URL.createObjectURL` download entirely client-side.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftFrame {
    pub index: usize,
    pub filename: String,
    pub sha256: String,
    /// Relative path from `label.html` to this frame's preview thumbnail.
    pub thumb: String,
    /// Every registered candidate's score on this frame, `(name, score)` -- shown in the contact
    /// sheet so a human labeller can see what the candidates already think before overriding it.
    pub candidate_scores: Vec<(String, f64)>,
}

/// One frame's real, human-assigned tags -- multiple may apply (e.g. both `motion_blur` and
/// `eyes_closed`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LabelRow {
    pub filename: String,
    pub sha256: String,
    pub sharp: bool,
    pub motion_blur: bool,
    pub defocus: bool,
    pub misfocus: bool,
    pub eyes_closed: bool,
    pub eyes_obscured: bool,
    pub not_applicable: bool,
}

pub fn write_draft(work_dir: &Path, frames: &[DraftFrame]) -> anyhow::Result<()> {
    fs::create_dir_all(work_dir)?;
    let draft_path = work_dir.join("draft.json");
    fs::write(&draft_path, serde_json::to_string_pretty(frames)?)?;

    // `<` is JSON-escaped before splicing into the `<script>` block, so a filename containing a
    // literal `</script>` can't terminate the tag early and inject HTML/JS -- the same defect
    // class `spikes/litter/src/label.rs` caught and fixed on its own PR.
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
<title>squint -- blur/misfocus/eye labels</title>
<style>
  body { font-family: system-ui, sans-serif; background: #1a1a1a; color: #eee; margin: 0; padding: 12px; }
  #toolbar { position: sticky; top: 0; background: #1a1a1a; padding: 8px 0; z-index: 10; }
  #toolbar button { font-size: 14px; padding: 6px 12px; margin-right: 8px; }
  #grid { display: flex; flex-wrap: wrap; gap: 8px; }
  .frame { width: 160px; padding: 6px; background: #232323; border-radius: 4px; }
  .frame img { width: 100%; height: auto; display: block; background: #333; margin-bottom: 4px; }
  .frame .cap { font-size: 10px; color: #aaa; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .frame .scores { font-size: 9px; color: #789; margin: 2px 0; }
  .tags label { display: block; font-size: 11px; }
  #status { color: #9c9; margin-left: 12px; }
</style>
</head>
<body>
<div id="toolbar">
  <button id="export">Export labels.json</button>
  <span>Tag each frame's real ground truth.</span>
  <span id="status"></span>
</div>
<div id="grid"></div>
<script>
const DATA = __DRAFT_JSON__;
const TAGS = ["sharp", "motion_blur", "defocus", "misfocus", "eyes_closed", "eyes_obscured", "not_applicable"];
const state = DATA.map(() => ({}));

function render() {
  const grid = document.getElementById('grid');
  grid.innerHTML = '';
  for (let i = 0; i < DATA.length; i++) {
    const f = DATA[i];
    const div = document.createElement('div');
    div.className = 'frame';

    const img = document.createElement('img');
    img.src = f.thumb;
    img.loading = 'lazy';
    div.appendChild(img);

    const nameCap = document.createElement('div');
    nameCap.className = 'cap';
    nameCap.textContent = f.filename;
    div.appendChild(nameCap);

    const scores = document.createElement('div');
    scores.className = 'scores';
    scores.textContent = f.candidate_scores.map(([n, s]) => `${n}:${s.toFixed(1)}`).join(' ');
    div.appendChild(scores);

    const tagsDiv = document.createElement('div');
    tagsDiv.className = 'tags';
    for (const tag of TAGS) {
      const label = document.createElement('label');
      const cb = document.createElement('input');
      cb.type = 'checkbox';
      cb.checked = !!state[i][tag];
      cb.onchange = () => { state[i][tag] = cb.checked; };
      label.appendChild(cb);
      label.append(' ' + tag);
      tagsDiv.appendChild(label);
    }
    div.appendChild(tagsDiv);

    grid.appendChild(div);
  }
  document.getElementById('status').textContent = `${DATA.length} frames`;
}

document.getElementById('export').onclick = () => {
  const labels = DATA.map((f, i) => ({
    filename: f.filename,
    sha256: f.sha256,
    sharp: !!state[i].sharp,
    motion_blur: !!state[i].motion_blur,
    defocus: !!state[i].defocus,
    misfocus: !!state[i].misfocus,
    eyes_closed: !!state[i].eyes_closed,
    eyes_obscured: !!state[i].eyes_obscured,
    not_applicable: !!state[i].not_applicable,
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

    fn frame(i: usize) -> DraftFrame {
        DraftFrame {
            index: i,
            filename: format!("DSC_{i:04}.NEF"),
            sha256: format!("{i:064x}"),
            thumb: format!("thumbs/{i:04}.jpg"),
            candidate_scores: vec![("laplacian_variance".to_string(), 123.4)],
        }
    }

    #[test]
    fn write_draft_produces_json_and_html() {
        let dir = tempfile::tempdir().unwrap();
        let frames = vec![frame(0), frame(1)];
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
        let dir = tempfile::tempdir().unwrap();
        let mut evil = frame(0);
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
        let rows = vec![LabelRow {
            filename: "a.nef".into(),
            sha256: "aa".into(),
            sharp: true,
            ..Default::default()
        }];
        let path = dir.path().join("labels.json");
        fs::write(&path, serde_json::to_string(&rows).unwrap()).unwrap();

        let read_back = read_labels(&path).unwrap();
        assert_eq!(read_back.len(), 1);
        assert!(read_back[0].sharp);
        assert!(!read_back[0].motion_blur);
    }

    #[test]
    fn read_labels_reports_missing_file_cleanly() {
        let err = read_labels(Path::new("/nonexistent/labels.json")).unwrap_err();
        assert!(err.to_string().contains("reading labels"));
    }
}
