//! Writes `rosette draft`'s output: `draft.json` (this pass's candidate clustering, per photo)
//! plus a self-contained local `label.html` contact sheet for the user to correct cluster
//! membership by hand and export `labels.json` -- and reads that file back for `rosette eval`.
//!
//! Structurally simpler than `spikes/litter/src/label.rs`'s tight/set boundary-toggle UI: subject
//! clusters aren't sequence-constrained (the same subject can reappear anywhere in the shoot), so
//! there's no "adjacent boundary" to click -- instead each photo gets a numeric cluster-id field
//! the user edits directly, grouped visually by current cluster. Same privacy/size rationale as
//! litter's page for staying local-only, not a published Artifact: real third-party photos, con
//! scale.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftPhoto {
    pub index: usize,
    pub filename: String,
    pub sha256: String,
    /// `None` = DBSCAN noise (unclustered), matching `cluster::Assignment`.
    pub cluster: Option<usize>,
    pub thumb: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelRow {
    pub filename: String,
    pub sha256: String,
    /// The user's corrected subject id. Unlike `cluster` above, this is never `None` -- every
    /// photo is assigned to a real subject group in ground truth, even if the "group" is a
    /// singleton "no one else in the shoot with this subject" case; `-1`-as-"noise" is a
    /// prediction-side concept only, not something ground truth expresses.
    pub subject_id: usize,
}

pub fn write_draft(work_dir: &Path, photos: &[DraftPhoto]) -> anyhow::Result<()> {
    fs::create_dir_all(work_dir)?;
    let draft_path = work_dir.join("draft.json");
    fs::write(&draft_path, serde_json::to_string_pretty(photos)?)?;

    // Same `<` escape as litter's label.rs, same reason: a filename containing `</script>` must
    // not terminate the embedded JSON's script tag early.
    let data_json = serde_json::to_string(photos)?.replace('<', "\\u003c");
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
<title>rosette -- subject grouping labels</title>
<style>
  body { font-family: system-ui, sans-serif; background: #1a1a1a; color: #eee; margin: 0; padding: 12px; }
  #toolbar { position: sticky; top: 0; background: #1a1a1a; padding: 8px 0; z-index: 10; }
  #toolbar button { font-size: 14px; padding: 6px 12px; margin-right: 8px; }
  .group { border: 1px solid #333; margin-bottom: 16px; padding: 8px; }
  .group h3 { margin: 0 0 8px 0; font-size: 14px; color: #9c9; }
  .strip { display: flex; flex-wrap: wrap; gap: 4px; }
  .photo { display: flex; flex-direction: column; align-items: center; padding: 4px; background: #232323; }
  .photo img { width: 120px; height: auto; display: block; background: #333; }
  .photo .cap { font-size: 10px; color: #aaa; max-width: 120px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .photo input { width: 50px; font-size: 12px; margin-top: 2px; }
  #status { color: #9c9; margin-left: 12px; }
</style>
</head>
<body>
<div id="toolbar">
  <button id="export">Export labels.json</button>
  <span>Edit the subject-id box under a photo to move it to a different group, then re-render.</span>
  <button id="rerender">Re-render groups</button>
  <span id="status"></span>
</div>
<div id="groups"></div>
<script>
const DATA = __DRAFT_JSON__;
// subjectId[i]: the user's current (possibly edited) subject assignment for photo i, seeded from
// the predicted cluster (noise/None becomes its own singleton group, numbered after every real
// predicted cluster so it doesn't collide).
const maxCluster = DATA.reduce((m, f) => f.cluster !== null && f.cluster > m ? f.cluster : m, -1);
let nextNoiseId = maxCluster + 1;
let subjectId = DATA.map(f => f.cluster !== null ? f.cluster : nextNoiseId++);

function render() {
  const groupsEl = document.getElementById('groups');
  groupsEl.innerHTML = '';
  const byGroup = new Map();
  DATA.forEach((f, i) => {
    const gid = subjectId[i];
    if (!byGroup.has(gid)) byGroup.set(gid, []);
    byGroup.get(gid).push(i);
  });
  const sortedGroupIds = [...byGroup.keys()].sort((a, b) => a - b);
  for (const gid of sortedGroupIds) {
    const indices = byGroup.get(gid);
    const groupDiv = document.createElement('div');
    groupDiv.className = 'group';
    const h3 = document.createElement('h3');
    h3.textContent = `Subject ${gid} (${indices.length} photos)`;
    groupDiv.appendChild(h3);
    const strip = document.createElement('div');
    strip.className = 'strip';
    for (const i of indices) {
      const f = DATA[i];
      const div = document.createElement('div');
      div.className = 'photo';
      const img = document.createElement('img');
      img.src = f.thumb;
      img.loading = 'lazy';
      const nameCap = document.createElement('div');
      nameCap.className = 'cap';
      nameCap.textContent = f.filename;
      const input = document.createElement('input');
      input.type = 'number';
      input.min = '0';
      input.value = gid;
      input.onchange = () => {
        const parsed = parseInt(input.value, 10);
        const value = Number.isFinite(parsed) && parsed >= 0 ? parsed : 0;
        subjectId[i] = value;
        input.value = value;
      };
      div.append(img, nameCap, input);
      strip.appendChild(div);
    }
    groupDiv.appendChild(strip);
    groupsEl.appendChild(groupDiv);
  }
  document.getElementById('status').textContent =
    `${DATA.length} photos, ${sortedGroupIds.length} subject groups`;
}

document.getElementById('rerender').onclick = render;

document.getElementById('export').onclick = () => {
  const labels = DATA.map((f, i) => ({
    filename: f.filename,
    sha256: f.sha256,
    subject_id: subjectId[i],
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

    fn photo(i: usize, cluster: Option<usize>) -> DraftPhoto {
        DraftPhoto {
            index: i,
            filename: format!("DSC_{i:04}.NEF"),
            sha256: format!("{i:064x}"),
            cluster,
            thumb: format!("thumbs/{i:04}.jpg"),
        }
    }

    #[test]
    fn write_draft_produces_json_and_html() {
        let dir = tempfile::tempdir().unwrap();
        let photos = vec![photo(0, Some(0)), photo(1, Some(0)), photo(2, None)];
        write_draft(dir.path(), &photos).unwrap();

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
        let mut evil = photo(0, Some(0));
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
                subject_id: 0,
            },
            LabelRow {
                filename: "b.nef".into(),
                sha256: "bb".into(),
                subject_id: 1,
            },
        ];
        let path = dir.path().join("labels.json");
        fs::write(&path, serde_json::to_string(&rows).unwrap()).unwrap();

        let read_back = read_labels(&path).unwrap();
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[1].subject_id, 1);
    }

    #[test]
    fn read_labels_reports_missing_file_cleanly() {
        let err = read_labels(Path::new("/nonexistent/labels.json")).unwrap_err();
        assert!(err.to_string().contains("reading labels"));
    }
}
