use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{Emitter, Manager};

/// One active watcher per window label, so several windows can watch
/// different repos at the same time. Dropping a watcher disconnects its
/// channel, which ends the matching debounce thread.
pub struct WatcherState(pub Mutex<HashMap<String, ActiveWatch>>);

pub struct ActiveWatch {
    _watcher: RecommendedWatcher,
}

/// How a changed path matters to what we show.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Hit {
    /// objects, logs, lock churn from our own git calls — noise
    Noise,
    /// `.git/index` — also rewritten by our own read-only refreshes
    /// (`git status` refreshes stat info), so the frontend treats an
    /// index-only event right after its own refresh as an echo
    Index,
    /// HEAD / refs / ignore rules — always someone else's doing
    Other,
    /// a worktree path (repo-relative, `/`-separated). Only matters when git
    /// doesn't ignore it: build output (target/, node_modules/, dist/) churns
    /// constantly and used to trigger a full refresh on every write
    Worktree(String),
}

/// Inside .git only index/HEAD/refs/info/exclude affect what we show;
/// everything else (objects, logs, lock churn from our own git calls) is noise.
fn classify(root: &Path, p: &Path) -> Hit {
    let rel = match p.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return Hit::Noise,
    };
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() {
        return Hit::Noise;
    }
    if let Some(rest) = s.strip_prefix(".git/").or(if s == ".git" { Some("") } else { None }) {
        return if rest == "index" {
            Hit::Index
        } else if rest == "HEAD"
            || rest == "MERGE_HEAD"
            || rest == "CHERRY_PICK_HEAD"
            || rest == "REVERT_HEAD"
            || rest == "MERGE_MSG"
            || rest == "info/exclude"
            || rest.starts_with("refs/")
        {
            Hit::Other
        } else {
            Hit::Noise
        };
    }
    // an ignore-rule change can flip any file's visibility
    if s == ".gitignore" || s.ends_with("/.gitignore") {
        return Hit::Other;
    }
    Hit::Worktree(s)
}

/// Stop collecting individual worktree paths past this many in one burst
/// (e.g. a fresh `npm install`) and just refresh.
const MAX_BURST_PATHS: usize = 20_000;
/// Paths asked of one `git check-ignore` call.
const MAX_CHECK_PATHS: usize = 2_000;

/// Decides whether worktree paths are gitignored, remembering ignored
/// directories that hold no tracked files so later churn under them
/// (the common case: a build writing into target/) costs no git call at all.
struct IgnoreFilter {
    root: PathBuf,
    ignored_dirs: HashSet<String>,
}

impl IgnoreFilter {
    fn new(root: PathBuf) -> Self {
        Self { root, ignored_dirs: HashSet::new() }
    }

    fn clear(&mut self) {
        self.ignored_dirs.clear();
    }

    /// whether `rel` lies inside (or is) a remembered ignored directory
    fn cached(&self, rel: &str) -> bool {
        if self.ignored_dirs.is_empty() {
            return false;
        }
        rel.match_indices('/').any(|(i, _)| self.ignored_dirs.contains(&rel[..i]))
            || self.ignored_dirs.contains(rel)
    }

    /// true when at least one path can change what status / the tree shows.
    /// Any git failure counts as relevant: refreshing too often beats missing
    /// a real change.
    fn any_relevant(&mut self, rels: &HashSet<String>) -> bool {
        let pending: Vec<&String> = rels.iter().filter(|r| !self.cached(r)).collect();
        if pending.is_empty() {
            return false;
        }
        let batch = &pending[..pending.len().min(MAX_CHECK_PATHS)];

        // ask about each path plus its ancestor dirs, so whole ignored
        // directories can be remembered
        let mut queries: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for r in batch {
            for (i, _) in r.match_indices('/') {
                let dir = format!("{}/", &r[..i]);
                if seen.insert(dir.clone()) {
                    queries.push(dir);
                }
            }
            if seen.insert((*r).clone()) {
                queries.push((*r).clone());
            }
        }
        let Some(ignored) = check_ignore(&self.root, &queries) else {
            return true;
        };

        // remember the top-most ignored dirs that hold nothing tracked
        // (a force-added file under an ignored dir must still be seen)
        let mut dirs: Vec<&str> = ignored.iter().filter_map(|q| q.strip_suffix('/')).collect();
        dirs.sort_unstable();
        let mut top: Vec<&str> = Vec::new();
        for d in dirs {
            if !top.last().is_some_and(|t| d.starts_with(&format!("{t}/"))) {
                top.push(d);
            }
        }
        if !top.is_empty() {
            if let Some(tracked) = tracked_dirs(&self.root, &top) {
                for d in top {
                    if !tracked.contains(d) {
                        self.ignored_dirs.insert(d.to_string());
                    }
                }
            }
        }

        // check-ignore reports a tracked file as not ignored, so a path is
        // noise only when git itself listed it
        let batch_relevant = batch.iter().any(|r| !ignored.contains(r.as_str()));
        batch_relevant || pending[batch.len()..].iter().any(|r| !self.cached(r))
    }
}

/// `git check-ignore --stdin -z`: the subset of `queries` git ignores, or
/// `None` if git failed.
fn check_ignore(root: &Path, queries: &[String]) -> Option<HashSet<String>> {
    let mut cmd = crate::git::git_command(root);
    cmd.args(["check-ignore", "--stdin", "-z"]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let mut input = Vec::with_capacity(queries.iter().map(|q| q.len() + 1).sum());
    for q in queries {
        input.extend_from_slice(q.as_bytes());
        input.push(0);
    }
    // git answers while it reads, so feed stdin from another thread or a
    // large batch deadlocks on full pipes
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().ok()?;
    let _ = writer.join();
    // 0 = some ignored, 1 = none ignored, anything else = error
    match out.status.code() {
        Some(0) | Some(1) => {}
        _ => return None,
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// which of `dirs` contain at least one tracked file, or `None` if git failed
fn tracked_dirs(root: &Path, dirs: &[&str]) -> Option<HashSet<String>> {
    let mut cmd = crate::git::git_command(root);
    cmd.env("GIT_LITERAL_PATHSPECS", "1");
    cmd.args(["ls-files", "-z", "--"]).args(dirs);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut tracked = HashSet::new();
    for f in text.split('\0').filter(|s| !s.is_empty()) {
        if let Some(d) = dirs.iter().find(|d| f.starts_with(&format!("{d}/"))) {
            tracked.insert(d.to_string());
        }
    }
    Some(tracked)
}

/// Everything one quiet-period burst of events touched.
#[derive(Default)]
struct Burst {
    index: bool,
    other: bool,
    worktree: HashSet<String>,
}

impl Burst {
    fn add(&mut self, filter: &mut IgnoreFilter, ev: notify::Result<notify::Event>) {
        let e = match ev {
            Ok(e) => e,
            // a watcher error may mean dropped events: don't trust the burst
            Err(_) => {
                self.other = true;
                return;
            }
        };
        if e.need_rescan() {
            self.other = true;
        }
        if self.other {
            return; // already refreshing; skip the per-path work
        }
        for p in &e.paths {
            match classify(&filter.root, p) {
                Hit::Noise => {}
                Hit::Index => self.index = true,
                Hit::Other => {
                    filter.clear(); // ignore rules may have changed
                    self.other = true;
                    return;
                }
                Hit::Worktree(rel) => {
                    if !filter.cached(&rel) {
                        self.worktree.insert(rel);
                        if self.worktree.len() > MAX_BURST_PATHS {
                            self.other = true;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// `Some(index_only)` when the frontend should refresh
    fn verdict(self, filter: &mut IgnoreFilter) -> Option<bool> {
        if self.other || (!self.worktree.is_empty() && filter.any_relevant(&self.worktree)) {
            Some(false)
        } else if self.index {
            Some(true)
        } else {
            None
        }
    }
}

/// `repo-changed` payload. `index_only` lets the frontend tell its own
/// refresh's echo (only `.git/index` rewritten) from a real external change
/// (HEAD moved, refs updated, files edited) that must never be dropped.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RepoChanged {
    root: String,
    index_only: bool,
}

fn debounce_loop(
    rx: Receiver<notify::Result<notify::Event>>,
    app: tauri::AppHandle,
    window_label: String,
    emit_root: String,
    root: PathBuf,
) {
    let mut filter = IgnoreFilter::new(root);
    loop {
        let first = match rx.recv() {
            Ok(e) => e,
            Err(_) => return, // watcher dropped
        };
        let mut burst = Burst::default();
        burst.add(&mut filter, first);
        // coalesce until the repo has been quiet for 400ms
        loop {
            match rx.recv_timeout(Duration::from_millis(400)) {
                Ok(e) => burst.add(&mut filter, e),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if let Some(index_only) = burst.verdict(&mut filter) {
            let payload = RepoChanged {
                root: emit_root.clone(),
                index_only,
            };
            let _ = app.emit_to(&window_label, "repo-changed", &payload);
        }
    }
}


#[tauri::command]
pub fn watch_repo(
    window: tauri::Window,
    state: tauri::State<'_, WatcherState>,
    repo: String,
) -> Result<(), String> {
    let label = window.label().to_string();
    let root = PathBuf::from(&repo);
    // one watcher per (window, repo): a window can hold several workspaces
    let key = format!("{label}|{repo}");
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if guard.contains_key(&key) {
        return Ok(());
    }

    let (tx, rx) = channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(tx).map_err(|e| e.to_string())?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;

    let app = window.app_handle().clone();
    let thread_root = root.clone();
    std::thread::spawn(move || debounce_loop(rx, app, label, repo, thread_root));

    guard.insert(key, ActiveWatch { _watcher: watcher });
    Ok(())
}

#[tauri::command]
pub fn unwatch_repo(
    window: tauri::Window,
    state: tauri::State<'_, WatcherState>,
    repo: String,
) -> Result<(), String> {
    let key = format!("{}|{repo}", window.label());
    state.0.lock().map_err(|e| e.to_string())?.remove(&key);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = crate::git::git_command(dir).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?} failed");
    }

    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gitprism-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join(".gitignore"), "node_modules\nbuild/\n*.log\n").unwrap();
        std::fs::write(dir.join("build/keep.txt"), "tracked\n").unwrap();
        std::fs::write(dir.join("build/out.o"), "x").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "x").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/debug.log"), "x").unwrap();
        git(&dir, &["add", ".gitignore", "src/main.rs"]);
        git(&dir, &["add", "-f", "build/keep.txt"]);
        dir
    }

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_separates_git_internals_and_worktree() {
        let root = PathBuf::from("/r");
        assert_eq!(classify(&root, &root.join(".git/index")), Hit::Index);
        assert_eq!(classify(&root, &root.join(".git/refs/heads/main")), Hit::Other);
        assert_eq!(classify(&root, &root.join(".git/info/exclude")), Hit::Other);
        assert_eq!(classify(&root, &root.join(".git/objects/ab/cd")), Hit::Noise);
        assert_eq!(classify(&root, &root.join("sub/.gitignore")), Hit::Other);
        assert_eq!(classify(&root, &root.join("src/a.rs")), Hit::Worktree("src/a.rs".into()));
    }

    #[test]
    fn ignored_churn_is_dropped_and_real_edits_are_not() {
        let dir = temp_repo("filter");
        let mut f = IgnoreFilter::new(dir.clone());

        assert!(!f.any_relevant(&set(&["node_modules/pkg/index.js", "src/debug.log"])));
        // the wholly ignored dir is remembered, so further churn needs no git call
        assert!(f.cached("node_modules/pkg/new.js"));
        assert!(f.cached("node_modules"));

        // build/ is ignored but holds a tracked file: never cache it
        assert!(!f.any_relevant(&set(&["build/out.o"])));
        assert!(!f.cached("build/out.o"));
        assert!(f.any_relevant(&set(&["build/keep.txt"])));

        assert!(f.any_relevant(&set(&["src/main.rs"])));
        assert!(f.any_relevant(&set(&["node_modules/pkg/a.js", "src/new.rs"])));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn burst_verdicts() {
        let dir = temp_repo("burst");
        let mut f = IgnoreFilter::new(dir.clone());
        let ev = |p: &str| {
            Ok(notify::Event::new(notify::EventKind::Any).add_path(dir.join(p)))
        };

        let mut b = Burst::default();
        b.add(&mut f, ev("node_modules/pkg/index.js"));
        b.add(&mut f, ev(".git/objects/aa/bb"));
        assert_eq!(b.verdict(&mut f), None, "ignored + git noise must not refresh");

        let mut b = Burst::default();
        b.add(&mut f, ev("node_modules/pkg/index.js"));
        b.add(&mut f, ev(".git/index"));
        assert_eq!(b.verdict(&mut f), Some(true), "only an index echo");

        let mut b = Burst::default();
        b.add(&mut f, ev(".git/index"));
        b.add(&mut f, ev("src/main.rs"));
        assert_eq!(b.verdict(&mut f), Some(false));

        // editing .gitignore drops the cache and always refreshes
        assert!(f.cached("node_modules/x"));
        let mut b = Burst::default();
        b.add(&mut f, ev(".gitignore"));
        assert!(!f.cached("node_modules/x"));
        assert_eq!(b.verdict(&mut f), Some(false));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
