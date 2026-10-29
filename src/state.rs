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

/// A local user's ask to focus the mirror of a remote pane they just created
/// (`remote-split`, `remote-tab`), left for the daemon, which maps it.
///
/// Only the remote-create actions write one, so a pane an agent or script
/// splits inside the sandbox is mirrored without taking focus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusRequest {
    pub remote_pane_id: String,
    /// the mirror workspace the action was invoked from
    pub local_workspace_id: String,
    pub at_ms: u64,
}

/// How long a focus request waits for its mapping. Past this the user has
/// likely moved on, and a late mapping must not pull them back.
pub const FOCUS_REQUEST_TTL_MS: u64 = 5_000;

pub fn focus_request_path(state_dir: &Path, host: &str) -> PathBuf {
    state_dir.join(format!("{host}-focus.json"))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Leave a focus request for the daemon. One per host: a newer action
/// replaces an older one, as a newer native split would take focus.
pub fn request_focus(state_dir: &Path, host: &str, req: &FocusRequest) -> Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let path = focus_request_path(state_dir, host);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(req)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load_focus_request(state_dir: &Path, host: &str) -> Option<FocusRequest> {
    let text = std::fs::read_to_string(focus_request_path(state_dir, host)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn clear_focus_request(state_dir: &Path, host: &str) {
    let _ = std::fs::remove_file(focus_request_path(state_dir, host));
}

/// What to do with a pending focus request after a converge.
#[derive(Debug, PartialEq)]
pub enum FocusStep {
    /// its pane isn't mirrored yet: keep it for the next converge
    Wait,
    /// too old, or the user left the workspace it was asked from: discard it
    Drop,
    /// focus this local mirror pane, then discard the request
    Focus(String),
}

/// `focused_workspace` is the local herdr's focused workspace now. Only the
/// requested remote pane's mirror is ever focused, and only while the user is
/// still in the workspace they asked from.
pub fn focus_step(
    req: &FocusRequest,
    state: &HostState,
    focused_workspace: Option<&str>,
    now_ms: u64,
) -> FocusStep {
    if now_ms.saturating_sub(req.at_ms) > FOCUS_REQUEST_TTL_MS {
        return FocusStep::Drop;
    }
    if focused_workspace != Some(req.local_workspace_id.as_str()) {
        return FocusStep::Drop;
    }
    match state.panes.get(&req.remote_pane_id) {
        Some(e) if !e.is_tombstoned() => FocusStep::Focus(e.local_id.clone()),
        _ => FocusStep::Wait,
    }
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

    fn focus_state(mapped: &[(&str, &str)]) -> HostState {
        let mut state = HostState::default();
        for (rid, lid) in mapped {
            state.panes.insert(
                rid.to_string(),
                PaneEntry { local_id: lid.to_string(), tombstone: None, seq: 0, reported: None },
            );
        }
        state
    }

    fn focus_req(at_ms: u64) -> FocusRequest {
        FocusRequest { remote_pane_id: "w1:p5".into(), local_workspace_id: "w9".into(), at_ms }
    }

    /// The mirror of the pane the user asked for is focused once it is mapped.
    #[test]
    fn a_pending_focus_lands_on_the_mirror_once_mapped() {
        let req = focus_req(1_000);
        assert_eq!(focus_step(&req, &focus_state(&[]), Some("w9"), 1_200), FocusStep::Wait);
        assert_eq!(
            focus_step(&req, &focus_state(&[("w1:p5", "w9:p3")]), Some("w9"), 1_400),
            FocusStep::Focus("w9:p3".into())
        );
    }

    /// A mapping that comes late never pulls the user back.
    #[test]
    fn a_focus_request_expires() {
        let req = focus_req(1_000);
        let mapped = focus_state(&[("w1:p5", "w9:p3")]);
        assert_eq!(focus_step(&req, &mapped, Some("w9"), 1_000 + FOCUS_REQUEST_TTL_MS), FocusStep::Focus("w9:p3".into()));
        assert_eq!(focus_step(&req, &mapped, Some("w9"), 1_001 + FOCUS_REQUEST_TTL_MS), FocusStep::Drop);
    }

    /// The user moved to another workspace: their focus is left where it is.
    #[test]
    fn no_focus_after_the_user_left_the_workspace() {
        let req = focus_req(1_000);
        let mapped = focus_state(&[("w1:p5", "w9:p3")]);
        assert_eq!(focus_step(&req, &mapped, Some("w4"), 1_100), FocusStep::Drop);
        assert_eq!(focus_step(&req, &mapped, None, 1_100), FocusStep::Drop);
    }

    /// A pane split inside the sandbox by something else (an agent, a script)
    /// is mapped without focus: only the requested pane is ever focused.
    #[test]
    fn a_remote_originated_pane_is_not_focused() {
        let req = focus_req(1_000);
        let other = focus_state(&[("w1:p6", "w9:p4")]);
        assert_eq!(focus_step(&req, &other, Some("w9"), 1_100), FocusStep::Wait);
        let dir = std::env::temp_dir().join(format!("hm-focus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_focus_request(&dir, "vps"), None, "no request, no focus");
        request_focus(&dir, "vps", &req).unwrap();
        assert_eq!(load_focus_request(&dir, "vps"), Some(req));
        clear_focus_request(&dir, "vps");
        assert_eq!(load_focus_request(&dir, "vps"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
