// Sidebar groups (`group_by_host`): each group of hosts gets one header
// workspace, and every mirror workspace of its hosts nests under it.
//
// herdr nests a workspace under another only through git worktrees: a linked
// worktree's workspace shows under the workspace of its repo's main checkout.
// So each group owns a stand-in repo (one empty commit) whose main checkout is
// the header's cwd, and each mirror workspace is opened on a linked worktree of
// it. A mirror pane runs the streamer, so the worktree is only a place for the
// workspace to belong; nothing ever reads it.
//
// The filesystem path is the identity: the header is the workspace whose
// checkout is `<root>/<group>/main`, and the mirror of remote workspace `rid`
// from `host` is the one whose checkout is `<root>/<group>/wt/<host>--<rid>`.
// Adopting what a crash or a restart left behind is a snapshot lookup, never a
// guess from a label.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::api::ApiClient;
use crate::config::MirrorConfig;
use crate::mirror::WsInfo;
use crate::state::load_state;
use crate::util::{err, home_dir, sane_component, Logger, Result};

pub struct Grouping {
    pub enabled: bool,
    root: PathBuf,
    /// host → its group (the leading host's name)
    group_of: HashMap<String, String>,
    /// group → header label (the leading host's prefix)
    header_label: HashMap<String, String>,
    /// one header per group: two hosts of a group converge concurrently, and
    /// both may find it missing. Also keeps `tidy` from closing a header
    /// between its creation and its first child opening.
    lock: tokio::sync::Mutex<()>,
}

/// What `worktree.open` answered.
pub struct Opened {
    pub workspace_id: String,
    pub tab_id: String,
    /// a mirror workspace already open on that checkout: one a crash or a
    /// restart left behind, adopted rather than doubled
    pub already_open: bool,
    pub tab_count: Option<u64>,
    pub pane_count: Option<u64>,
}

pub fn default_root() -> PathBuf {
    home_dir().join(".cache").join("herdr-mirror").join("groups")
}

impl Grouping {
    pub fn new(config: &MirrorConfig, root: PathBuf) -> Grouping {
        // herdr reports checkout paths with symlinks resolved (/tmp is
        // /private/tmp on macOS), and every comparison here is by path
        let _ = std::fs::create_dir_all(&root);
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let prefix_of: HashMap<&str, &str> =
            config.hosts.iter().map(|h| (h.name.as_str(), h.prefix.as_str())).collect();
        Grouping {
            enabled: config.group_by_host,
            root,
            group_of: config.hosts.iter().map(|h| (h.name.clone(), h.group.clone())).collect(),
            header_label: config
                .hosts
                .iter()
                .map(|h| (h.group.clone(), prefix_of.get(h.group.as_str()).unwrap_or(&h.group.as_str()).to_string()))
                .collect(),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// A grouping rooted at a path that is never created or resolved.
    #[cfg(test)]
    pub fn for_tests(config: &MirrorConfig, root: &str) -> Grouping {
        let mut g = Grouping::new(config, std::env::temp_dir().join("hm-groups-unused"));
        g.root = PathBuf::from(root);
        g
    }

    fn group_dir(&self, group: &str) -> PathBuf {
        self.root.join(sane_component(group))
    }

    pub fn main_dir(&self, group: &str) -> PathBuf {
        self.group_dir(group).join("main")
    }

    fn child_dir_in(&self, group: &str, host: &str, rid: &str) -> PathBuf {
        self.group_dir(group)
            .join("wt")
            .join(format!("{}--{}", sane_component(host), sane_component(rid)))
    }

    /// Where the mirror of `host`'s remote workspace `rid` belongs: its
    /// worktree checkout when grouping, `None` for a flat workspace.
    pub fn desired_checkout(&self, host: &str, rid: &str) -> Option<PathBuf> {
        if !self.enabled {
            return None;
        }
        let group = self.group_of.get(host)?;
        Some(self.child_dir_in(group, host, rid))
    }

    /// The checkout of ours a local workspace is a mirror on, `None` when it
    /// is not a linked worktree under the groups root (a flat mirror).
    pub fn actual_checkout(&self, ws: &WsInfo) -> Option<PathBuf> {
        let wt = ws.worktree.as_ref().filter(|w| w.is_linked_worktree == Some(true))?;
        let path = PathBuf::from(wt.checkout_path.as_deref()?);
        path.starts_with(&self.root).then_some(path)
    }

    /// The group a local workspace sits in: a header or a mirror under one.
    /// Keyed by the group's main checkout, so a group no host names any more
    /// still reads as one.
    pub fn ws_group_main(&self, ws: &WsInfo) -> Option<PathBuf> {
        let wt = ws.worktree.as_ref()?;
        let main = PathBuf::from(wt.repo_root.as_deref()?);
        (main.starts_with(&self.root) && main.file_name().is_some_and(|f| f == "main")).then_some(main)
    }

    /// Open the mirror of `host`'s remote workspace `rid` under its group's
    /// header, making the header, the stand-in repo and the worktree first
    /// when they are missing.
    pub async fn open_child(&self, local: &ApiClient, host: &str, rid: &str, label: &str, log: &Logger) -> Result<Opened> {
        let group = self.group_of.get(host).ok_or_else(|| err(format!("no group for host {host}")))?;
        let path = self.child_dir_in(group, host, rid);
        let _guard = self.lock.lock().await;
        let header = self.ensure_header(local, group, log).await?;
        let main = self.main_dir(group);
        ensure_worktree(&main, &path).await?;
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            already_open: bool,
            workspace: RawWs,
            tab: RawTab,
        }
        #[derive(Deserialize)]
        struct RawWs {
            workspace_id: String,
            tab_count: Option<u64>,
            pane_count: Option<u64>,
        }
        #[derive(Deserialize)]
        struct RawTab {
            tab_id: String,
        }
        let raw: Raw = local
            .request_t(
                "worktree.open",
                json!({
                    "workspace_id": header,
                    "path": path.display().to_string(),
                    "label": label,
                    "focus": false,
                }),
            )
            .await?;
        Ok(Opened {
            workspace_id: raw.workspace.workspace_id,
            tab_id: raw.tab.tab_id,
            already_open: raw.already_open,
            tab_count: raw.workspace.tab_count,
            pane_count: raw.workspace.pane_count,
        })
    }

    /// The header's workspace id, created when the group has none. Called
    /// with the lock held.
    async fn ensure_header(&self, local: &ApiClient, group: &str, log: &Logger) -> Result<String> {
        let main = self.main_dir(group);
        ensure_repo(&main).await?;
        let list = list_workspaces(local).await?;
        if let Some(h) = list.iter().find(|w| header_main(&self.root, w).as_deref() == Some(main.as_path())) {
            return Ok(h.workspace_id.clone());
        }
        let label = self.header_label.get(group).cloned().unwrap_or_else(|| group.to_string());
        log.log(&format!("creating group header {label} for {group}"));
        #[derive(Deserialize)]
        struct Created {
            workspace: CreatedWs,
        }
        #[derive(Deserialize)]
        struct CreatedWs {
            workspace_id: String,
        }
        let created: Created = local
            .request_t(
                "workspace.create",
                json!({ "label": label, "cwd": main.display().to_string(), "focus": false }),
            )
            .await?;
        Ok(created.workspace.workspace_id)
    }

    /// Bring the groups in line with what is on screen and in the maps:
    /// close a header with no mirror left under it, remove a worktree no
    /// workspace is open on, and keep each header labelled with its group's
    /// prefix. Safe to call at any time and from any process; a no-op when
    /// everything is in place. Runs whether or not grouping is on, so turning
    /// it off clears what it left.
    pub async fn tidy(&self, local: &ApiClient, state_dir: &Path, log: &Logger) {
        // nothing grouped, ever: the common case costs one read_dir
        if !self.enabled && std::fs::read_dir(&self.root).map_or(true, |mut d| d.next().is_none()) {
            return;
        }
        let _guard = self.lock.lock().await;
        let Ok(list) = list_workspaces(local).await else { return };
        // the checkouts live map entries will want: with grouping off they
        // want none, and what grouping left goes
        let protected: HashSet<PathBuf> = self
            .group_of
            .keys()
            .flat_map(|host| {
                load_state(state_dir, host)
                    .workspaces
                    .into_iter()
                    .filter(|(_, e)| !e.is_tombstoned())
                    .filter_map(|(rid, _)| self.desired_checkout(host, &rid))
                    .collect::<Vec<_>>()
            })
            .collect();
        let plan = plan_tidy(&self.root, &list, &protected, &existing_children(&self.root));
        for ws in plan.close_headers {
            log.log(&format!("closing group header {ws}: no mirror is left under it"));
            // never close_group: a mirror that appeared since makes herdr
            // refuse, which is the answer we want
            if let Err(e) = local.request("workspace.close", json!({ "workspace_id": ws })).await {
                log.log(&format!("could not close group header {ws}: {e}"));
            }
        }
        for path in plan.remove_worktrees {
            if let Some(main) = path.parent().and_then(Path::parent).map(|g| g.join("main")) {
                if let Err(e) = remove_worktree(&main, &path).await {
                    log.log(&format!("could not remove worktree {}: {e}", path.display()));
                }
            }
        }
        if self.enabled {
            for w in &list {
                let Some(main) = header_main(&self.root, w) else { continue };
                let Some(want) = self
                    .header_label
                    .iter()
                    .find(|(g, _)| self.main_dir(g) == main)
                    .map(|(_, l)| l)
                else {
                    continue;
                };
                if &w.label != want {
                    let _ = local
                        .request("workspace.rename", json!({ "workspace_id": w.workspace_id, "label": want }))
                        .await;
                }
            }
        }
    }
}

/// The main checkout of a header of ours, if `ws` is one.
fn header_main(root: &Path, ws: &WsInfo) -> Option<PathBuf> {
    let wt = ws.worktree.as_ref().filter(|w| w.is_linked_worktree != Some(true))?;
    let path = PathBuf::from(wt.checkout_path.as_deref()?);
    (path.starts_with(root) && path.file_name().is_some_and(|f| f == "main")).then_some(path)
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct TidyPlan {
    /// header workspace ids
    pub close_headers: Vec<String>,
    pub remove_worktrees: Vec<PathBuf>,
}

/// Pure planner for `tidy`.
///
/// A header closes when no workspace is open under it and no map holds a live
/// mirror that belongs there: right after a local server restart, session
/// restore can bring the header back before its mirrors, and the maps are what
/// say those are coming. A worktree goes when nothing is open on it and no
/// live map entry claims it, for the same reason: herdr restores a workspace
/// only while its directory exists.
pub(crate) fn plan_tidy(
    root: &Path,
    list: &[WsInfo],
    protected: &HashSet<PathBuf>,
    children: &[PathBuf],
) -> TidyPlan {
    let open: HashSet<PathBuf> = list
        .iter()
        .filter_map(|w| w.worktree.as_ref()?.checkout_path.as_deref().map(PathBuf::from))
        .collect();
    let mut plan = TidyPlan::default();
    for w in list {
        let Some(main) = header_main(root, w) else { continue };
        let group = main.parent().map(Path::to_path_buf).unwrap_or_default();
        let has_open_child = list.iter().any(|c| {
            c.worktree.as_ref().is_some_and(|wt| {
                wt.is_linked_worktree == Some(true) && wt.repo_root.as_deref().map(Path::new) == Some(main.as_path())
            })
        });
        let has_coming_child = protected.iter().any(|p| p.starts_with(&group));
        if !has_open_child && !has_coming_child {
            plan.close_headers.push(w.workspace_id.clone());
        }
    }
    for c in children {
        if !open.contains(c) && !protected.contains(c) {
            plan.remove_worktrees.push(c.clone());
        }
    }
    plan
}

/// Every worktree directory under the groups root: `<root>/<group>/wt/*`.
fn existing_children(root: &Path) -> Vec<PathBuf> {
    let Ok(groups) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out = Vec::new();
    for g in groups.flatten() {
        let Ok(children) = std::fs::read_dir(g.path().join("wt")) else { continue };
        out.extend(children.flatten().map(|c| c.path()));
    }
    out.sort();
    out
}

async fn list_workspaces(local: &ApiClient) -> Result<Vec<WsInfo>> {
    #[derive(Deserialize)]
    struct Listed {
        workspaces: Vec<WsInfo>,
    }
    let listed: Listed = local.request_t("workspace.list", json!({})).await?;
    Ok(listed.workspaces)
}

async fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        // a machine without a git identity, or one that signs every commit,
        // must still make the stand-in repo's one empty commit
        .args(["-c", "user.name=herdr-mirror", "-c", "user.email=herdr-mirror@localhost", "-c", "commit.gpgsign=false"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| err(format!("cannot run git: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(err(format!(
            "git {} in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// The group's stand-in repo: a main checkout with one empty commit on a
/// branch named `mirror`, which is what the header's sidebar row shows.
async fn ensure_repo(main: &Path) -> Result<()> {
    if main.join(".git").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(main)?;
    git(main, &["init", "-q"]).await?;
    git(main, &["symbolic-ref", "HEAD", "refs/heads/mirror"]).await?;
    git(main, &["commit", "-q", "--allow-empty", "-m", "herdr-mirror group"]).await
}

async fn ensure_worktree(main: &Path, path: &Path) -> Result<()> {
    if path.join(".git").is_file() {
        return Ok(());
    }
    // a directory left half-made, or a worktree git forgot, is redone
    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    git(main, &["worktree", "prune"]).await?;
    git(main, &["worktree", "add", "-q", "--detach", &path.display().to_string()]).await
}

async fn remove_worktree(main: &Path, path: &Path) -> Result<()> {
    if git(main, &["worktree", "remove", "--force", &path.display().to_string()]).await.is_err() {
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
        git(main, &["worktree", "prune"]).await?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::parse_config;
    use crate::mirror::WsWorktree;

    fn grouping(toml: &str) -> Grouping {
        Grouping::for_tests(&parse_config(toml).unwrap(), "/g")
    }

    pub(crate) fn ws(id: &str, checkout: Option<&str>, repo: Option<&str>, linked: bool) -> WsInfo {
        WsInfo {
            workspace_id: id.into(),
            label: id.into(),
            tab_count: None,
            pane_count: None,
            active_tab_id: None,
            tokens: HashMap::new(),
            worktree: checkout.map(|c| WsWorktree {
                checkout_path: Some(c.into()),
                repo_name: Some("main".into()),
                repo_root: repo.map(String::from),
                is_linked_worktree: Some(linked),
            }),
        }
    }

    const CONFIG: &str = "group_by_host = true\n\
        [hosts.sb]\ntarget = \"sb\"\nprefix = \"brightlights\"\n\
        [hosts.ws-1]\ntarget = \"ws-1\"\ngroup = \"sb\"\n\
        [hosts.vm]\ntarget = \"vm\"\n";

    #[test]
    fn a_mirror_belongs_under_its_hosts_group_by_path() {
        let g = grouping(CONFIG);
        assert_eq!(g.desired_checkout("sb", "w7:T"), Some(PathBuf::from("/g/sb/wt/sb--w7_T")));
        assert_eq!(g.desired_checkout("ws-1", "w1"), Some(PathBuf::from("/g/sb/wt/ws_1--w1")));
        assert_eq!(g.desired_checkout("vm", "w1"), Some(PathBuf::from("/g/vm/wt/vm--w1")));
        assert_eq!(g.header_label["sb"], "brightlights", "the leading host's prefix");
        let off = grouping(&CONFIG.replace("group_by_host = true", "group_by_host = false"));
        assert_eq!(off.desired_checkout("sb", "w1"), None, "flat when grouping is off");
    }

    #[test]
    fn only_a_linked_worktree_under_the_root_is_a_grouped_mirror() {
        let g = grouping(CONFIG);
        let child = ws("w2", Some("/g/sb/wt/sb--w1"), Some("/g/sb/main"), true);
        assert_eq!(g.actual_checkout(&child), Some(PathBuf::from("/g/sb/wt/sb--w1")));
        assert_eq!(g.actual_checkout(&ws("w3", None, None, false)), None, "a flat mirror");
        let elsewhere = ws("w4", Some("/src/app-wt"), Some("/src/app"), true);
        assert_eq!(g.actual_checkout(&elsewhere), None, "someone's own worktree");
        let header = ws("w1", Some("/g/sb/main"), Some("/g/sb/main"), false);
        assert!(header_main(&g.root, &header).is_some() && header_main(&g.root, &child).is_none());
        assert_eq!(g.ws_group_main(&child), Some(PathBuf::from("/g/sb/main")));
        assert_eq!(g.ws_group_main(&header), Some(PathBuf::from("/g/sb/main")));
        assert_eq!(g.ws_group_main(&elsewhere), None);
    }

    #[test]
    fn tidy_closes_an_empty_header_and_keeps_one_with_mirrors() {
        let root = Path::new("/g");
        let list = vec![
            ws("w1", Some("/g/sb/main"), Some("/g/sb/main"), false),
            ws("w2", Some("/g/sb/wt/sb--w1"), Some("/g/sb/main"), true),
            ws("w3", Some("/g/vm/main"), Some("/g/vm/main"), false),
            ws("w4", None, None, false),
            ws("w5", Some("/src/app"), Some("/src/app"), false),
        ];
        let plan = plan_tidy(root, &list, &HashSet::new(), &[]);
        assert_eq!(plan.close_headers, vec!["w3".to_string()]);
    }

    #[test]
    fn tidy_keeps_a_header_whose_mirrors_the_maps_say_are_coming() {
        // session restore can bring the header back before its mirrors
        let root = Path::new("/g");
        let list = vec![ws("w1", Some("/g/sb/main"), Some("/g/sb/main"), false)];
        let protected: HashSet<PathBuf> = [PathBuf::from("/g/sb/wt/sb--w1")].into();
        let plan = plan_tidy(root, &list, &protected, &[PathBuf::from("/g/sb/wt/sb--w1")]);
        assert_eq!(plan, TidyPlan::default());
    }

    #[test]
    fn tidy_removes_only_worktrees_nothing_is_open_on_or_coming_to() {
        let root = Path::new("/g");
        let list = vec![
            ws("w1", Some("/g/sb/main"), Some("/g/sb/main"), false),
            ws("w2", Some("/g/sb/wt/sb--w1"), Some("/g/sb/main"), true),
        ];
        let children = [
            PathBuf::from("/g/sb/wt/sb--w1"),
            PathBuf::from("/g/sb/wt/sb--w2"),
            PathBuf::from("/g/sb/wt/sb--w3"),
        ];
        let protected: HashSet<PathBuf> = [PathBuf::from("/g/sb/wt/sb--w2")].into();
        let plan = plan_tidy(root, &list, &protected, &children);
        assert_eq!(plan.remove_worktrees, vec![PathBuf::from("/g/sb/wt/sb--w3")]);
        assert!(plan.close_headers.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_stand_in_repo_and_a_worktree_are_made_once_and_removed() {
        let dir = std::env::temp_dir().join(format!("hm-stand-in-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let main = dir.join("sb").join("main");
        let child = dir.join("sb").join("wt").join("sb--w1");
        ensure_repo(&main).await.unwrap();
        ensure_repo(&main).await.unwrap();
        ensure_worktree(&main, &child).await.unwrap();
        ensure_worktree(&main, &child).await.unwrap();
        assert!(child.join(".git").is_file());
        assert_eq!(existing_children(&dir), vec![child.clone()]);
        remove_worktree(&main, &child).await.unwrap();
        assert!(!child.exists());
        // a directory git does not know is redone rather than refused
        std::fs::create_dir_all(&child).unwrap();
        ensure_worktree(&main, &child).await.unwrap();
        assert!(child.join(".git").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
