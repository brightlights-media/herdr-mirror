// The persisted per-host id map — the heart of reconciliation.
//
// remote id → { local id, tombstone, seq, reported }. A tombstone means "the
// user closed this mirror" — never recreate it until restore. Absence of a
// remote id means "remote went away" — close the mirror. Restart-idempotent.
// The camelCase JSON shape matches the TS implementation so an existing
// <host>-map.json carries over.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneEntry {
    pub local_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<bool>,
    #[serde(default)]
    pub seq: u64,
    /// agent label last reported onto this pane; must be explicitly released
    /// when the remote agent goes away, or it sticks forever
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported: Option<String>,
}

impl PaneEntry {
    pub fn is_tombstoned(&self) -> bool {
        self.tombstone == Some(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsEntry {
    pub local_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<bool>,
    /// the auto-created root tab of a fresh mirror workspace; consumed by the
    /// first remote tab's layout.apply so it doesn't stack an extra tab
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_tab_local_id: Option<String>,
    /// remote label as of the last converge — distinguishes "remote renamed"
    /// (remote wins, restamp local) from "user renamed the mirror locally"
    /// (push the rename to the remote instead of stomping it)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_remote_label: Option<String>,
}

impl WsEntry {
    pub fn is_tombstoned(&self) -> bool {
        self.tombstone == Some(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabEntry {
    pub local_id: String,
    /// remote label as of the last converge, exactly as on `WsEntry`: it is
    /// what tells "remote renamed" apart from "user renamed the mirror tab"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_remote_label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostState {
    #[serde(default)]
    pub workspaces: BTreeMap<String, WsEntry>,
    #[serde(default)]
    pub tabs: BTreeMap<String, TabEntry>,
    #[serde(default)]
    pub panes: BTreeMap<String, PaneEntry>,
    /// remote object ids (ws/tab/pane) seen in the previous converge. A mirror is
    /// only closed on snapshot-absence when the object was absent last pass too,
    /// so a remote that reconnects mid-restore doesn't mass-close mirrors.
    #[serde(default)]
    pub prev_remote_ids: std::collections::BTreeSet<String>,
    /// last split ratio both sides agreed on, keyed `<remote tab id>|<path>`
    /// (see layout_sync::path_key). This is the base of the three-way merge
    /// that makes ratio sync two-way: without it a converge can see that the
    /// two sides differ but not which one was resized, so it has to pick a
    /// permanent winner and revert the other side's drag.
    #[serde(default)]
    pub ratios: BTreeMap<String, f64>,
}

/// Marker for `hide`: this host's mirrors are off the sidebar until `show`.
///
/// Deliberately its OWN file rather than a field on `HostState`. The map file is
/// load-modify-written by the daemon, by every CLI subcommand, and by converge
/// around a pass that spans dozens of awaits, with no lock anywhere — so a flag
/// living inside it is silently reset by whoever saves last, and `hide` reports
/// success having done nothing. A marker file has no such race: it is written by
/// one process and only ever read by the others. Same shape as `daemon.paused`.
pub fn hidden_path(state_dir: &Path, host: &str) -> PathBuf {
    state_dir.join(format!("{host}.hidden"))
}

pub fn is_hidden(state_dir: &Path, host: &str) -> bool {
    hidden_path(state_dir, host).exists()
}

/// Returns the error rather than swallowing it: this one write gates the whole
/// feature, so a read-only state dir or a host name that is not a single path
/// component would otherwise make `hide` claim success forever while nothing
/// ever acts on it.
pub fn set_hidden(state_dir: &Path, host: &str, hidden: bool) -> std::io::Result<()> {
    let path = hidden_path(state_dir, host);
    if hidden {
        std::fs::write(path, "")
    } else {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// A one-line notice for one specific mirror pane to show.
///
/// The interception runs in its own short-lived process and closes a plain
/// local shell — nothing of ours is in that pane to draw with, and herdr has no
/// API to write into someone else's pane. But the pane the user was *looking
/// at* when they pressed the key is a live mirror with a streamer in it, and a
/// streamer can paint its own status row. So the notice is addressed to that
/// pane by its local id, and it lands in the same row as "reconnecting in 10s".
///
/// Keyed by pane id on purpose: an earlier version left the note unaddressed
/// and the next streamer to start collected it, which was the replacement pane,
/// reporting a move that had already finished.
fn pane_hint_path(state_dir: &Path, local_pane_id: &str) -> PathBuf {
    state_dir.join(format!(".hint-{}", crate::util::sane_component(local_pane_id)))
}

pub fn set_pane_hint(state_dir: &Path, local_pane_id: &str, msg: &str) {
    let _ = std::fs::create_dir_all(state_dir);
    let _ = std::fs::write(pane_hint_path(state_dir, local_pane_id), msg);
}

/// Read and consume this pane's notice, if any.
pub fn take_pane_hint(state_dir: &Path, local_pane_id: &str) -> Option<String> {
    let path = pane_hint_path(state_dir, local_pane_id);
    let msg = std::fs::read_to_string(&path).ok()?;
    // A notice is about something that just happened. One left behind by a
    // streamer that died before collecting it is stale, and showing it later
    // would report a close the user has long since forgotten. Stat before the
    // unlink: afterwards there is nothing left to ask.
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .and_then(|t| t.elapsed().map_err(std::io::Error::other))
        .is_ok_and(|e| e < std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&path);
    let msg = msg.trim().to_string();
    (fresh && !msg.is_empty()).then_some(msg)
}

pub fn state_path(state_dir: &Path, host: &str) -> PathBuf {
    state_dir.join(format!("{host}-map.json"))
}

/// Hosts that left mirror workspaces on screen but are not in `configured`.
///
/// The daemon builds its host tasks from the config once, at start, so a host
/// dropped from hosts.toml (by hand, or by a tool that writes it) gets no task:
/// its mirrors stay up, frozen, and nothing is ever logged for it again. This
/// names them so the cause is in the log rather than inferred from silence.
/// Returns `(host, live mirror workspaces)`, sorted by host.
pub fn unconfigured_hosts(state_dir: &Path, configured: &[&str]) -> Vec<(String, usize)> {
    let Ok(dir) = std::fs::read_dir(state_dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, usize)> = dir
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix("-map.json").map(str::to_string))
        .filter(|h| !configured.contains(&h.as_str()))
        .map(|h| {
            let live = load_state(state_dir, &h).workspaces.values().filter(|w| !w.is_tombstoned()).count();
            (h, live)
        })
        .filter(|(_, live)| *live > 0)
        .collect();
    out.sort();
    out
}

pub fn load_state(state_dir: &Path, host: &str) -> HostState {
    std::fs::read_to_string(state_path(state_dir, host))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_state(state_dir: &Path, host: &str, state: &HostState) -> Result<()> {
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(state_path(state_dir, host), serde_json::to_string_pretty(state)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape the TS implementation writes must round-trip.
    #[test]
    fn ts_state_shape_roundtrips() {
        let ts = r#"{
 "workspaces": {
  "w9": { "localId": "w1234", "rootTabLocalId": "t99" },
  "wB": { "localId": "w5678", "tombstone": true }
 },
 "tabs": { "w9:t1": { "localId": "t42" } },
 "panes": {
  "w9:p1": { "localId": "w1234:p1", "seq": 12, "reported": "claude" },
  "wB:p1": { "localId": "w5678:p1", "tombstone": true, "seq": 3 }
 }
}"#;
        let state: HostState = serde_json::from_str(ts).unwrap();
        assert_eq!(state.workspaces["w9"].local_id, "w1234");
        assert_eq!(state.workspaces["w9"].root_tab_local_id.as_deref(), Some("t99"));
        assert!(state.workspaces["wB"].is_tombstoned());
        // a tab mapped before label history existed loads with none, which the
        // resolver reads as "remote wins once"
        assert_eq!(state.tabs["w9:t1"].last_remote_label, None);
        assert_eq!(state.panes["w9:p1"].seq, 12);
        assert_eq!(state.panes["w9:p1"].reported.as_deref(), Some("claude"));
        assert!(state.panes["wB:p1"].is_tombstoned());

        let out = serde_json::to_string(&state).unwrap();
        let reparsed: HostState = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed.panes["w9:p1"].local_id, "w1234:p1");
        assert!(out.contains("localId"));
        assert!(out.contains("rootTabLocalId"));
        // absent options stay absent
        assert!(!out.contains("\"reported\":null"));
    }

    #[test]
    fn hidden_is_a_marker_file_not_a_state_field() {
        let dir = std::env::temp_dir().join(format!("hm-hidden-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert!(!is_hidden(&dir, "h"));
        set_hidden(&dir, "h", true).unwrap();
        assert!(is_hidden(&dir, "h"));
        // and it survives a map rewrite, which is the whole reason it is not a
        // field on HostState
        save_state(&dir, "h", &HostState::default()).unwrap();
        assert!(is_hidden(&dir, "h"));
        set_hidden(&dir, "h", false).unwrap();
        assert!(!is_hidden(&dir, "h"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pane_hint_goes_to_one_pane_and_only_once() {
        let dir = std::env::temp_dir().join(format!("hm-hint-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        set_pane_hint(&dir, "wBT:p2", "closing the local tab");
        // not the neighbour's: an unaddressed notice is what let the
        // REPLACEMENT pane announce a move that had already finished
        assert_eq!(take_pane_hint(&dir, "wBT:p3"), None);
        assert_eq!(take_pane_hint(&dir, "wBT:p2").as_deref(), Some("closing the local tab"));
        // consumed, so a repaint doesn't resurrect it
        assert_eq!(take_pane_hint(&dir, "wBT:p2"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_pane_hint_is_dropped_not_shown() {
        let dir = std::env::temp_dir().join(format!("hm-hint-old-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        set_pane_hint(&dir, "wBT:p2", "closing the local tab");
        let path = pane_hint_path(&dir, "wBT:p2");
        // backdate it: only the mtime distinguishes a notice from a leftover
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 120;
        let t = libc::timeval { tv_sec: secs, tv_usec: 0 };
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), [t, t].as_ptr()) }, 0);
        assert_eq!(take_pane_hint(&dir, "wBT:p2"), None);
        assert!(!path.exists(), "stale or not, it is consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host dropped from the config gets no task, so the daemon never logs it
    /// again; its mirrors freeze in silence. The start line must name it.
    #[test]
    fn a_host_dropped_from_the_config_is_named_with_its_live_mirrors() {
        let dir = std::env::temp_dir().join(format!("hm-unconfigured-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = |tomb: Option<bool>| WsEntry {
            local_id: "w9".into(),
            tombstone: tomb,
            root_tab_local_id: None,
            last_remote_label: None,
        };
        let with = |entries: Vec<(&str, WsEntry)>| HostState {
            workspaces: entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            ..HostState::default()
        };
        save_state(&dir, "kept", &with(vec![("w1", ws(None))])).unwrap();
        save_state(&dir, "dropped", &with(vec![("w1", ws(None)), ("w2", ws(None)), ("w3", ws(Some(true)))]))
            .unwrap();
        // nothing left on screen: dropping it is not worth a line on every start
        save_state(&dir, "all-closed", &with(vec![("w1", ws(Some(true)))])).unwrap();
        std::fs::write(dir.join("dropped.ctl"), "").unwrap();

        assert_eq!(unconfigured_hosts(&dir, &["kept"]), vec![("dropped".to_string(), 2)]);
        assert!(unconfigured_hosts(&dir, &["kept", "dropped"]).is_empty());
        assert!(unconfigured_hosts(&dir.join("missing"), &[]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
