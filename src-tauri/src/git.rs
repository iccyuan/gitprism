use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tauri::Emitter;

/// git's well-known empty tree object; used as the diff base in repos with no commits
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const MAX_FILE_SIZE: u64 = 5 * 1024 * 1024;
/// fetch/pull/push/clone give up and kill the process tree past this — long enough
/// for a slow-but-alive connection, short enough that a dead one doesn't sit there
/// as an orphaned git.exe/ssh.exe pair for the rest of the session
const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);

/// Kills a whole process tree on timeout (git may have spawned ssh/askpass
/// children, or a hook may have spawned its own children, for the op) using
/// real OS process-tree primitives rather than shelling out to a helper
/// binary (`taskkill`/`kill`):
/// - Windows: the child is spawned suspended (`CREATE_SUSPENDED`) and
///   assigned to a kill-on-close Job Object *before* its main thread ever
///   runs, then resumed. New processes inherit their parent's job by default,
///   so once resumed the whole tree it spawns is covered and
///   `TerminateJobObject` takes it all out with one syscall. Assigning after
///   an unconditional spawn (tried first, reverted) is NOT good enough: a
///   fast-spawning child can create grandchildren before the assignment
///   syscall lands, and those escape the job — confirmed empirically by a
///   leaked `sleep.exe` in `run_git_timeout_kills_whole_process_tree_on_hang`
///   before the suspend/assign/resume ordering was added. This is also why a
///   PID-snapshot tool like `taskkill /T` isn't good enough on its own: it
///   walks parent-child links *after the fact*, so it has the same race plus
///   its own (already changed by the time it runs).
/// - Unix: `process_group(0)` at spawn puts the child in its own new process
///   group; `kill(-pid, SIGKILL)` signals every process in that group. No
///   equivalent race here — the group is set atomically at fork/exec time by
///   the kernel, before the child can run anything.
#[cfg(windows)]
mod proc_tree {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    /// OR'd into the child's creation flags alongside CREATE_NO_WINDOW so it
    /// starts frozen until `resume_main_thread` is called.
    pub const CREATE_SUSPENDED: u32 = 0x0000_0004;

    pub struct JobGuard(HANDLE);

    impl JobGuard {
        /// Creates a kill-on-close job and assigns `child` (still suspended)
        /// to it. Returns `None` on any failure — the caller falls back to a
        /// plain kill of just the top process rather than erroring the whole
        /// operation out.
        pub fn new(child: &std::process::Child) -> Option<Self> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set_ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                let assign_ok = set_ok != 0 && AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) != 0;
                if !assign_ok {
                    CloseHandle(job);
                    return None;
                }
                Some(JobGuard(job))
            }
        }

        /// Kills every process still alive in the job.
        pub fn kill_tree(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Finds the sole thread of a just-created suspended process (Toolhelp
    /// snapshot, filtered by owner PID) and resumes it. Must be called only
    /// after the process has been registered with any job it needs to belong
    /// to — that ordering is the entire point of spawning suspended.
    pub fn resume_main_thread(pid: u32) {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap == INVALID_HANDLE_VALUE {
                return;
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut has_entry = Thread32First(snap, &mut entry) != 0;
            while has_entry {
                if entry.th32OwnerProcessID == pid {
                    let th = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !th.is_null() {
                        ResumeThread(th);
                        CloseHandle(th);
                    }
                }
                has_entry = Thread32Next(snap, &mut entry) != 0;
            }
            CloseHandle(snap);
        }
    }

    /// Fallback for the rare case `JobGuard::new` itself failed: a direct
    /// handle-based kill of just the top process (no shelling out to
    /// `taskkill` — any orphaned child is a much smaller regression than the
    /// unbounded hang this whole mechanism exists to prevent).
    pub fn kill_pid_only(pid: u32) {
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }
}

/// mid-merge / mid-cherry-pick / mid-revert, detected from marker files under
/// the git dir (MERGE_HEAD / CHERRY_PICK_HEAD / REVERT_HEAD)
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum RepoOperation {
    None,
    Merge,
    CherryPick,
    Revert,
    Rebase,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfo {
    pub root: String,
    pub branch: Option<String>,
    pub has_head: bool,
    pub upstream: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub operation: RepoOperation,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

/// which side(s) touched a conflicted path, from `git status --porcelain`'s
/// unmerged XY codes (DD/AU/UD/UA/DU/AA/UU)
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum ConflictSide {
    BothModified,
    AddedByUs,
    AddedByThem,
    DeletedByUs,
    DeletedByThem,
    BothAdded,
    BothDeleted,
}

#[derive(Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileStatus {
    pub path: String,
    pub old_path: Option<String>,
    pub kind: ChangeKind,
    /// line stats from numstat; None for binary files
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
    /// Some(..) only when kind == Conflicted
    pub conflict: Option<ConflictSide>,
    /// true if this entry is index-vs-HEAD (staged); false if worktree-vs-index
    /// (unstaged). A partially-staged file legitimately appears twice, once
    /// with each value. Always false for untracked/conflicted/historical
    /// (commit_files) entries, where the distinction doesn't apply.
    pub staged: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitInfo {
    pub hash: String,
    pub short_hash: String,
    pub author: String,
    pub email: String,
    pub date: String,
    pub subject: String,
    pub additions: u32,
    pub deletions: u32,
    /// >1 means a merge commit — cherry-pick/revert need a -m mainline choice
    /// we don't offer, so the UI disables those actions for these
    pub parents: Vec<String>,
    /// branch/tag names pointing at this commit (from `%D`), cleaned of the
    /// "HEAD -> " / "tag: " decorations git adds — e.g. ["main", "origin/main"]
    pub refs: Vec<String>,
}

#[derive(Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    pub index: usize,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// raw hunk text starting at the "@@" line, verbatim bytes from `git diff`
    pub text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub original: Option<String>,
    pub modified: Option<String>,
    pub is_binary: bool,
    pub too_large: bool,
    /// "diff --git ..." through "+++ ..." lines, verbatim; prepended to a hunk to form a patch
    pub file_header: String,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    fn binary() -> Self {
        FileDiff {
            original: None,
            modified: None,
            is_binary: true,
            too_large: false,
            file_header: String::new(),
            hunks: Vec::new(),
        }
    }

    fn too_large() -> Self {
        FileDiff {
            original: None,
            modified: None,
            is_binary: false,
            too_large: true,
            file_header: String::new(),
            hunks: Vec::new(),
        }
    }
}

/// one recorded git invocation, shown in the Git panel's 控制台 tab
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEntry {
    pub root: String,
    pub args: String,
    pub ok: bool,
    pub at_ms: u64,
}

const CONSOLE_CAP: usize = 500;

fn console_log() -> &'static Mutex<VecDeque<ConsoleEntry>> {
    static LOG: OnceLock<Mutex<VecDeque<ConsoleEntry>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(VecDeque::with_capacity(CONSOLE_CAP)))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn record_console(repo: &Path, args: &[&str], ok: bool) {
    let mut log = console_log().lock().unwrap();
    if log.len() >= CONSOLE_CAP {
        log.pop_front();
    }
    log.push_back(ConsoleEntry {
        root: repo.to_string_lossy().into_owned(),
        args: args.join(" "),
        ok,
        at_ms: now_ms(),
    });
}

/// `creation_flags()` replaces rather than ORs with any previous call, so
/// every site that needs to add its own flag (e.g. `run_git_timeout`'s
/// CREATE_SUSPENDED) re-applies this alongside its own instead of clobbering it.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(crate) fn git_command(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo);
    cmd.args(["-c", "core.quotepath=off"]);
    // a GUI app has no terminal to prompt into — without this, a fetch/pull/push
    // hitting a missing/expired credential hangs run_git's blocking wait_with_output
    // forever instead of failing fast with a clear stderr message
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Blocks on `wait_with_output()` with no wall-clock timeout. `GIT_TERMINAL_PROMPT=0`
/// (set in `git_command`) covers the common hang (missing/expired credentials
/// prompting for input), but a genuinely stalled TCP connection to a remote
/// (dead proxy, dropped VPN) can still block indefinitely — network callers
/// (fetch/pull/push) use `run_git_timeout` below instead, which can kill it.
fn run_git(repo: &Path, args: &[&str], stdin_data: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut cmd = git_command(repo);
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = cmd.spawn().map_err(|e| format!("无法执行 git: {e}"))?;
    if let Some(data) = stdin_data {
        let mut stdin = child.stdin.take().ok_or("无法打开 git stdin")?;
        stdin
            .write_all(data)
            .map_err(|e| format!("写入 git stdin 失败: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git 执行失败: {e}"))?;
    record_console(repo, args, out.status.success());
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Like `run_git`, but for network ops (fetch/pull/push/clone) that can stall on
/// a dead proxy/VPN with no local signal to fail on. `wait_with_output()` runs on
/// a helper thread so a timeout can fire on the caller side; on timeout the whole
/// process tree is killed via `proc_tree` (see above) — plain `Child::kill()`
/// only kills git.exe itself and leaves any ssh/askpass child orphaned, which is
/// what accumulated as stray processes before this fix.
fn run_git_timeout(repo: &Path, args: &[&str], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut cmd = git_command(repo);
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // frozen until resume_main_thread() — see proc_tree's doc comment for
        // why this ordering (not a plain spawn-then-assign) is required
        cmd.creation_flags(CREATE_NO_WINDOW | proc_tree::CREATE_SUSPENDED);
    }
    let child = cmd.spawn().map_err(|e| format!("无法执行 git: {e}"))?;
    let pid = child.id();
    // registers the (still-suspended) child with a kill-on-close job before
    // it runs; if this fails (rare — e.g. job creation denied by policy),
    // `kill()` on timeout still takes out the top-level git.exe process
    #[cfg(windows)]
    let job = proc_tree::JobGuard::new(&child);
    #[cfg(windows)]
    proc_tree::resume_main_thread(pid);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => {
            record_console(repo, args, out.status.success());
            if out.status.success() {
                Ok(out.stdout)
            } else {
                Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
            }
        }
        Ok(Err(e)) => Err(format!("git 执行失败: {e}")),
        Err(_) => {
            #[cfg(windows)]
            match &job {
                Some(j) => j.kill_tree(),
                None => proc_tree::kill_pid_only(pid),
            }
            #[cfg(unix)]
            {
                // negative pid targets the whole process group (see process_group(0) above)
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            record_console(repo, args, false);
            Err(format!(
                "git {} 超时（超过 {}s 无响应，可能是网络连接卡住），已终止",
                args.join(" "),
                timeout.as_secs()
            ))
        }
    }
}

fn run_git_text(repo: &Path, args: &[&str]) -> Result<String, String> {
    let bytes = run_git(repo, args, None)?;
    String::from_utf8(bytes).map_err(|_| "git 输出不是有效的 UTF-8".to_string())
}

/// Like `run_git_timeout`, but for fetch/pull specifically: streams git's own
/// `--progress` output (received objects, compression, …) to the frontend as
/// it happens instead of leaving the UI showing a static "正在 fetch…" until
/// the whole operation finishes. Git only writes progress to stderr when it
/// thinks it's talking to a terminal; since our stderr is a pipe, callers
/// must pass `--progress` explicitly or git silently omits it.
///
/// Git's progress lines use `\r` to redraw the same line (not `\n`), so a
/// dedicated reader thread splits on either byte rather than treating stderr
/// as normal line-oriented output. That thread also accumulates the full
/// text so a failure still gets a real error message instead of losing it
/// to the progress stream.
///
/// Takes a plain callback instead of a `tauri::Window` directly so the git
/// logic stays testable without a running Tauri app — the real command
/// wraps a window-emitting closure around it (see `fetch_remote`).
fn run_git_progress<F>(repo: &Path, args: &[&str], timeout: Duration, on_progress: F) -> Result<(), String>
where
    F: Fn(&str) + Send + 'static,
{
    let mut cmd = git_command(repo);
    cmd.args(args);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped()).stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW | proc_tree::CREATE_SUSPENDED);
    }
    let mut child = cmd.spawn().map_err(|e| format!("无法执行 git: {e}"))?;
    let pid = child.id();
    #[cfg(windows)]
    let job = proc_tree::JobGuard::new(&child);
    #[cfg(windows)]
    proc_tree::resume_main_thread(pid);

    let mut stderr = child.stderr.take().ok_or("无法读取 git stderr")?;
    let full_stderr = Arc::new(Mutex::new(String::new()));
    let reader_stderr = full_stderr.clone();
    std::thread::spawn(move || {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stderr.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    if byte[0] == b'\r' || byte[0] == b'\n' {
                        if !line.is_empty() {
                            let text = String::from_utf8_lossy(&line).trim().to_string();
                            line.clear();
                            if !text.is_empty() {
                                if let Ok(mut acc) = reader_stderr.lock() {
                                    acc.push_str(&text);
                                    acc.push('\n');
                                }
                                on_progress(&text);
                            }
                        }
                    } else {
                        line.push(byte[0]);
                    }
                }
                Err(_) => break,
            }
        }
    });

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(status)) => {
            record_console(repo, args, status.success());
            if status.success() {
                Ok(())
            } else {
                let msg = full_stderr.lock().map(|s| s.trim().to_string()).unwrap_or_default();
                Err(if msg.is_empty() { "git 命令失败".to_string() } else { msg })
            }
        }
        Ok(Err(e)) => Err(format!("git 执行失败: {e}")),
        Err(_) => {
            #[cfg(windows)]
            match &job {
                Some(j) => j.kill_tree(),
                None => proc_tree::kill_pid_only(pid),
            }
            #[cfg(unix)]
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
            record_console(repo, args, false);
            Err(format!(
                "git {} 超时（超过 {}s 无响应，可能是网络连接卡住），已终止",
                args.join(" "),
                timeout.as_secs()
            ))
        }
    }
}

/// "HEAD" if the repo has at least one commit, otherwise the empty-tree hash,
/// so that diff/status work uniformly in brand-new repos.
fn base_ref(repo: &Path) -> String {
    match run_git(repo, &["rev-parse", "--verify", "--quiet", "HEAD"], None) {
        Ok(_) => "HEAD".to_string(),
        Err(_) => EMPTY_TREE.to_string(),
    }
}

/// Resolved `.git` directory, NOT a naive `repo.join(".git")` — required for
/// linked worktrees / submodules where `.git` is a file pointing elsewhere.
fn git_dir(repo: &Path) -> PathBuf {
    match run_git_text(repo, &["rev-parse", "--git-dir"]) {
        Ok(s) => {
            let p = PathBuf::from(s.trim());
            if p.is_absolute() {
                p
            } else {
                repo.join(p)
            }
        }
        Err(_) => repo.join(".git"),
    }
}

/// Whether the repo is mid-merge / mid-cherry-pick / mid-revert, from the
/// presence of git's own state marker files.
fn detect_operation(repo: &Path) -> RepoOperation {
    let gd = git_dir(repo);
    if gd.join("MERGE_HEAD").exists() {
        RepoOperation::Merge
    } else if gd.join("CHERRY_PICK_HEAD").exists() {
        RepoOperation::CherryPick
    } else if gd.join("REVERT_HEAD").exists() {
        RepoOperation::Revert
    } else if gd.join("rebase-merge").exists() || gd.join("rebase-apply").exists() {
        RepoOperation::Rebase
    } else {
        RepoOperation::None
    }
}

/// (upstream ref, ahead, behind), all None when no upstream is configured —
/// that's a normal state, not an error.
fn ahead_behind(repo: &Path) -> (Option<String>, Option<u32>, Option<u32>) {
    let upstream = run_git_text(
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    let Some(upstream) = upstream else {
        return (None, None, None);
    };
    // left-right count of `upstream...HEAD`: left = upstream-only (behind),
    // right = HEAD-only (ahead)
    let spec = format!("{upstream}...HEAD");
    let counts = run_git_text(repo, &["rev-list", "--left-right", "--count", &spec]).ok();
    let parsed = counts.and_then(|s| {
        let mut it = s.split_whitespace();
        let behind = it.next()?.parse::<u32>().ok()?;
        let ahead = it.next()?.parse::<u32>().ok()?;
        Some((ahead, behind))
    });
    match parsed {
        Some((ahead, behind)) => (Some(upstream), Some(ahead), Some(behind)),
        None => (Some(upstream), None, None),
    }
}

/// Maps `git status --porcelain`'s unmerged XY codes to which side(s) touched
/// the path. Only these 7 codes represent unmerged/conflicted entries.
fn conflict_side(xy: &str) -> Option<ConflictSide> {
    match xy {
        "DD" => Some(ConflictSide::BothDeleted),
        "AU" => Some(ConflictSide::AddedByUs),
        "UD" => Some(ConflictSide::DeletedByThem),
        "UA" => Some(ConflictSide::AddedByThem),
        "DU" => Some(ConflictSide::DeletedByUs),
        "AA" => Some(ConflictSide::BothAdded),
        "UU" => Some(ConflictSide::BothModified),
        _ => None,
    }
}

/// Conflicted (path, side) pairs from `git status --porcelain -z`. Renamed
/// entries consume an extra NUL-separated old-path token; conflicts never
/// carry a rename code, but we must still skip that token to keep the
/// zero-separated stream aligned for entries that follow.
fn conflicted_paths(repo: &Path) -> Vec<(String, ConflictSide)> {
    let out = match run_git_text(repo, &["status", "--porcelain", "-z"]) {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let mut result = Vec::new();
    let mut tokens = out.split('\0');
    while let Some(entry) = tokens.next() {
        if entry.len() < 3 {
            continue;
        }
        let xy = &entry[0..2];
        let path = &entry[3..];
        if xy.contains('R') || xy.contains('C') {
            tokens.next(); // consume the old-path token; conflicts never use R/C
        }
        if let Some(side) = conflict_side(xy) {
            result.push((path.to_string(), side));
        }
    }
    result
}

fn parse_name_status(out: &str) -> Vec<FileStatus> {
    let mut tokens = out.split('\0');
    let mut files = Vec::new();
    while let Some(status) = tokens.next() {
        if status.is_empty() {
            continue;
        }
        let code = status.chars().next().unwrap();
        match code {
            'R' | 'C' => {
                let old = match tokens.next() {
                    Some(p) if !p.is_empty() => p,
                    _ => break,
                };
                let new = match tokens.next() {
                    Some(p) if !p.is_empty() => p,
                    _ => break,
                };
                if code == 'R' {
                    files.push(FileStatus {
                        path: new.to_string(),
                        old_path: Some(old.to_string()),
                        kind: ChangeKind::Renamed,
                        additions: None,
                        deletions: None,
                        conflict: None,
                        staged: false,
                    });
                } else {
                    files.push(FileStatus {
                        path: new.to_string(),
                        old_path: None,
                        kind: ChangeKind::Added,
                        additions: None,
                        deletions: None,
                        conflict: None,
                        staged: false,
                    });
                }
            }
            _ => {
                let path = match tokens.next() {
                    Some(p) if !p.is_empty() => p,
                    _ => break,
                };
                let kind = match code {
                    'A' => ChangeKind::Added,
                    'D' => ChangeKind::Deleted,
                    _ => ChangeKind::Modified, // M, T, U(unmerged) all render as modified
                };
                files.push(FileStatus {
                    path: path.to_string(),
                    old_path: None,
                    kind,
                    additions: None,
                    deletions: None,
                    conflict: None,
                    staged: false,
                });
            }
        }
    }
    files
}

/// numstat -z: "added\tdeleted\tpath\0" per file; renames are
/// "added\tdeleted\t\0old\0new\0". "-" columns mean binary.
/// Returns (new_path, additions, deletions).
fn parse_numstat(out: &str) -> Vec<(String, Option<u32>, Option<u32>)> {
    let mut tokens = out.split('\0');
    let mut stats = Vec::new();
    while let Some(tok) = tokens.next() {
        if tok.is_empty() {
            continue;
        }
        let mut cols = tok.splitn(3, '\t');
        let (Some(a), Some(d), Some(rest)) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        let added = a.parse::<u32>().ok();
        let deleted = d.parse::<u32>().ok();
        let path = if rest.is_empty() {
            // rename: two more NUL-separated tokens, keep the new path
            let _old = tokens.next();
            match tokens.next() {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => continue,
            }
        } else {
            rest.to_string()
        };
        stats.push((path, added, deleted));
    }
    stats
}

/// "@@ -a[,b] +c[,d] @@ ..." -> (a, b, c, d); missing count means 1
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(" +")?;
    let (new_part, _) = rest.split_once(" @@")?;
    let parse_pair = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = parse_pair(old_part)?;
    let (new_start, new_lines) = parse_pair(new_part)?;
    Some((old_start, old_lines, new_start, new_lines))
}

/// Split raw `git diff` output into (file_header, hunks, is_binary).
/// Hunk text is kept verbatim (incl. "\ No newline at end of file" markers)
/// so that file_header + hunk.text is a valid patch for `git apply -R`.
fn parse_diff(diff: &str) -> (String, Vec<Hunk>, bool) {
    if diff
        .lines()
        .any(|l| l.starts_with("Binary files ") || l == "GIT binary patch")
    {
        return (String::new(), Vec::new(), true);
    }
    let mut header = String::new();
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    let mut index = 0usize;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("@@ ") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            let (old_start, old_lines, new_start, new_lines) =
                parse_hunk_header(line).unwrap_or((0, 0, 0, 0));
            current = Some(Hunk {
                index,
                old_start,
                old_lines,
                new_start,
                new_lines,
                text: line.to_string(),
            });
            index += 1;
        } else if let Some(h) = current.as_mut() {
            h.text.push_str(line);
        } else {
            header.push_str(line);
        }
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    (header, hunks, false)
}

fn untracked_diff(repo: &Path, path: &str) -> Result<FileDiff, String> {
    let full = repo.join(path);
    let meta = std::fs::metadata(&full).map_err(|e| format!("无法读取 {path}: {e}"))?;
    if meta.len() > MAX_FILE_SIZE {
        return Ok(FileDiff::too_large());
    }
    let bytes = std::fs::read(&full).map_err(|e| format!("无法读取 {path}: {e}"))?;
    // git's own binary heuristic: NUL byte in the first 8000 bytes
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Ok(FileDiff::binary());
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(FileDiff {
            original: None,
            modified: Some(text),
            is_binary: false,
            too_large: false,
            file_header: String::new(),
            hunks: Vec::new(),
        }),
        Err(_) => Ok(FileDiff::binary()),
    }
}

#[derive(Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// Repo-wide fixed-string search via `git grep` (tracked + untracked,
/// binary files skipped). whole_word approximates symbol lookup.
#[tauri::command]
pub async fn search_text(
    repo: String,
    query: String,
    whole_word: bool,
    max: u32,
) -> Result<Vec<SearchHit>, String> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let repo = PathBuf::from(repo);
    let mut args: Vec<&str> = vec!["grep", "-n", "-I", "--no-color", "--untracked", "-F"];
    if whole_word {
        args.push("-w");
    }
    args.push("-e");
    args.push(&query);
    let out = match run_git(&repo, &args, None) {
        Ok(o) => o,
        // exit code 1 with empty stderr = no matches
        Err(e) if e.is_empty() => Vec::new(),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&out);
    let mut hits = Vec::new();
    for line in text.lines() {
        if hits.len() >= max as usize {
            break;
        }
        let mut parts = line.splitn(3, ':');
        let (Some(p), Some(l), Some(t)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(ln) = l.parse::<u32>() else { continue };
        hits.push(SearchHit {
            path: p.to_string(),
            line: ln,
            text: t.trim_end().chars().take(300).collect(),
        });
    }
    Ok(hits)
}

#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ReplaceResult {
    pub files: u32,
    pub replacements: u32,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// whole-word replace against the ORIGINAL text (boundaries judged on the
/// source, not the partially-built output)
fn replace_whole_word(content: &str, query: &str, replacement: &str) -> (String, u32) {
    let mut out = String::with_capacity(content.len());
    let mut count = 0u32;
    let mut i = 0;
    while let Some(rel) = content[i..].find(query) {
        let start = i + rel;
        let end = start + query.len();
        let before_ok = content[..start].chars().next_back().is_none_or(|c| !is_word_char(c));
        let after_ok = content[end..].chars().next().is_none_or(|c| !is_word_char(c));
        out.push_str(&content[i..start]);
        if before_ok && after_ok {
            out.push_str(replacement);
            count += 1;
        } else {
            out.push_str(&content[start..end]);
        }
        i = end;
    }
    out.push_str(&content[i..]);
    (out, count)
}

/// IDEA-style "Replace in Files": fixed-string replace across the worktree
/// (scoped to `paths` when given). Candidate files come from `git grep -l`,
/// so .gitignore'd and binary files are never touched; non-UTF-8 files are
/// skipped rather than corrupted.
#[tauri::command]
pub async fn replace_in_files(
    repo: String,
    query: String,
    replacement: String,
    whole_word: bool,
    paths: Option<Vec<String>>,
) -> Result<ReplaceResult, String> {
    if query.is_empty() {
        return Err("搜索内容不能为空".to_string());
    }
    let repo = PathBuf::from(repo);
    let mut args: Vec<&str> = vec!["grep", "-l", "-I", "--no-color", "--untracked", "-F"];
    if whole_word {
        args.push("-w");
    }
    args.push("-e");
    args.push(&query);
    let scoped: Vec<String>;
    if let Some(p) = &paths {
        args.push("--");
        scoped = p.clone();
        args.extend(scoped.iter().map(String::as_str));
    }
    let out = match run_git(&repo, &args, None) {
        Ok(o) => o,
        Err(e) if e.is_empty() => Vec::new(), // exit 1 = no matches
        Err(e) => return Err(e),
    };
    let list = String::from_utf8_lossy(&out);
    let mut files = 0u32;
    let mut replacements = 0u32;
    for rel in list.lines().filter(|l| !l.is_empty()) {
        let full = repo.join(rel);
        let Ok(bytes) = std::fs::read(&full) else { continue };
        let Ok(content) = String::from_utf8(bytes) else { continue }; // skip non-UTF-8
        let (next, n) = if whole_word {
            replace_whole_word(&content, &query, &replacement)
        } else {
            (content.replace(&query, &replacement), content.matches(&query).count() as u32)
        };
        if n == 0 {
            continue;
        }
        std::fs::write(&full, next).map_err(|e| format!("写入 {rel} 失败: {e}"))?;
        files += 1;
        replacements += n;
    }
    Ok(ReplaceResult { files, replacements })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageDiff {
    /// data URL of the HEAD version (None for added/untracked)
    pub original: Option<String>,
    /// data URL of the working-tree version (None for deleted)
    pub modified: Option<String>,
}

fn image_mime(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        _ => return None,
    })
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    use base64::Engine;
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Before/after data URLs for an image file (working tree vs HEAD).
#[tauri::command]
pub async fn get_image_diff(
    repo: String,
    path: String,
    old_path: Option<String>,
    kind: ChangeKind,
) -> Result<ImageDiff, String> {
    let repo = PathBuf::from(repo);
    let mime = image_mime(&path).ok_or("不支持的图片格式")?;
    let base = base_ref(&repo);
    let head_rel = old_path.unwrap_or_else(|| path.clone());

    let mut original = None;
    if kind != ChangeKind::Added && kind != ChangeKind::Untracked {
        let spec = format!("{base}:{head_rel}");
        if let Ok(bytes) = run_git(&repo, &["show", &spec], None) {
            if bytes.len() as u64 <= MAX_FILE_SIZE {
                original = Some(data_url(mime, &bytes));
            }
        }
    }
    let mut modified = None;
    if kind != ChangeKind::Deleted {
        let full = repo.join(&path);
        if let Ok(meta) = std::fs::metadata(&full) {
            if meta.len() <= MAX_FILE_SIZE {
                if let Ok(bytes) = std::fs::read(&full) {
                    modified = Some(data_url(mime, &bytes));
                }
            }
        }
    }
    Ok(ImageDiff { original, modified })
}

/// repo to open on launch, from AI_DIFF_OPEN_REPO (or legacy VITE_OPEN_REPO);
/// read on the Rust side so it works regardless of how vite resolves env.
/// Only the main window auto-opens — extra windows start empty.
#[tauri::command]
pub fn auto_open_path(window: tauri::Window) -> Option<String> {
    if window.label() != "main" {
        return None;
    }
    std::env::var("AI_DIFF_OPEN_REPO")
        .or_else(|_| std::env::var("VITE_OPEN_REPO"))
        .ok()
        .filter(|s| !s.trim().is_empty())
}

#[tauri::command]
pub async fn open_repo(path: String) -> Result<RepoInfo, String> {
    let mut p = PathBuf::from(&path);
    // dropping a file counts as its directory; rev-parse walks up to the root
    if p.is_file() {
        if let Some(parent) = p.parent() {
            p = parent.to_path_buf();
        }
    }
    if !p.is_dir() {
        return Err(format!("目录不存在: {path}"));
    }
    let root = run_git_text(&p, &["rev-parse", "--show-toplevel"])
        .map_err(|_| "该目录不是 git 仓库".to_string())?;
    let root = root.trim().to_string();
    let root_path = PathBuf::from(&root);
    let has_head = base_ref(&root_path) == "HEAD";
    let branch = run_git_text(&root_path, &["branch", "--show-current"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let (upstream, ahead, behind) = ahead_behind(&root_path);
    let operation = detect_operation(&root_path);
    Ok(RepoInfo {
        root,
        branch,
        has_head,
        upstream,
        ahead,
        behind,
        operation,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    pub content: Option<String>,
    pub is_binary: bool,
    pub too_large: bool,
}

#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileListing {
    /// every path shown in the project view, sorted, repo-relative
    pub files: Vec<String>,
    /// the subset of `files` that .gitignore excludes — the UI dims these
    pub ignored: Vec<String>,
}

fn split_nul(out: &str) -> impl Iterator<Item = &str> {
    out.split('\0').filter(|s| !s.is_empty())
}

/// How much of what .gitignore excludes the 全部文件 tree lists.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum IgnoredMode {
    /// nothing ignored — tracked + untracked only
    Hide,
    /// ignored files sitting in the normal tree (local.properties, .env,
    /// *.iml …) are listed (dimmed); wholly ignored directories
    /// (node_modules, build/ …) stay out — they are bulk, not the user's files
    Files,
    /// everything, ignored directories' contents included
    All,
}

/// All files in the project view: tracked (incl. staged) + untracked, plus
/// whatever `mode` admits of the gitignored set — flagged in `ignored` so the
/// UI can dim it. The .git directory is always skipped by git itself.
#[tauri::command]
pub async fn list_files(repo: String, mode: IgnoredMode) -> Result<FileListing, String> {
    let repo = PathBuf::from(repo);
    let out = run_git_text(&repo, &["ls-files", "--cached", "--others", "--exclude-standard", "-z"])?;
    let mut files: Vec<String> = split_nul(&out).map(str::to_string).collect();
    if mode == IgnoredMode::Hide {
        files.sort();
        files.dedup();
        return Ok(FileListing { files, ignored: Vec::new() });
    }
    // --directory collapses a wholly ignored dir to one "dir/" entry, which
    // we drop; without it every file under node_modules etc. comes back
    let mut args = vec!["ls-files", "--others", "--ignored", "--exclude-standard"];
    if mode == IgnoredMode::Files {
        args.push("--directory");
    }
    args.push("-z");
    let out = run_git_text(&repo, &args)?;
    let mut ignored: Vec<String> = split_nul(&out)
        .filter(|s| !s.ends_with('/'))
        .map(str::to_string)
        .collect();
    ignored.sort();
    ignored.dedup();
    files.extend(ignored.iter().cloned());
    files.sort();
    files.dedup();
    Ok(FileListing { files, ignored })
}

#[tauri::command]
pub async fn read_file(repo: String, path: String) -> Result<FileContent, String> {
    let full = PathBuf::from(&repo).join(&path);
    let meta = std::fs::metadata(&full).map_err(|e| format!("无法读取 {path}: {e}"))?;
    if meta.len() > MAX_FILE_SIZE {
        return Ok(FileContent {
            content: None,
            is_binary: false,
            too_large: true,
        });
    }
    let bytes = std::fs::read(&full).map_err(|e| format!("无法读取 {path}: {e}"))?;
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Ok(FileContent {
            content: None,
            is_binary: true,
            too_large: false,
        });
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(FileContent {
            content: Some(text),
            is_binary: false,
            too_large: false,
        }),
        Err(_) => Ok(FileContent {
            content: None,
            is_binary: true,
            too_large: false,
        }),
    }
}

/// Saves the working-tree file's full text content (全部文件 mode's single-file
/// editor) — a plain overwrite, not a git operation; `list_files`/`get_status`
/// picks up the resulting modification the same as an edit made outside the app.
#[tauri::command]
pub async fn write_file(repo: String, path: String, content: String) -> Result<(), String> {
    let full = PathBuf::from(&repo).join(&path);
    std::fs::write(&full, content).map_err(|e| format!("无法保存 {path}: {e}"))
}

/// Files above this are skipped for stats (vendored bundles, lockfiles-as-code,
/// generated data) — they'd both skew the language breakdown and slow this
/// command down without telling you anything about the project's actual code.
const STATS_FILE_CAP: u64 = 2 * 1024 * 1024;

/// Extension (and a few bare filenames) to (language, linguist-style color)
/// — not exhaustive, just the languages a typical repo mixes; anything
/// unrecognized (images, fonts, lockfiles, …) is left out of the breakdown
/// entirely rather than lumped into a misleading "Other" bucket.
fn language_for(rel_path: &str) -> Option<(&'static str, &'static str)> {
    let lower = rel_path.to_lowercase();
    let file_name = Path::new(&lower).file_name().and_then(|f| f.to_str()).unwrap_or("");
    match file_name {
        "dockerfile" => return Some(("Dockerfile", "#384d54")),
        "makefile" | "gnumakefile" => return Some(("Makefile", "#427819")),
        "cmakelists.txt" => return Some(("CMake", "#da3434")),
        _ => {}
    }
    let ext = Path::new(&lower).extension().and_then(|e| e.to_str())?;
    Some(match ext {
        "rs" => ("Rust", "#dea584"),
        "ts" | "tsx" | "mts" | "cts" => ("TypeScript", "#3178c6"),
        "js" | "jsx" | "mjs" | "cjs" => ("JavaScript", "#f1e05a"),
        "vue" => ("Vue", "#41b883"),
        "css" => ("CSS", "#563d7c"),
        "scss" | "sass" => ("SCSS", "#c6538c"),
        "less" => ("Less", "#1d365d"),
        "html" | "htm" => ("HTML", "#e34c26"),
        "json" | "jsonc" => ("JSON", "#292929"),
        "md" | "markdown" => ("Markdown", "#083fa1"),
        "py" => ("Python", "#3572a5"),
        "go" => ("Go", "#00add8"),
        "java" => ("Java", "#b07219"),
        "kt" | "kts" => ("Kotlin", "#a97bff"),
        "c" | "h" => ("C", "#555555"),
        "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ipp" | "inl" => ("C++", "#f34b7d"),
        "cs" => ("C#", "#178600"),
        "sh" | "bash" | "zsh" => ("Shell", "#89e051"),
        "ps1" | "psm1" => ("PowerShell", "#012456"),
        "yml" | "yaml" => ("YAML", "#cb171e"),
        "toml" => ("TOML", "#9c4221"),
        "sql" => ("SQL", "#e38c00"),
        "php" => ("PHP", "#4f5d95"),
        "rb" => ("Ruby", "#701516"),
        "swift" => ("Swift", "#f05138"),
        "dart" => ("Dart", "#00b4ab"),
        "xml" => ("XML", "#0060ac"),
        "lua" => ("Lua", "#000080"),
        "qml" => ("QML", "#44a51c"),
        "pro" | "pri" | "prf" => ("QMake", "#41cd52"),
        "ui" => ("Qt UI", "#41cd52"),
        "qrc" => ("Qt Resource", "#41cd52"),
        "qss" => ("Qt Style Sheet", "#41cd52"),
        "cmake" => ("CMake", "#da3434"),
        "aidl" => ("AIDL", "#34eb92"),
        "hidl" => ("HIDL", "#34eb92"),
        _ => return None,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LangStat {
    pub language: String,
    pub color: String,
    pub lines: u64,
    pub files: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoStats {
    pub total_files: u32,
    pub total_lines: u64,
    pub languages: Vec<LangStat>,
    pub skipped_binary: u32,
    pub skipped_too_large: u32,
}

/// GitHub-style "what's this project made of": line counts per language
/// across tracked files (mirrors linguist's tracked-files-only scope — an
/// untracked build artifact shouldn't count toward "what language is this
/// project"). Binary/oversized files are counted but excluded from the
/// language breakdown rather than silently dropped from the totals.
#[tauri::command]
pub async fn repo_stats(repo: String) -> Result<RepoStats, String> {
    let repo_path = PathBuf::from(&repo);
    let out = run_git_text(&repo_path, &["ls-files", "-z"])?;

    let mut by_lang: std::collections::HashMap<&'static str, (u64, u32, &'static str)> = std::collections::HashMap::new();
    let mut total_lines: u64 = 0;
    let mut total_files: u32 = 0;
    let mut skipped_binary = 0u32;
    let mut skipped_too_large = 0u32;

    for rel in out.split('\0').filter(|s| !s.is_empty()) {
        let Some((lang, color)) = language_for(rel) else { continue };
        let full = repo_path.join(rel);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        if meta.len() > STATS_FILE_CAP {
            skipped_too_large += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else { continue };
        if bytes.iter().take(8000).any(|&b| b == 0) {
            skipped_binary += 1;
            continue;
        }
        let mut lines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
        if !bytes.is_empty() && bytes[bytes.len() - 1] != b'\n' {
            lines += 1;
        }
        total_lines += lines;
        total_files += 1;
        let entry = by_lang.entry(lang).or_insert((0, 0, color));
        entry.0 += lines;
        entry.1 += 1;
    }

    let mut languages: Vec<LangStat> = by_lang
        .into_iter()
        .map(|(language, (lines, files, color))| LangStat {
            language: language.to_string(),
            color: color.to_string(),
            lines,
            files,
        })
        .collect();
    languages.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.language.cmp(&b.language)));

    Ok(RepoStats {
        total_files,
        total_lines,
        languages,
        skipped_binary,
        skipped_too_large,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub name: String,
    /// path relative to the repo root (as shown in the tree)
    pub path: String,
    /// absolute path on disk
    pub full_path: String,
    /// false for deleted files that no longer exist in the working tree
    pub exists: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub readonly: bool,
    /// modification / creation time as unix-epoch milliseconds, when available
    pub modified: Option<u64>,
    pub created: Option<u64>,
    /// best-effort line count; None for binary/large/missing files
    pub lines: Option<u32>,
}

fn system_time_millis(t: std::io::Result<std::time::SystemTime>) -> Option<u64> {
    t.ok()
        .and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

/// Filesystem metadata for the file/dir at `path` (relative to `repo`), shown
/// in the "查看文件信息" dialog. Missing files (e.g. deleted) return exists=false
/// rather than an error so the dialog can still report the path.
#[tauri::command]
pub async fn file_info(repo: String, path: String) -> Result<FileInfo, String> {
    let full = PathBuf::from(&repo).join(&path);
    let name = Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    let full_path = full.to_string_lossy().into_owned();
    let is_symlink = std::fs::symlink_metadata(&full)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

    match std::fs::metadata(&full) {
        Ok(meta) => Ok(FileInfo {
            name,
            path,
            full_path,
            exists: true,
            is_dir: meta.is_dir(),
            is_symlink,
            size: meta.len(),
            readonly: meta.permissions().readonly(),
            modified: system_time_millis(meta.modified()),
            created: system_time_millis(meta.created()),
            lines: if meta.is_dir() { None } else { count_lines(&full) },
        }),
        Err(_) => Ok(FileInfo {
            name,
            path,
            full_path,
            exists: false,
            is_dir: false,
            is_symlink,
            size: 0,
            readonly: false,
            modified: None,
            created: None,
            lines: None,
        }),
    }
}

/// Creates an empty file at `path` (parent dirs created as needed). Does not
/// stage it — IDEA-style "new = untracked", the user decides when to add it.
/// Errors if something already exists there.
#[tauri::command]
pub async fn create_file(repo: String, path: String) -> Result<(), String> {
    let full = PathBuf::from(&repo).join(&path);
    if full.exists() {
        return Err(format!("{path} 已存在"));
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    std::fs::write(&full, b"").map_err(|e| format!("创建文件失败: {e}"))
}

/// Creates a directory (and any missing parents) at `path`. Errors if
/// something already exists there.
#[tauri::command]
pub async fn create_dir(repo: String, path: String) -> Result<(), String> {
    let full = PathBuf::from(&repo).join(&path);
    if full.exists() {
        return Err(format!("{path} 已存在"));
    }
    std::fs::create_dir_all(&full).map_err(|e| format!("创建目录失败: {e}"))
}

/// Recent git invocations for `repo`, most-recent-last, shown in the Git
/// panel's 控制台 tab. Backed by an in-process ring buffer (capacity 500
/// across all repos) — not persisted, resets on app restart.
#[tauri::command]
pub async fn get_console_log(repo: String) -> Result<Vec<ConsoleEntry>, String> {
    let log = console_log().lock().unwrap();
    Ok(log.iter().filter(|e| e.root == repo).cloned().collect())
}

/// best-effort line count of an untracked file; None for binary/large
fn count_lines(full: &Path) -> Option<u32> {
    let meta = std::fs::metadata(full).ok()?;
    if meta.len() > 1024 * 1024 {
        return None;
    }
    let bytes = std::fs::read(full).ok()?;
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return None;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count() as u32;
    Some(newlines + if bytes.last().is_some_and(|&b| b != b'\n') { 1 } else { 0 })
}

/// Refresh the index's stat cache so `git diff HEAD` doesn't report a stale
/// "clean" tree. Without this, the first status read after opening a repo can
/// miss real changes (git's stat shortcut skips content comparison when the
/// cached stat still matches); `git status` does this implicitly, we don't.
/// Exits non-zero when entries need updating — that's expected, so ignore it.
fn refresh_index(repo: &Path) {
    let _ = run_git(repo, &["update-index", "-q", "--refresh"], None);
}

/// `refs` is the ref-related prefix to `git diff` (e.g. `["--cached", "HEAD"]`,
/// `["parent", "hash"]`, or `[]` for a plain index-vs-worktree diff).
fn attach_numstat(repo: &Path, refs: &[&str], files: &mut [FileStatus]) {
    let mut args = vec!["diff"];
    args.extend_from_slice(refs);
    args.extend(["--numstat", "-z", "-M", "--no-color", "--no-ext-diff"]);
    if let Ok(out) = run_git_text(repo, &args) {
        for (path, added, deleted) in parse_numstat(&out) {
            if let Some(f) = files.iter_mut().find(|f| f.path == path) {
                f.additions = added;
                f.deletions = deleted;
            }
        }
    }
}

#[tauri::command]
pub async fn get_status(repo: String) -> Result<Vec<FileStatus>, String> {
    let repo = PathBuf::from(repo);
    refresh_index(&repo);
    // conflicted paths are reported via their own kind — their plain `diff`
    // output would otherwise be confusing (it shows one arbitrary stage),
    // so exclude them from the regular staged/unstaged passes below.
    let conflicted = conflicted_paths(&repo);
    let conflicted_set: std::collections::HashSet<&str> =
        conflicted.iter().map(|(p, _)| p.as_str()).collect();
    let base = base_ref(&repo);

    // staged: index vs HEAD (or the empty tree, in a repo with no commits yet)
    let staged_out = run_git_text(
        &repo,
        &[
            "diff",
            "--cached",
            &base,
            "--name-status",
            "-z",
            "-M",
            "--no-color",
            "--no-ext-diff",
        ],
    )?;
    let mut staged_files = parse_name_status(&staged_out);
    staged_files.retain(|f| !conflicted_set.contains(f.path.as_str()));
    attach_numstat(&repo, &["--cached", &base], &mut staged_files);
    for f in &mut staged_files {
        f.staged = true;
    }

    // unstaged: worktree vs index — deliberately no ref at all, so a
    // partially-staged file's remaining worktree edit shows up here too
    let unstaged_out = run_git_text(
        &repo,
        &["diff", "--name-status", "-z", "-M", "--no-color", "--no-ext-diff"],
    )?;
    let mut unstaged_files = parse_name_status(&unstaged_out);
    unstaged_files.retain(|f| !conflicted_set.contains(f.path.as_str()));
    attach_numstat(&repo, &[], &mut unstaged_files);

    let mut files = staged_files;
    files.append(&mut unstaged_files);

    let untracked = run_git_text(&repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for p in untracked.split('\0').filter(|s| !s.is_empty()) {
        if conflicted_set.contains(p) {
            continue;
        }
        files.push(FileStatus {
            path: p.to_string(),
            old_path: None,
            kind: ChangeKind::Untracked,
            additions: count_lines(&repo.join(p)),
            deletions: Some(0),
            conflict: None,
            staged: false,
        });
    }
    for (path, side) in conflicted {
        files.push(FileStatus {
            path,
            old_path: None,
            kind: ChangeKind::Conflicted,
            additions: None,
            deletions: None,
            conflict: Some(side),
            staged: false,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// first parent of a commit, or the empty tree for root commits —
/// both forms are valid `git diff` operands
fn parent_ref(repo: &Path, hash: &str) -> String {
    let spec = format!("{hash}^1");
    match run_git(repo, &["rev-parse", "--verify", "--quiet", &spec], None) {
        Ok(_) => spec,
        Err(_) => EMPTY_TREE.to_string(),
    }
}

/// Parses `%D`'s ref-name decoration string (e.g. "HEAD -> main, origin/main,
/// tag: v1.0") into cleaned individual names (["main", "origin/main", "v1.0"]).
/// A bare "HEAD" (detached) isn't a real ref, so it's dropped.
fn parse_decorations(raw: &str) -> Vec<String> {
    raw.split(", ")
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() || part == "HEAD" {
                return None;
            }
            let part = part.strip_prefix("HEAD -> ").unwrap_or(part);
            let part = part.strip_prefix("tag: ").unwrap_or(part);
            Some(part.to_string())
        })
        .collect()
}

#[tauri::command]
pub async fn log_commits(
    repo: String,
    skip: u32,
    count: u32,
    branch: Option<String>,
    author: Option<String>,
) -> Result<Vec<CommitInfo>, String> {
    let repo = PathBuf::from(repo);
    if base_ref(&repo) != "HEAD" {
        return Ok(Vec::new()); // empty repo: no history yet
    }
    let skip_arg = format!("--skip={skip}");
    let count_arg = format!("--max-count={count}");
    // \x01 marks each record start; --shortstat appends a
    // " N files changed, X insertions(+), Y deletions(-)" line per commit
    let mut args: Vec<&str> = vec!["log"];
    if let Some(b) = &branch {
        args.push(b); // scope the log to this branch/ref instead of HEAD
    }
    let author_arg = author.as_ref().map(|a| format!("--author={a}"));
    if let Some(a) = &author_arg {
        // --fixed-strings keeps the name from being read as a regex
        args.push("--fixed-strings");
        args.push(a);
    }
    args.extend([
        &skip_arg,
        &count_arg,
        "--shortstat",
        "--date=format:%Y-%m-%d %H:%M",
        "--pretty=format:%x01%H%x00%h%x00%an%x00%ae%x00%ad%x00%P%x00%s%x00%D",
    ]);
    let out = run_git_text(&repo, &args)?;
    let mut commits = Vec::new();
    for block in out.split('\x01').filter(|b| !b.is_empty()) {
        let mut lines = block.lines();
        let Some(head) = lines.next() else { continue };
        let cols: Vec<&str> = head.splitn(8, '\0').collect();
        if cols.len() != 8 {
            continue;
        }
        let (mut additions, mut deletions) = (0u32, 0u32);
        for line in lines {
            if !line.contains(" changed") {
                continue;
            }
            for seg in line.split(',') {
                let n = seg.trim().split(' ').next().and_then(|s| s.parse::<u32>().ok());
                if seg.contains("insertion") {
                    additions = n.unwrap_or(0);
                } else if seg.contains("deletion") {
                    deletions = n.unwrap_or(0);
                }
            }
        }
        let parents = cols[5].split_whitespace().map(String::from).collect();
        commits.push(CommitInfo {
            hash: cols[0].to_string(),
            short_hash: cols[1].to_string(),
            author: cols[2].to_string(),
            email: cols[3].to_string(),
            date: cols[4].to_string(),
            subject: cols[6].to_string(),
            additions,
            deletions,
            parents,
            refs: parse_decorations(cols[7]),
        });
    }
    Ok(commits)
}

/// Scans history in batches (reusing `log_commits`), matching `query`
/// case-insensitively against hash / author / email / subject, until either
/// `max_results` matches are found or `SCAN_CAP` commits have been scanned —
/// git has no single revision-walk flag that ORs a message/author/hash
/// substring together, so filtering happens on our side instead.
#[tauri::command]
pub async fn search_commits(
    repo: String,
    branch: Option<String>,
    query: String,
    max_results: u32,
    author: Option<String>,
    subject: Option<String>,
    hash: Option<String>,
) -> Result<Vec<CommitInfo>, String> {
    const SCAN_CAP: u32 = 10_000;
    const BATCH: u32 = 500;
    let q = query.trim().to_lowercase();
    let author = author.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let subject = subject.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let hash_q = hash.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    if q.is_empty() && author.is_none() && subject.is_none() && hash_q.is_none() {
        return Ok(Vec::new());
    }
    let mut matches = Vec::new();
    let mut skip = 0u32;
    while skip < SCAN_CAP && (matches.len() as u32) < max_results {
        // the author qualifier filters at the git level (--author)
        let batch = log_commits(repo.clone(), skip, BATCH, branch.clone(), author.clone()).await?;
        let got = batch.len() as u32;
        for c in batch {
            let mut hit = true;
            if let Some(s) = &subject {
                hit &= c.subject.to_lowercase().contains(s);
            }
            if let Some(h) = &hash_q {
                hit &= c.hash.to_lowercase().starts_with(h) || c.short_hash.to_lowercase().starts_with(h);
            }
            if !q.is_empty() {
                hit &= c.hash.to_lowercase().contains(&q)
                    || c.short_hash.to_lowercase().contains(&q)
                    || c.author.to_lowercase().contains(&q)
                    || c.email.to_lowercase().contains(&q)
                    || c.subject.to_lowercase().contains(&q);
            }
            if hit {
                matches.push(c);
                if matches.len() as u32 >= max_results {
                    break;
                }
            }
        }
        if got < BATCH {
            break; // exhausted history
        }
        skip += BATCH;
    }
    Ok(matches)
}

/// Distinct commit authors with commit counts (IDEA-style log author filter)
#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AuthorInfo {
    pub name: String,
    pub email: String,
    pub commits: u32,
}

#[tauri::command]
pub async fn list_authors(repo: String) -> Result<Vec<AuthorInfo>, String> {
    let repo = PathBuf::from(repo);
    if base_ref(&repo) != "HEAD" {
        return Ok(Vec::new()); // empty repo
    }
    // "  12\tName <email>" per line, ordered by commit count descending
    let out = run_git_text(&repo, &["shortlog", "-sne", "HEAD"])?;
    let mut authors = Vec::new();
    for line in out.lines() {
        let Some((count, rest)) = line.trim().split_once('\t') else {
            continue;
        };
        let commits = count.trim().parse::<u32>().unwrap_or(0);
        let (name, email) = match rest.rsplit_once(" <") {
            Some((n, e)) => (n.trim().to_string(), e.trim_end_matches('>').to_string()),
            None => (rest.trim().to_string(), String::new()),
        };
        authors.push(AuthorInfo { name, email, commits });
    }
    Ok(authors)
}

/// Full commit message (subject + body) — `log_commits`' `%s` only carries the
/// subject, which would silently drop a multi-line body if used to prefill an
/// edit-message dialog.
#[tauri::command]
pub async fn get_commit_message(repo: String, hash: String) -> Result<String, String> {
    let repo = PathBuf::from(repo);
    let out = run_git_text(&repo, &["log", "-1", "--format=%B", &hash])?;
    Ok(out.trim_end_matches('\n').to_string())
}

/// per-line git blame of the working-tree file — powers the IDEA-style
/// "显示提交信息" line-number annotations
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BlameLine {
    pub hash: String,
    pub author: String,
    /// author date as unix seconds; 0 for not-yet-committed lines
    pub time: i64,
    pub summary: String,
    /// false for lines that only exist in the working tree (all-zero hash)
    pub committed: bool,
}

#[tauri::command]
pub async fn blame_file(repo: String, path: String) -> Result<Vec<BlameLine>, String> {
    let root = PathBuf::from(&repo);
    // content lines may be any encoding, so parse lossily; only headers matter
    let bytes = run_git(&root, &["blame", "--line-porcelain", "--", &path], None)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = Vec::new();
    let mut cur: Option<BlameLine> = None;
    for l in text.lines() {
        if l.starts_with('\t') {
            // the tab-prefixed content line closes one record
            if let Some(b) = cur.take() {
                lines.push(b);
            }
            continue;
        }
        let Some(b) = cur.as_mut() else {
            // "<40-hex> <orig-line> <final-line> [group-size]" starts a record
            let hash = l.split(' ').next().unwrap_or("");
            if hash.len() == 40 && hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                cur = Some(BlameLine {
                    committed: !hash.bytes().all(|c| c == b'0'),
                    hash: hash.to_string(),
                    author: String::new(),
                    time: 0,
                    summary: String::new(),
                });
            }
            continue;
        };
        if let Some(v) = l.strip_prefix("author ") {
            b.author = v.to_string();
        } else if let Some(v) = l.strip_prefix("author-time ") {
            b.time = v.parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("summary ") {
            b.summary = v.to_string();
        }
    }
    Ok(lines)
}

#[tauri::command]
pub async fn commit_files(repo: String, hash: String) -> Result<Vec<FileStatus>, String> {
    let repo = PathBuf::from(repo);
    let parent = parent_ref(&repo, &hash);
    let out = run_git_text(
        &repo,
        &[
            "diff",
            &parent,
            &hash,
            "--name-status",
            "-z",
            "-M",
            "--no-color",
            "--no-ext-diff",
        ],
    )?;
    let mut files = parse_name_status(&out);
    attach_numstat(&repo, &[&parent, &hash], &mut files);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Diff of one file within a historical commit (parent vs commit).
/// No hunks/file_header: history view is read-only, nothing to revert.
#[tauri::command]
pub async fn get_commit_file_diff(
    repo: String,
    hash: String,
    path: String,
    old_path: Option<String>,
    kind: ChangeKind,
) -> Result<FileDiff, String> {
    let repo = PathBuf::from(repo);
    let parent = parent_ref(&repo, &hash);
    let head_rel = old_path.unwrap_or_else(|| path.clone());

    for (skip, spec) in [
        (kind == ChangeKind::Added || kind == ChangeKind::Untracked, format!("{parent}:{head_rel}")),
        (kind == ChangeKind::Deleted, format!("{hash}:{path}")),
    ] {
        if !skip {
            if let Ok(sz) = run_git_text(&repo, &["cat-file", "-s", &spec]) {
                if sz.trim().parse::<u64>().unwrap_or(0) > MAX_FILE_SIZE {
                    return Ok(FileDiff::too_large());
                }
            }
        }
    }

    let mut original = None;
    let mut modified = None;
    let mut is_binary = false;
    if kind != ChangeKind::Added && kind != ChangeKind::Untracked {
        let spec = format!("{parent}:{head_rel}");
        match String::from_utf8(run_git(&repo, &["show", &spec], None)?) {
            Ok(s) => original = Some(s),
            Err(_) => is_binary = true,
        }
    }
    if kind != ChangeKind::Deleted && !is_binary {
        let spec = format!("{hash}:{path}");
        match String::from_utf8(run_git(&repo, &["show", &spec], None)?) {
            Ok(s) => modified = Some(s),
            Err(_) => is_binary = true,
        }
    }
    if is_binary {
        return Ok(FileDiff::binary());
    }
    Ok(FileDiff {
        original,
        modified,
        is_binary: false,
        too_large: false,
        file_header: String::new(),
        hunks: Vec::new(),
    })
}

/// `staged=true`: index vs HEAD (survives further worktree edits on top).
/// `staged=false`: worktree vs index (the "unstaged" remainder of a
/// partially-staged file). These are genuinely different comparisons, not
/// just plumbing — a file can have independent staged and unstaged hunks.
#[tauri::command]
pub async fn get_file_diff(
    repo: String,
    path: String,
    old_path: Option<String>,
    kind: ChangeKind,
    staged: bool,
) -> Result<FileDiff, String> {
    let repo = PathBuf::from(repo);

    if kind == ChangeKind::Untracked {
        return untracked_diff(&repo, &path);
    }
    refresh_index(&repo); // avoid stale-clean hunks (see get_status)

    let wt_path = repo.join(&path);
    // the "old" side's rev-spec: HEAD-relative for staged (renames live at the
    // old path there), index-relative (at the current path) for unstaged —
    // once staged, a rename has already happened in the index.
    let old_spec = if staged {
        let head_rel = old_path.clone().unwrap_or_else(|| path.clone());
        format!("{}:{head_rel}", base_ref(&repo))
    } else {
        format!(":{path}")
    };

    if kind != ChangeKind::Deleted {
        if staged {
            let new_spec = format!(":{path}");
            if let Ok(sz) = run_git_text(&repo, &["cat-file", "-s", &new_spec]) {
                if sz.trim().parse::<u64>().unwrap_or(0) > MAX_FILE_SIZE {
                    return Ok(FileDiff::too_large());
                }
            }
        } else if let Ok(meta) = std::fs::metadata(&wt_path) {
            if meta.len() > MAX_FILE_SIZE {
                return Ok(FileDiff::too_large());
            }
        }
    }
    if kind != ChangeKind::Added {
        if let Ok(sz) = run_git_text(&repo, &["cat-file", "-s", &old_spec]) {
            if sz.trim().parse::<u64>().unwrap_or(0) > MAX_FILE_SIZE {
                return Ok(FileDiff::too_large());
            }
        }
    }

    let mut args: Vec<String> = vec!["diff".into()];
    if staged {
        args.push("--cached".into());
        args.push(base_ref(&repo));
    }
    args.extend(["--no-color", "--no-ext-diff", "--unified=3", "-M", "--"].map(String::from));
    if let Some(old) = &old_path {
        args.push(old.clone());
    }
    args.push(path.clone());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let diff_bytes = run_git(&repo, &arg_refs, None)?;
    let (file_header, hunks, mut is_binary) = match String::from_utf8(diff_bytes) {
        Ok(text) => parse_diff(&text),
        Err(_) => (String::new(), Vec::new(), true),
    };

    let mut original = None;
    let mut modified = None;
    if !is_binary && kind != ChangeKind::Added {
        match String::from_utf8(run_git(&repo, &["show", &old_spec], None)?) {
            Ok(s) => original = Some(s),
            Err(_) => is_binary = true,
        }
    }
    if !is_binary && kind != ChangeKind::Deleted {
        if staged {
            let new_spec = format!(":{path}");
            match String::from_utf8(run_git(&repo, &["show", &new_spec], None)?) {
                Ok(s) => modified = Some(s),
                Err(_) => is_binary = true,
            }
        } else {
            let bytes = std::fs::read(&wt_path).map_err(|e| format!("无法读取 {path}: {e}"))?;
            match String::from_utf8(bytes) {
                Ok(s) => modified = Some(s),
                Err(_) => is_binary = true,
            }
        }
    }

    if is_binary {
        return Ok(FileDiff::binary());
    }
    Ok(FileDiff {
        original,
        modified,
        is_binary: false,
        too_large: false,
        file_header,
        hunks,
    })
}

#[tauri::command]
pub async fn revert_file(
    repo: String,
    path: String,
    old_path: Option<String>,
    kind: ChangeKind,
) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    match kind {
        ChangeKind::Modified | ChangeKind::Deleted => {
            run_git(
                &repo,
                &["restore", "--source=HEAD", "--staged", "--worktree", "--", &path],
                None,
            )?;
        }
        ChangeKind::Added => {
            run_git(&repo, &["rm", "--cached", "-f", "--", &path], None)?;
            std::fs::remove_file(repo.join(&path)).map_err(|e| format!("删除 {path} 失败: {e}"))?;
        }
        ChangeKind::Untracked => {
            std::fs::remove_file(repo.join(&path)).map_err(|e| format!("删除 {path} 失败: {e}"))?;
        }
        ChangeKind::Renamed => {
            let old = old_path.ok_or("重命名缺少原路径")?;
            run_git(
                &repo,
                &["restore", "--source=HEAD", "--staged", "--worktree", "--", &old],
                None,
            )?;
            run_git(&repo, &["rm", "--cached", "-f", "--", &path], None)?;
            std::fs::remove_file(repo.join(&path)).map_err(|e| format!("删除 {path} 失败: {e}"))?;
        }
        ChangeKind::Conflicted => {
            return Err("该文件存在合并冲突，请先解决冲突".to_string());
        }
    }
    Ok(())
}

/// delete the given worktree files or directories from disk. Tracked files
/// then show up as "deleted" changes (still recoverable via revert);
/// untracked ones are gone. Directories are removed recursively.
#[tauri::command]
pub async fn delete_paths(repo: String, paths: Vec<String>) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    for path in &paths {
        let full = repo.join(path);
        if full.is_dir() {
            std::fs::remove_dir_all(&full).map_err(|e| format!("删除 {path} 失败: {e}"))?;
        } else {
            std::fs::remove_file(&full).map_err(|e| format!("删除 {path} 失败: {e}"))?;
        }
    }
    Ok(())
}

/// IDEA-shelve-style stash entry. `hash` is the stash commit itself (diff it
/// against its first parent to see the shelved tracked changes); untracked
/// files live in the third-parent commit, exposed as `untracked_hash`.
#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StashInfo {
    pub index: u32,
    pub message: String,
    pub date: String,
    pub hash: String,
    pub untracked_hash: Option<String>,
}

#[tauri::command]
pub async fn list_stashes(repo: String) -> Result<Vec<StashInfo>, String> {
    let repo = PathBuf::from(repo);
    // NB: with --date set, %gd renders stash@{<date>} instead of stash@{N},
    // so the index comes from the line position — stash list is 0..n ordered.
    // %P delivers the untracked-files commit (3rd parent) in the same call —
    // a per-entry rev-parse here made the shelf feel frozen on click.
    let out = run_git(
        &repo,
        &[
            "stash",
            "list",
            "--date=format:%Y-%m-%d %H:%M",
            "--format=%H%x00%P%x00%ad%x00%gs",
        ],
        None,
    )?;
    let text = String::from_utf8_lossy(&out);
    let mut list = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let cols: Vec<&str> = line.split('\u{0}').collect();
        if cols.len() < 4 {
            continue;
        }
        list.push(StashInfo {
            index: i as u32,
            hash: cols[0].to_string(),
            untracked_hash: cols[1].split(' ').nth(2).map(str::to_string),
            date: cols[2].to_string(),
            message: cols[3].to_string(),
        });
    }
    Ok(list)
}

/// IDEA-style shelve: move the given files' worktree changes onto the stash
/// shelf (untracked files included), leaving the rest of the worktree alone.
#[tauri::command]
pub async fn stash_push(repo: String, message: String, paths: Vec<String>) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    if paths.is_empty() {
        return Err("没有要搁置的文件".to_string());
    }
    let mut args: Vec<&str> = vec!["stash", "push", "--include-untracked"];
    let msg = message.trim();
    if !msg.is_empty() {
        args.push("-m");
        args.push(msg);
    }
    args.push("--");
    args.extend(paths.iter().map(String::as_str));
    run_git(&repo, &args, None)?;
    Ok(())
}

/// unshelve: re-apply the entry to the worktree and drop it on success
#[tauri::command]
pub async fn stash_pop(repo: String, index: u32) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let r = format!("stash@{{{index}}}");
    run_git(&repo, &["stash", "pop", &r], None)?;
    Ok(())
}

#[tauri::command]
pub async fn stash_drop(repo: String, index: u32) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let r = format!("stash@{{{index}}}");
    run_git(&repo, &["stash", "drop", &r], None)?;
    Ok(())
}

#[tauri::command]
pub async fn revert_hunk(repo: String, path: String, patch: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let mut patch = patch;
    if !patch.ends_with('\n') {
        patch.push('\n');
    }
    let first = run_git(
        &repo,
        &["apply", "-R", "--whitespace=nowarn", "-"],
        Some(patch.as_bytes()),
    );
    if let Err(e1) = first {
        run_git(
            &repo,
            &["apply", "-R", "--whitespace=nowarn", "--ignore-whitespace", "-"],
            Some(patch.as_bytes()),
        )
        .map_err(|e2| format!("还原失败: {e1}（宽松空白重试: {e2}）"))?;
    }
    // apply -R only touches the worktree; re-point a possibly-staged index entry at HEAD
    // so the HEAD-vs-worktree model stays consistent. Best-effort: fails in empty repos.
    if base_ref(&repo) == "HEAD" {
        let _ = run_git(&repo, &["restore", "--staged", "--", &path], None);
        // apply -R writes the blob's (clean-filtered) bytes, so under autocrlf=true a
        // CRLF worktree file comes back LF; the index entry's recorded eol-convert
        // state also goes stale, making `git status` report a phantom modification.
        // Once the file is filter-clean vs HEAD, a forced path checkout rewrites
        // worktree (through smudge) AND the index entry, fixing both.
        if run_git(&repo, &["diff", "--quiet", "HEAD", "--", &path], None).is_ok() {
            let _ = run_git(&repo, &["checkout", "HEAD", "--", &path], None);
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn revert_all(repo: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    if base_ref(&repo) != "HEAD" {
        return Err("仓库还没有任何提交，无法整体还原".to_string());
    }
    run_git(&repo, &["reset", "--hard", "HEAD"], None)?;
    // -fd without -x: untracked files/dirs go, .gitignore'd artifacts survive
    run_git(&repo, &["clean", "-fd"], None)?;
    Ok(())
}

/// Stage one path (or a rename's both sides) — `git add -A` so deletions and
/// untracked files stage correctly too, not just modifications.
#[tauri::command]
pub async fn stage_file(repo: String, path: String, old_path: Option<String>) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    if let Some(old) = &old_path {
        run_git(&repo, &["add", "-A", "--", old, &path], None)?;
    } else {
        run_git(&repo, &["add", "-A", "--", &path], None)?;
    }
    Ok(())
}

/// Index-only inverse of staging — mirrors `revert_file`'s per-kind dispatch
/// but never touches the worktree. `kind`/`old_path` matter because a staged
/// Added file has no HEAD version to restore from, and a staged Renamed file
/// needs its old path un-deleted from the index as well as its new path
/// un-added.
#[tauri::command]
pub async fn unstage_file(
    repo: String,
    path: String,
    old_path: Option<String>,
    kind: ChangeKind,
) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    match kind {
        ChangeKind::Added => {
            run_git(&repo, &["rm", "--cached", "-f", "--", &path], None)?;
        }
        ChangeKind::Renamed => {
            let old = old_path.ok_or("重命名缺少原路径")?;
            if base_ref(&repo) == "HEAD" {
                // old_path no longer has an index entry to infer source from —
                // must say --source=HEAD explicitly (see revert_file's rename arm)
                run_git(&repo, &["restore", "--source=HEAD", "--staged", "--", &old], None)?;
            }
            run_git(&repo, &["rm", "--cached", "-f", "--", &path], None)?;
        }
        _ => {
            if base_ref(&repo) == "HEAD" {
                run_git(&repo, &["restore", "--staged", "--", &path], None)?;
            } else {
                run_git(&repo, &["rm", "--cached", "-f", "--", &path], None)?;
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn stage_all(repo: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    run_git(&repo, &["add", "-A"], None)?;
    Ok(())
}

#[tauri::command]
pub async fn unstage_all(repo: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    if base_ref(&repo) == "HEAD" {
        run_git(&repo, &["reset"], None)?; // mixed reset to HEAD: HEAD itself doesn't move
    } else {
        run_git(&repo, &["read-tree", "--empty"], None)?;
    }
    Ok(())
}

/// Stage exactly one hunk from the *unstaged* (worktree-vs-index) diff —
/// index-only, the worktree file is untouched.
#[tauri::command]
pub async fn stage_hunk(repo: String, path: String, patch: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let mut patch = patch;
    if !patch.ends_with('\n') {
        patch.push('\n');
    }
    let first = run_git(
        &repo,
        &["apply", "--cached", "--whitespace=nowarn", "-"],
        Some(patch.as_bytes()),
    );
    if let Err(e1) = first {
        run_git(
            &repo,
            &["apply", "--cached", "--whitespace=nowarn", "--ignore-whitespace", "-"],
            Some(patch.as_bytes()),
        )
        .map_err(|e2| format!("暂存 {path} 失败: {e1}（宽松空白重试: {e2}）"))?;
    }
    Ok(())
}

/// Unstage exactly one hunk from the *staged* (index-vs-HEAD) diff —
/// index-only, the worktree file is untouched.
#[tauri::command]
pub async fn unstage_hunk(repo: String, path: String, patch: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let mut patch = patch;
    if !patch.ends_with('\n') {
        patch.push('\n');
    }
    let first = run_git(
        &repo,
        &["apply", "--cached", "-R", "--whitespace=nowarn", "-"],
        Some(patch.as_bytes()),
    );
    if let Err(e1) = first {
        run_git(
            &repo,
            &["apply", "--cached", "-R", "--whitespace=nowarn", "--ignore-whitespace", "-"],
            Some(patch.as_bytes()),
        )
        .map_err(|e2| format!("取消暂存 {path} 失败: {e1}（宽松空白重试: {e2}）"))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn create_commit(repo: String, message: String, amend: bool) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    if message.trim().is_empty() {
        return Err("提交信息不能为空".to_string());
    }
    let mut args = vec!["commit", "-F", "-"];
    if amend {
        args.push("--amend");
    }
    run_git(&repo, &args, Some(message.as_bytes()))?;
    Ok(())
}

/// IDEA-style commit: the UI's checkboxes only pick files, actual staging
/// happens here at commit time. `add -A -- <paths>` stages exactly the picked
/// paths (modifications, deletions and untracked files alike), then `--only`
/// commits just them, leaving any other staged content in the index untouched.
#[tauri::command]
pub async fn commit_paths(repo: String, message: String, paths: Vec<String>) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    if message.trim().is_empty() {
        return Err("提交信息不能为空".to_string());
    }
    if paths.is_empty() {
        return Err("没有勾选要提交的文件".to_string());
    }
    let mut add_args: Vec<&str> = vec!["add", "-A", "--"];
    add_args.extend(paths.iter().map(String::as_str));
    run_git(&repo, &add_args, None)?;
    // git forbids partial commits mid-merge/cherry-pick/revert — commit the
    // whole index there (that IS the resolution being concluded)
    if detect_operation(&repo) == RepoOperation::None {
        let mut args: Vec<&str> = vec!["commit", "-F", "-", "--only", "--"];
        args.extend(paths.iter().map(String::as_str));
        run_git(&repo, &args, Some(message.as_bytes()))?;
    } else {
        run_git(&repo, &["commit", "-F", "-"], Some(message.as_bytes()))?;
    }
    Ok(())
}

#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    pub is_remote: bool,
    pub upstream: Option<String>,
    /// upstream is configured but its remote-tracking ref no longer exists
    /// (the branch was deleted on the remote and pruned locally)
    pub upstream_gone: bool,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
}

/// "[ahead 2, behind 1]" | "[ahead 2]" | "[behind 1]" | "[gone]" | ""
/// -> (ahead, behind, gone)
fn parse_track(track: &str) -> (Option<u32>, Option<u32>, bool) {
    if track.contains("gone") {
        return (None, None, true);
    }
    if track.is_empty() {
        return (None, None, false);
    }
    let inner = track.trim_start_matches('[').trim_end_matches(']');
    let mut ahead = None;
    let mut behind = None;
    for part in inner.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().ok();
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().ok();
        }
    }
    (ahead, behind, false)
}

/// Local + remote-tracking branches, current branch marked. Uses `\x01` as a
/// field separator — refs cannot contain control characters, so this can
/// never collide with a real branch/upstream name.
#[tauri::command]
pub async fn list_branches(repo: String) -> Result<Vec<BranchInfo>, String> {
    let repo = PathBuf::from(repo);
    let out = run_git_text(
        &repo,
        &[
            "for-each-ref",
            "refs/heads",
            "refs/remotes",
            "--format=%(refname)%01%(HEAD)%01%(upstream:short)%01%(upstream:track)",
        ],
    )?;
    let mut branches = Vec::new();
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, '\u{1}');
        let (Some(refname), Some(head), Some(upstream), Some(track)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (name, is_remote) = if let Some(n) = refname.strip_prefix("refs/heads/") {
            (n.to_string(), false)
        } else if let Some(n) = refname.strip_prefix("refs/remotes/") {
            (n.to_string(), true)
        } else {
            continue;
        };
        if is_remote && name.ends_with("/HEAD") {
            continue; // symbolic ref to the remote's default branch, not a real branch
        }
        let (ahead, behind, upstream_gone) = parse_track(track);
        branches.push(BranchInfo {
            name,
            is_current: head == "*",
            is_remote,
            upstream: if upstream.is_empty() { None } else { Some(upstream.to_string()) },
            upstream_gone,
            ahead,
            behind,
        });
    }
    Ok(branches)
}

#[tauri::command]
pub async fn create_branch(
    repo: String,
    name: String,
    start_point: Option<String>,
    checkout: bool,
) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    if checkout {
        let mut args = vec!["switch", "-c", name.as_str()];
        if let Some(sp) = &start_point {
            args.push(sp);
        }
        run_git(&repo, &args, None)?;
    } else {
        let mut args = vec!["branch", name.as_str()];
        if let Some(sp) = &start_point {
            args.push(sp);
        }
        run_git(&repo, &args, None)?;
    }
    Ok(())
}

/// `is_remote` branches (e.g. "origin/feature-x") get a local tracking branch
/// created (named after the part past the first `/`) rather than landing in
/// detached HEAD, which is what a bare `git switch origin/feature-x` would do.
/// If a local branch of that name already exists, switch to it instead —
/// `switch -c` would fail with "branch already exists".
#[tauri::command]
pub async fn checkout_branch(repo: String, name: String, is_remote: bool) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    if is_remote {
        let local = name.splitn(2, '/').nth(1).unwrap_or(&name);
        let local_ref = format!("refs/heads/{local}");
        if run_git(&repo, &["rev-parse", "--verify", "--quiet", &local_ref], None).is_ok() {
            run_git(&repo, &["switch", local], None)?;
        } else {
            run_git(&repo, &["switch", "--track", "-c", local, &name], None)?;
        }
    } else {
        run_git(&repo, &["switch", &name], None)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn delete_branch(repo: String, name: String, force: bool) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let flag = if force { "-D" } else { "-d" };
    run_git(&repo, &["branch", flag, "--", &name], None)?;
    Ok(())
}

#[tauri::command]
pub async fn rename_branch(repo: String, old: String, new: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    run_git(&repo, &["branch", "-m", &old, &new], None)?;
    Ok(())
}

#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RemoteInfo {
    pub name: String,
    pub url: String,
}

#[tauri::command]
pub async fn list_remotes(repo: String) -> Result<Vec<RemoteInfo>, String> {
    let repo = PathBuf::from(repo);
    let out = run_git_text(&repo, &["remote", "-v"])?;
    let mut seen = std::collections::HashSet::new();
    let mut remotes = Vec::new();
    for line in out.lines() {
        // "origin\thttps://example.com/repo.git (fetch)"
        let mut parts = line.splitn(2, '\t');
        let (Some(name), Some(rest)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !seen.insert(name.to_string()) {
            continue; // fetch/push URLs usually match; list each remote once
        }
        let url = rest.rsplit_once(' ').map(|(u, _)| u).unwrap_or(rest);
        remotes.push(RemoteInfo { name: name.to_string(), url: url.to_string() });
    }
    Ok(remotes)
}

/// clones `url` into `dest` (a not-yet-existing or empty directory), optionally
/// scoped to a single branch and/or shallow. Runs with `dest`'s parent as the
/// working directory since `dest` itself doesn't exist as a repo yet.
#[tauri::command]
pub async fn clone_repo(url: String, dest: String, branch: Option<String>, depth: Option<u32>) -> Result<(), String> {
    let dest_path = PathBuf::from(&dest);
    let cwd = dest_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));

    let depth_arg = depth.filter(|d| *d > 0).map(|d| d.to_string());
    let mut args: Vec<&str> = vec!["clone"];
    if let Some(b) = &branch {
        args.push("--branch");
        args.push(b);
        args.push("--single-branch");
    }
    if let Some(d) = &depth_arg {
        args.push("--depth");
        args.push(d);
    }
    args.push(&url);
    args.push(&dest);
    run_git_timeout(&cwd, &args, NETWORK_TIMEOUT)?;
    Ok(())
}

#[tauri::command]
pub async fn fetch_remote(window: tauri::Window, repo: String, remote: Option<String>, prune: bool) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let mut args = vec!["fetch", "--progress"];
    if prune {
        args.push("--prune");
    }
    if let Some(r) = &remote {
        args.push(r);
    }
    let root = repo.to_string_lossy().into_owned();
    run_git_progress(&repo, &args, NETWORK_TIMEOUT, move |line| {
        let _ = window.emit("git-progress", (root.clone(), line));
    })?;
    Ok(())
}

#[tauri::command]
pub async fn pull_branch(window: tauri::Window, repo: String, remote: Option<String>, rebase: bool) -> Result<OpOutcome, String> {
    let repo = PathBuf::from(repo);
    // --prune drops remote-tracking refs for branches deleted on the remote,
    // so the 远程分支 list doesn't keep showing branches that no longer exist
    let mut args = vec!["pull", "--progress", "--prune", if rebase { "--rebase" } else { "--no-rebase" }];
    if let Some(r) = &remote {
        args.push(r);
    }
    let root = repo.to_string_lossy().into_owned();
    let result = run_git_progress(&repo, &args, NETWORK_TIMEOUT, move |line| {
        let _ = window.emit("git-progress", (root.clone(), line));
    })
    .map(|_| Vec::new());
    classify_op_result(&repo, result)
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum ForceMode {
    None,
    Lease,
    Force,
}

#[tauri::command]
pub async fn push_branch(
    repo: String,
    remote: String,
    branch: String,
    set_upstream: bool,
    force: ForceMode,
) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let mut args = vec!["push".to_string()];
    match force {
        ForceMode::Lease => args.push("--force-with-lease".to_string()),
        ForceMode::Force => args.push("--force".to_string()),
        ForceMode::None => {}
    }
    if set_upstream {
        args.push("-u".to_string());
    }
    args.push(remote);
    args.push(branch);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git_timeout(&repo, &arg_refs, NETWORK_TIMEOUT)?;
    Ok(())
}

/// Shared by cherry-pick / revert / merge (PR6): a non-zero exit either means
/// the operation genuinely failed (bad hash, dirty worktree, ...) or that it
/// left a conflict to resolve — only the presence of `*_HEAD` after the call
/// disambiguates which one happened.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum OpOutcome {
    Applied,
    Conflict,
}

fn classify_op_result(repo: &Path, result: Result<Vec<u8>, String>) -> Result<OpOutcome, String> {
    match result {
        Ok(_) => Ok(OpOutcome::Applied),
        Err(e) => {
            if detect_operation(repo) != RepoOperation::None {
                Ok(OpOutcome::Conflict)
            } else {
                Err(e)
            }
        }
    }
}

#[tauri::command]
pub async fn cherry_pick_commit(repo: String, hash: String) -> Result<OpOutcome, String> {
    let repo = PathBuf::from(repo);
    let result = run_git(&repo, &["cherry-pick", &hash], None);
    classify_op_result(&repo, result)
}

#[tauri::command]
pub async fn revert_commit(repo: String, hash: String) -> Result<OpOutcome, String> {
    let repo = PathBuf::from(repo);
    let result = run_git(&repo, &["revert", "--no-edit", &hash], None);
    classify_op_result(&repo, result)
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[tauri::command]
pub async fn reset_to(repo: String, hash: String, mode: ResetMode) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let flag = match mode {
        ResetMode::Soft => "--soft",
        ResetMode::Mixed => "--mixed",
        ResetMode::Hard => "--hard",
    };
    run_git(&repo, &["reset", flag, &hash], None)?;
    Ok(())
}

/// commit count strictly between `from` (exclusive) and `to` (inclusive) —
/// used to phrase the reset --hard confirm dialog ("discards N commits")
#[tauri::command]
pub async fn count_commits_between(repo: String, from: String, to: String) -> Result<u32, String> {
    let repo = PathBuf::from(repo);
    let spec = format!("{from}..{to}");
    let out = run_git_text(&repo, &["rev-list", "--count", &spec])?;
    out.trim().parse::<u32>().map_err(|_| "无法解析提交数量".to_string())
}

/// removes a single commit from history by replaying everything after it onto
/// its parent (`git rebase --onto <hash>^ <hash>`) — only well-defined for a
/// commit with exactly one parent (the UI disables this for merge commits,
/// same restriction as cherry-pick/revert) and only works if `hash` is
/// actually an ancestor of HEAD; git reports a conflict/error otherwise.
#[tauri::command]
pub async fn drop_commit(repo: String, hash: String) -> Result<OpOutcome, String> {
    let repo = PathBuf::from(repo);
    let parent = format!("{hash}^");
    let result = run_git(&repo, &["rebase", "--onto", &parent, &hash], None);
    classify_op_result(&repo, result)
}

#[tauri::command]
pub async fn merge_branch(repo: String, source: String, no_ff: bool) -> Result<OpOutcome, String> {
    let repo = PathBuf::from(repo);
    let mut args = vec!["merge", "--no-edit"];
    if no_ff {
        args.push("--no-ff");
    }
    args.push(&source);
    let result = run_git(&repo, &args, None);
    classify_op_result(&repo, result)
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ConflictSides {
    pub base: Option<String>,
    pub ours: Option<String>,
    pub theirs: Option<String>,
    pub is_binary: bool,
    pub too_large: bool,
}

enum StageContent {
    Missing,
    Binary,
    Text(String),
}

/// stage 1 = base (common ancestor), 2 = ours, 3 = theirs. A missing stage is
/// a legitimate outcome for add/add and delete/modify conflicts, not an error.
fn read_conflict_stage(repo: &Path, stage: u8, path: &str) -> StageContent {
    let spec = format!(":{stage}:{path}");
    match run_git(repo, &["show", &spec], None) {
        Err(_) => StageContent::Missing,
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => StageContent::Text(s),
            Err(_) => StageContent::Binary,
        },
    }
}

#[tauri::command]
pub async fn get_conflict_sides(repo: String, path: String) -> Result<ConflictSides, String> {
    let repo = PathBuf::from(repo);
    for stage in [1u8, 2, 3] {
        let spec = format!(":{stage}:{path}");
        if let Ok(sz) = run_git_text(&repo, &["cat-file", "-s", &spec]) {
            if sz.trim().parse::<u64>().unwrap_or(0) > MAX_FILE_SIZE {
                return Ok(ConflictSides {
                    base: None,
                    ours: None,
                    theirs: None,
                    is_binary: false,
                    too_large: true,
                });
            }
        }
    }
    let base = read_conflict_stage(&repo, 1, &path);
    let ours = read_conflict_stage(&repo, 2, &path);
    let theirs = read_conflict_stage(&repo, 3, &path);
    let is_binary = [&base, &ours, &theirs]
        .iter()
        .any(|s| matches!(s, StageContent::Binary));
    fn to_opt(s: StageContent) -> Option<String> {
        match s {
            StageContent::Text(t) => Some(t),
            _ => None,
        }
    }
    Ok(ConflictSides {
        base: to_opt(base),
        ours: to_opt(ours),
        theirs: to_opt(theirs),
        is_binary,
        too_large: false,
    })
}

/// Writes the resolved content to the worktree and stages it — the standard
/// "mark resolved" step for a text conflict the user edited in place.
#[tauri::command]
pub async fn resolve_conflict(repo: String, path: String, content: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    std::fs::write(repo.join(&path), content).map_err(|e| format!("写入 {path} 失败: {e}"))?;
    run_git(&repo, &["add", "--", &path], None)?;
    Ok(())
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum ConflictTake {
    Ours,
    Theirs,
}

/// For true binary conflicts, where both stage 2 and 3 exist as blobs (unlike
/// delete-conflicts, where one side is simply absent — see resolve_conflict_delete).
#[tauri::command]
pub async fn resolve_conflict_binary(repo: String, path: String, take: ConflictTake) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let flag = match take {
        ConflictTake::Ours => "--ours",
        ConflictTake::Theirs => "--theirs",
    };
    run_git(&repo, &["checkout", flag, "--", &path], None)?;
    run_git(&repo, &["add", "--", &path], None)?;
    Ok(())
}

/// For delete/modify conflicts where keeping "the deletion" is the resolution
/// — `checkout --ours/--theirs` has nothing to check out in that case.
#[tauri::command]
pub async fn resolve_conflict_delete(repo: String, path: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    run_git(&repo, &["rm", "-f", "--", &path], None)?;
    Ok(())
}

fn run_git_with_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    let mut cmd = git_command(repo);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let child = cmd.spawn().map_err(|e| format!("无法执行 git: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("git 执行失败: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Dispatches to the right `--continue` for whichever operation is in
/// progress. `GIT_EDITOR=true` keeps the default commit message instead of
/// blocking on an editor that has no terminal to attach to.
#[tauri::command]
pub async fn continue_operation(repo: String) -> Result<OpOutcome, String> {
    let repo = PathBuf::from(repo);
    let args: &[&str] = match detect_operation(&repo) {
        RepoOperation::Merge => &["merge", "--continue"],
        RepoOperation::CherryPick => &["cherry-pick", "--continue"],
        RepoOperation::Revert => &["revert", "--continue"],
        RepoOperation::Rebase => &["rebase", "--continue"],
        RepoOperation::None => return Err("当前没有进行中的合并 / cherry-pick / revert / rebase".to_string()),
    };
    let result = run_git_with_env(&repo, args, &[("GIT_EDITOR", "true")]);
    classify_op_result(&repo, result)
}

#[tauri::command]
pub async fn abort_operation(repo: String) -> Result<(), String> {
    let repo = PathBuf::from(repo);
    let args: &[&str] = match detect_operation(&repo) {
        RepoOperation::Merge => &["merge", "--abort"],
        RepoOperation::CherryPick => &["cherry-pick", "--abort"],
        RepoOperation::Revert => &["revert", "--abort"],
        RepoOperation::Rebase => &["rebase", "--abort"],
        RepoOperation::None => return Err("当前没有进行中的合并 / cherry-pick / revert / rebase".to_string()),
    };
    run_git(&repo, args, None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_basic() {
        let out = "M\0src/a.rs\0A\0new.txt\0D\0gone.txt\0";
        let files = parse_name_status(out);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].kind, ChangeKind::Modified);
        assert_eq!(files[1].path, "new.txt");
        assert_eq!(files[1].kind, ChangeKind::Added);
        assert_eq!(files[2].path, "gone.txt");
        assert_eq!(files[2].kind, ChangeKind::Deleted);
    }

    #[test]
    fn numstat_forms() {
        let out = "3\t1\tsrc/a.rs\0-\t-\tbin.dat\05\t0\t\0old.txt\0new.txt\0";
        let stats = parse_numstat(out);
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0], ("src/a.rs".into(), Some(3), Some(1)));
        assert_eq!(stats[1], ("bin.dat".into(), None, None)); // binary
        assert_eq!(stats[2], ("new.txt".into(), Some(5), Some(0))); // rename keyed by new path
    }

    #[test]
    fn decoration_parsing() {
        assert_eq!(parse_decorations(""), Vec::<String>::new());
        assert_eq!(parse_decorations("HEAD -> main"), vec!["main"]);
        assert_eq!(
            parse_decorations("HEAD -> main, origin/main"),
            vec!["main", "origin/main"]
        );
        assert_eq!(parse_decorations("tag: v1.0"), vec!["v1.0"]);
        assert_eq!(
            parse_decorations("HEAD -> main, tag: v1.0, origin/main"),
            vec!["main", "v1.0", "origin/main"]
        );
        assert_eq!(parse_decorations("HEAD"), Vec::<String>::new());
    }

    #[test]
    fn name_status_rename_and_copy() {
        let out = "R100\0old name.txt\0new name.txt\0C75\0src.txt\0copy.txt\0T\0link\0";
        let files = parse_name_status(out);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].kind, ChangeKind::Renamed);
        assert_eq!(files[0].path, "new name.txt");
        assert_eq!(files[0].old_path.as_deref(), Some("old name.txt"));
        assert_eq!(files[1].kind, ChangeKind::Added);
        assert_eq!(files[1].path, "copy.txt");
        assert_eq!(files[2].kind, ChangeKind::Modified); // T -> modified
    }

    #[test]
    fn hunk_header_forms() {
        assert_eq!(
            parse_hunk_header("@@ -1,5 +1,6 @@ fn main() {\n"),
            Some((1, 5, 1, 6))
        );
        assert_eq!(parse_hunk_header("@@ -1 +1,2 @@\n"), Some((1, 1, 1, 2)));
        assert_eq!(parse_hunk_header("@@ -0,0 +1 @@\n"), Some((0, 0, 1, 1)));
        assert_eq!(parse_hunk_header("not a hunk"), None);
    }

    #[test]
    fn diff_split_two_hunks() {
        let diff = "diff --git a/a.txt b/a.txt\n\
                    index 111..222 100644\n\
                    --- a/a.txt\n\
                    +++ b/a.txt\n\
                    @@ -1,3 +1,3 @@\n line1\n-line2\n+LINE2\n line3\n\
                    @@ -8,3 +8,3 @@\n line8\n-line9\n+LINE9\n line10\n\
                    \\ No newline at end of file\n";
        let (header, hunks, binary) = parse_diff(diff);
        assert!(!binary);
        assert!(header.starts_with("diff --git"));
        assert!(header.ends_with("+++ b/a.txt\n"));
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[1].new_start, 8);
        assert!(hunks[1].text.ends_with("\\ No newline at end of file\n"));
        // header + hunk reconstructs a valid standalone patch
        let patch = format!("{}{}", header, hunks[0].text);
        assert!(patch.contains("--- a/a.txt"));
        assert!(patch.contains("-line2"));
        assert!(!patch.contains("line8"));
    }

    #[test]
    fn diff_binary_detected() {
        let diff = "diff --git a/bin.dat b/bin.dat\n\
                    index 111..222 100644\n\
                    Binary files a/bin.dat and b/bin.dat differ\n";
        let (_, hunks, binary) = parse_diff(diff);
        assert!(binary);
        assert!(hunks.is_empty());
    }
}

/// Integration tests against real temporary git repos: exercise every status
/// kind and every revert path exactly the way the frontend drives them.
#[cfg(test)]
mod repo_tests {
    use super::*;

    // The commands are async (so Tauri runs them off the UI thread); these
    // same-named sync wrappers shadow the glob import above so the existing
    // synchronous tests keep calling them unchanged.
    fn bl<F: std::future::Future>(f: F) -> F::Output {
        tauri::async_runtime::block_on(f)
    }
    fn open_repo(path: String) -> Result<RepoInfo, String> {
        bl(super::open_repo(path))
    }
    fn get_status(repo: String) -> Result<Vec<FileStatus>, String> {
        bl(super::get_status(repo))
    }
    fn list_files(repo: String, show_ignored: bool) -> Result<Vec<String>, String> {
        list_files_full(repo, show_ignored).map(|l| l.files)
    }
    fn list_files_full(repo: String, show_ignored: bool) -> Result<FileListing, String> {
        let mode = if show_ignored { IgnoredMode::All } else { IgnoredMode::Files };
        bl(super::list_files(repo, mode))
    }
    fn list_files_mode(repo: String, mode: IgnoredMode) -> Result<FileListing, String> {
        bl(super::list_files(repo, mode))
    }
    fn read_file(repo: String, path: String) -> Result<FileContent, String> {
        bl(super::read_file(repo, path))
    }
    fn write_file(repo: String, path: String, content: String) -> Result<(), String> {
        bl(super::write_file(repo, path, content))
    }
    fn repo_stats(repo: String) -> Result<RepoStats, String> {
        bl(super::repo_stats(repo))
    }
    fn log_commits(repo: String, skip: u32, count: u32) -> Result<Vec<CommitInfo>, String> {
        bl(super::log_commits(repo, skip, count, None, None))
    }
    fn log_commits_on(repo: String, skip: u32, count: u32, branch: &str) -> Result<Vec<CommitInfo>, String> {
        bl(super::log_commits(repo, skip, count, Some(branch.to_string()), None))
    }
    fn commit_files(repo: String, hash: String) -> Result<Vec<FileStatus>, String> {
        bl(super::commit_files(repo, hash))
    }
    fn get_commit_message(repo: String, hash: String) -> Result<String, String> {
        bl(super::get_commit_message(repo, hash))
    }
    fn get_commit_file_diff(
        repo: String,
        hash: String,
        path: String,
        old_path: Option<String>,
        kind: ChangeKind,
    ) -> Result<FileDiff, String> {
        bl(super::get_commit_file_diff(repo, hash, path, old_path, kind))
    }
    fn get_file_diff(
        repo: String,
        path: String,
        old_path: Option<String>,
        kind: ChangeKind,
        staged: bool,
    ) -> Result<FileDiff, String> {
        bl(super::get_file_diff(repo, path, old_path, kind, staged))
    }
    fn stage_file(repo: String, path: String, old_path: Option<String>) -> Result<(), String> {
        bl(super::stage_file(repo, path, old_path))
    }
    fn unstage_file(
        repo: String,
        path: String,
        old_path: Option<String>,
        kind: ChangeKind,
    ) -> Result<(), String> {
        bl(super::unstage_file(repo, path, old_path, kind))
    }
    fn stage_all(repo: String) -> Result<(), String> {
        bl(super::stage_all(repo))
    }
    fn unstage_all(repo: String) -> Result<(), String> {
        bl(super::unstage_all(repo))
    }
    fn stage_hunk(repo: String, path: String, patch: String) -> Result<(), String> {
        bl(super::stage_hunk(repo, path, patch))
    }
    fn unstage_hunk(repo: String, path: String, patch: String) -> Result<(), String> {
        bl(super::unstage_hunk(repo, path, patch))
    }
    fn create_commit(repo: String, message: String, amend: bool) -> Result<(), String> {
        bl(super::create_commit(repo, message, amend))
    }
    fn commit_paths(repo: String, message: String, paths: Vec<String>) -> Result<(), String> {
        bl(super::commit_paths(repo, message, paths))
    }
    fn replace_in_files(
        repo: String,
        query: String,
        replacement: String,
        whole_word: bool,
        paths: Option<Vec<String>>,
    ) -> Result<ReplaceResult, String> {
        bl(super::replace_in_files(repo, query, replacement, whole_word, paths))
    }
    fn list_stashes(repo: String) -> Result<Vec<StashInfo>, String> {
        bl(super::list_stashes(repo))
    }
    fn stash_push(repo: String, message: String, paths: Vec<String>) -> Result<(), String> {
        bl(super::stash_push(repo, message, paths))
    }
    fn stash_pop(repo: String, index: u32) -> Result<(), String> {
        bl(super::stash_pop(repo, index))
    }
    fn stash_drop(repo: String, index: u32) -> Result<(), String> {
        bl(super::stash_drop(repo, index))
    }
    fn list_branches(repo: String) -> Result<Vec<BranchInfo>, String> {
        bl(super::list_branches(repo))
    }
    fn create_branch(
        repo: String,
        name: String,
        start_point: Option<String>,
        checkout: bool,
    ) -> Result<(), String> {
        bl(super::create_branch(repo, name, start_point, checkout))
    }
    fn checkout_branch(repo: String, name: String, is_remote: bool) -> Result<(), String> {
        bl(super::checkout_branch(repo, name, is_remote))
    }
    fn delete_branch(repo: String, name: String, force: bool) -> Result<(), String> {
        bl(super::delete_branch(repo, name, force))
    }
    fn rename_branch(repo: String, old: String, new: String) -> Result<(), String> {
        bl(super::rename_branch(repo, old, new))
    }
    fn list_remotes(repo: String) -> Result<Vec<RemoteInfo>, String> {
        bl(super::list_remotes(repo))
    }
    // fetch_remote/pull_branch take a `tauri::Window` (for progress events)
    // that can't be constructed outside a running app, so these test-only
    // wrappers call `run_git_progress` directly with a no-op progress
    // callback instead of going through the real `#[tauri::command]` fns —
    // same git args, just without the window plumbing.
    fn fetch_remote(repo: String, remote: Option<String>, prune: bool) -> Result<(), String> {
        let repo = PathBuf::from(repo);
        let mut args = vec!["fetch", "--progress"];
        if prune {
            args.push("--prune");
        }
        if let Some(r) = &remote {
            args.push(r);
        }
        run_git_progress(&repo, &args, NETWORK_TIMEOUT, |_| {})
    }
    fn clone_repo(url: String, dest: String, branch: Option<String>, depth: Option<u32>) -> Result<(), String> {
        bl(super::clone_repo(url, dest, branch, depth))
    }
    fn pull_branch(repo: String, remote: Option<String>, rebase: bool) -> Result<OpOutcome, String> {
        let repo = PathBuf::from(repo);
        let mut args = vec!["pull", "--progress", "--prune", if rebase { "--rebase" } else { "--no-rebase" }];
        if let Some(r) = &remote {
            args.push(r);
        }
        let result = run_git_progress(&repo, &args, NETWORK_TIMEOUT, |_| {}).map(|_| Vec::new());
        classify_op_result(&repo, result)
    }
    fn drop_commit(repo: String, hash: String) -> Result<OpOutcome, String> {
        bl(super::drop_commit(repo, hash))
    }
    fn search_commits(repo: String, branch: Option<String>, query: String, max_results: u32) -> Result<Vec<CommitInfo>, String> {
        bl(super::search_commits(repo, branch, query, max_results, None, None, None))
    }
    fn push_branch(
        repo: String,
        remote: String,
        branch: String,
        set_upstream: bool,
        force: ForceMode,
    ) -> Result<(), String> {
        bl(super::push_branch(repo, remote, branch, set_upstream, force))
    }
    fn cherry_pick_commit(repo: String, hash: String) -> Result<OpOutcome, String> {
        bl(super::cherry_pick_commit(repo, hash))
    }
    fn revert_commit(repo: String, hash: String) -> Result<OpOutcome, String> {
        bl(super::revert_commit(repo, hash))
    }
    fn reset_to(repo: String, hash: String, mode: ResetMode) -> Result<(), String> {
        bl(super::reset_to(repo, hash, mode))
    }
    fn count_commits_between(repo: String, from: String, to: String) -> Result<u32, String> {
        bl(super::count_commits_between(repo, from, to))
    }
    fn merge_branch(repo: String, source: String, no_ff: bool) -> Result<OpOutcome, String> {
        bl(super::merge_branch(repo, source, no_ff))
    }
    fn get_conflict_sides(repo: String, path: String) -> Result<ConflictSides, String> {
        bl(super::get_conflict_sides(repo, path))
    }
    fn resolve_conflict(repo: String, path: String, content: String) -> Result<(), String> {
        bl(super::resolve_conflict(repo, path, content))
    }
    fn resolve_conflict_binary(repo: String, path: String, take: ConflictTake) -> Result<(), String> {
        bl(super::resolve_conflict_binary(repo, path, take))
    }
    fn resolve_conflict_delete(repo: String, path: String) -> Result<(), String> {
        bl(super::resolve_conflict_delete(repo, path))
    }
    fn continue_operation(repo: String) -> Result<OpOutcome, String> {
        bl(super::continue_operation(repo))
    }
    fn abort_operation(repo: String) -> Result<(), String> {
        bl(super::abort_operation(repo))
    }
    fn revert_file(
        repo: String,
        path: String,
        old_path: Option<String>,
        kind: ChangeKind,
    ) -> Result<(), String> {
        bl(super::revert_file(repo, path, old_path, kind))
    }
    fn revert_hunk(repo: String, path: String, patch: String) -> Result<(), String> {
        bl(super::revert_hunk(repo, path, patch))
    }
    fn revert_all(repo: String) -> Result<(), String> {
        bl(super::revert_all(repo))
    }
    fn search_text(
        repo: String,
        query: String,
        whole_word: bool,
        max: u32,
    ) -> Result<Vec<SearchHit>, String> {
        bl(super::search_text(repo, query, whole_word, max))
    }
    fn create_file(repo: String, path: String) -> Result<(), String> {
        bl(super::create_file(repo, path))
    }
    fn create_dir(repo: String, path: String) -> Result<(), String> {
        bl(super::create_dir(repo, path))
    }
    fn get_console_log(repo: String) -> Result<Vec<ConsoleEntry>, String> {
        bl(super::get_console_log(repo))
    }

    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ai-diff-test-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            run_git(&dir, &["init", "-q"], None).unwrap();
            run_git(&dir, &["config", "user.email", "t@test"], None).unwrap();
            run_git(&dir, &["config", "user.name", "t"], None).unwrap();
            TempRepo(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn root(&self) -> String {
            self.0.to_string_lossy().to_string()
        }

        fn write(&self, rel: &str, content: &[u8]) {
            std::fs::write(self.0.join(rel), content).unwrap();
        }

        fn commit_all(&self) {
            run_git(&self.0, &["add", "-A"], None).unwrap();
            run_git(&self.0, &["commit", "-qm", "c"], None).unwrap();
        }

        /// Sets up a real both-modified merge conflict on `path`: a base
        /// commit, a diverging `theirs` branch, a diverging commit back on
        /// the starting branch, then `git merge theirs` — expected to fail
        /// and leave MERGE_HEAD + conflict markers in place, exactly like a
        /// real IDE merge flow.
        fn merge_conflict(&self, path: &str, base: &str, ours: &str, theirs: &str) {
            self.write(path, base.as_bytes());
            self.commit_all();
            let start_branch =
                String::from_utf8(run_git(&self.0, &["branch", "--show-current"], None).unwrap())
                    .unwrap()
                    .trim()
                    .to_string();
            run_git(&self.0, &["checkout", "-qb", "theirs"], None).unwrap();
            self.write(path, theirs.as_bytes());
            self.commit_all();
            run_git(&self.0, &["checkout", "-q", &start_branch], None).unwrap();
            self.write(path, ours.as_bytes());
            self.commit_all();
            let _ = run_git(&self.0, &["merge", "theirs"], None); // expected to fail (conflict)
        }

        /// `git status --porcelain` — the strictest cleanliness oracle: also
        /// catches index-vs-worktree drift that `git diff HEAD` cannot see
        /// (e.g. stale eol-convert state causing phantom modifications)
        fn porcelain(&self) -> String {
            String::from_utf8(run_git(&self.0, &["status", "--porcelain"], None).unwrap())
                .unwrap()
        }

        fn find<'a>(files: &'a [FileStatus], path: &str) -> &'a FileStatus {
            files
                .iter()
                .find(|f| f.path == path)
                .unwrap_or_else(|| panic!("{path} not in status"))
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn status_kinds_and_all_revert_paths() {
        let r = TempRepo::new("full");
        // 20 lines so that edits at line 2 and line 16 stay two separate hunks
        // (unified=3 context regions must not touch, or git merges them)
        let base_lines: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        r.write("a.txt", base_lines.as_bytes());
        r.write("b.txt", b"keep me\n");
        r.write("c.txt", b"rename me, keep this content identical\n");
        let bin: Vec<u8> = (0u16..64).map(|i| (i % 256) as u8).collect();
        r.write("bin.dat", &bin);
        r.commit_all();

        // mutate: 2 separate hunks in a.txt, delete, rename, untracked, binary edit, staged add
        let edited = base_lines
            .replace("line2\n", "LINE2-EDIT\n")
            .replace("line16\n", "LINE16-EDIT\n");
        r.write("a.txt", edited.as_bytes());
        std::fs::remove_file(r.path().join("b.txt")).unwrap();
        run_git(r.path(), &["mv", "c.txt", "c-renamed.txt"], None).unwrap();
        r.write("untracked.txt", b"brand new\n");
        let bin2: Vec<u8> = (0u16..64).rev().map(|i| (i % 256) as u8).collect();
        r.write("bin.dat", &bin2);
        r.write("staged.txt", b"staged change\n");
        run_git(r.path(), &["add", "staged.txt"], None).unwrap();

        let files = get_status(r.root()).unwrap();
        assert_eq!(files.len(), 6);
        assert_eq!(TempRepo::find(&files, "a.txt").kind, ChangeKind::Modified);
        assert_eq!(TempRepo::find(&files, "b.txt").kind, ChangeKind::Deleted);
        let renamed = TempRepo::find(&files, "c-renamed.txt");
        assert_eq!(renamed.kind, ChangeKind::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("c.txt"));
        assert_eq!(TempRepo::find(&files, "bin.dat").kind, ChangeKind::Modified);
        assert_eq!(TempRepo::find(&files, "staged.txt").kind, ChangeKind::Added);
        assert_eq!(
            TempRepo::find(&files, "untracked.txt").kind,
            ChangeKind::Untracked
        );

        // binary detection
        let bd = get_file_diff(r.root(), "bin.dat".into(), None, ChangeKind::Modified, false).unwrap();
        assert!(bd.is_binary);
        assert!(bd.hunks.is_empty());

        // hunk revert + offset drift: revert hunk 0, re-fetch, revert remaining hunk
        let d1 = get_file_diff(r.root(), "a.txt".into(), None, ChangeKind::Modified, false).unwrap();
        assert_eq!(d1.hunks.len(), 2);
        let patch1 = format!("{}{}", d1.file_header, d1.hunks[0].text);
        revert_hunk(r.root(), "a.txt".into(), patch1).unwrap();
        let d2 = get_file_diff(r.root(), "a.txt".into(), None, ChangeKind::Modified, false).unwrap();
        assert_eq!(d2.hunks.len(), 1, "one hunk must remain after first revert");
        let patch2 = format!("{}{}", d2.file_header, d2.hunks[0].text);
        revert_hunk(r.root(), "a.txt".into(), patch2).unwrap();
        let files = get_status(r.root()).unwrap();
        assert!(
            !files.iter().any(|f| f.path == "a.txt"),
            "a.txt must be clean after both hunks reverted"
        );

        // file-level reverts
        revert_file(r.root(), "b.txt".into(), None, ChangeKind::Deleted).unwrap();
        assert!(r.path().join("b.txt").is_file());

        revert_file(
            r.root(),
            "c-renamed.txt".into(),
            Some("c.txt".into()),
            ChangeKind::Renamed,
        )
        .unwrap();
        assert!(r.path().join("c.txt").is_file());
        assert!(!r.path().join("c-renamed.txt").exists());

        revert_file(r.root(), "staged.txt".into(), None, ChangeKind::Added).unwrap();
        assert!(!r.path().join("staged.txt").exists());

        revert_file(r.root(), "untracked.txt".into(), None, ChangeKind::Untracked).unwrap();
        assert!(!r.path().join("untracked.txt").exists());

        revert_file(r.root(), "bin.dat".into(), None, ChangeKind::Modified).unwrap();
        assert_eq!(std::fs::read(r.path().join("bin.dat")).unwrap(), bin);

        assert!(get_status(r.root()).unwrap().is_empty());
        assert_eq!(r.porcelain(), "", "git status must be fully clean");
    }

    /// Under autocrlf=false (CRLF stored raw) and autocrlf=true (LF stored,
    /// CRLF worktree) a hunk revert must leave the file clean AND keep CRLF.
    /// Under autocrlf=input LF is the canonical worktree form, so only
    /// cleanliness is asserted there.
    #[test]
    fn crlf_hunk_revert_keeps_line_endings() {
        for autocrlf in ["false", "true", "input"] {
            let r = TempRepo::new(&format!("crlf-{autocrlf}"));
            run_git(r.path(), &["config", "core.autocrlf", autocrlf], None).unwrap();
            r.write("crlf.txt", b"crlf1\r\ncrlf2\r\ncrlf3\r\n");
            r.commit_all();
            r.write("crlf.txt", b"crlf1\r\nCRLF2-EDIT\r\ncrlf3\r\n");

            let d =
                get_file_diff(r.root(), "crlf.txt".into(), None, ChangeKind::Modified, false).unwrap();
            assert_eq!(d.hunks.len(), 1, "autocrlf={autocrlf}");
            let patch = format!("{}{}", d.file_header, d.hunks[0].text);
            revert_hunk(r.root(), "crlf.txt".into(), patch).unwrap();

            assert!(
                get_status(r.root()).unwrap().is_empty(),
                "autocrlf={autocrlf}: git must see the file as clean after CRLF hunk revert"
            );
            assert_eq!(
                r.porcelain(),
                "",
                "autocrlf={autocrlf}: no phantom modification may remain in git status"
            );
            if autocrlf != "input" {
                let bytes = std::fs::read(r.path().join("crlf.txt")).unwrap();
                assert!(
                    bytes.windows(2).any(|w| w == b"\r\n"),
                    "autocrlf={autocrlf}: CRLF endings must survive the revert"
                );
            }
        }
    }

    #[test]
    fn revert_all_spares_ignored_files() {
        let r = TempRepo::new("revertall");
        r.write(".gitignore", b"ignored.txt\n");
        r.write("tracked.txt", b"v1\n");
        r.commit_all();

        r.write("tracked.txt", b"v2\n");
        r.write("untracked.txt", b"junk\n");
        r.write("ignored.txt", b"build artifact\n");
        r.write("staged-add.txt", b"staged\n");
        run_git(r.path(), &["add", "staged-add.txt"], None).unwrap();

        revert_all(r.root()).unwrap();

        assert!(get_status(r.root()).unwrap().is_empty());
        assert_eq!(std::fs::read(r.path().join("tracked.txt")).unwrap(), b"v1\n");
        assert!(!r.path().join("untracked.txt").exists());
        assert!(!r.path().join("staged-add.txt").exists());
        assert!(
            r.path().join("ignored.txt").is_file(),
            "clean -fd (no -x) must not delete .gitignore'd files"
        );
    }

    #[test]
    fn empty_repo_no_commits() {
        let r = TempRepo::new("empty");
        r.write("new.txt", b"hello\n");
        r.write("staged.txt", b"staged\n");
        run_git(r.path(), &["add", "staged.txt"], None).unwrap();

        let info = open_repo(r.root()).unwrap();
        assert!(!info.has_head);

        let files = get_status(r.root()).unwrap();
        assert_eq!(
            TempRepo::find(&files, "staged.txt").kind,
            ChangeKind::Added
        );
        assert_eq!(
            TempRepo::find(&files, "new.txt").kind,
            ChangeKind::Untracked
        );

        let d = get_file_diff(r.root(), "staged.txt".into(), None, ChangeKind::Added, true).unwrap();
        assert_eq!(d.modified.as_deref(), Some("staged\n"));
        assert!(d.original.is_none());

        assert!(revert_all(r.root()).is_err(), "revert_all must refuse without HEAD");

        revert_file(r.root(), "staged.txt".into(), None, ChangeKind::Added).unwrap();
        assert!(!r.path().join("staged.txt").exists());
    }

    #[test]
    fn history_log_files_and_diff() {
        let r = TempRepo::new("history");
        r.write("a.txt", b"one\ntwo\n");
        r.commit_all();
        r.write("a.txt", b"one\nTWO\nthree\n");
        r.write("b.txt", b"new\n");
        r.commit_all();

        let commits = log_commits(r.root(), 0, 10).unwrap();
        assert_eq!(commits.len(), 2);
        assert!(!commits[0].hash.is_empty());
        assert_eq!(commits[0].subject, "c");
        assert_eq!(commits[0].email, "t@test");
        // newest commit: a.txt +2 -1, b.txt +1 => totals +3 -1
        assert_eq!(commits[0].additions, 3);
        assert_eq!(commits[0].deletions, 1);
        assert_eq!(commits[1].additions, 2); // root commit adds a.txt (2 lines)
        assert_eq!(commits[1].deletions, 0);

        // paging
        let page2 = log_commits(r.root(), 1, 10).unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].hash, commits[1].hash);

        // newest commit: a.txt modified (+2 -1), b.txt added
        let files = commit_files(r.root(), commits[0].hash.clone()).unwrap();
        let a = TempRepo::find(&files, "a.txt");
        assert_eq!(a.kind, ChangeKind::Modified);
        assert_eq!(a.additions, Some(2));
        assert_eq!(a.deletions, Some(1));
        assert_eq!(TempRepo::find(&files, "b.txt").kind, ChangeKind::Added);

        let d = get_commit_file_diff(
            r.root(),
            commits[0].hash.clone(),
            "a.txt".into(),
            None,
            ChangeKind::Modified,
        )
        .unwrap();
        assert_eq!(d.original.as_deref(), Some("one\ntwo\n"));
        assert_eq!(d.modified.as_deref(), Some("one\nTWO\nthree\n"));
        assert!(d.hunks.is_empty());

        // root commit diffs against the empty tree
        let root_files = commit_files(r.root(), commits[1].hash.clone()).unwrap();
        assert_eq!(TempRepo::find(&root_files, "a.txt").kind, ChangeKind::Added);

        // working-tree status carries numstat too
        r.write("a.txt", b"one\nTWO\nthree\nfour\n");
        let st = get_status(r.root()).unwrap();
        let a = TempRepo::find(&st, "a.txt");
        assert_eq!(a.additions, Some(1));
        assert_eq!(a.deletions, Some(0));
    }

    #[test]
    fn list_files_and_read_file() {
        let r = TempRepo::new("listing");
        r.write(".gitignore", b"ignored.txt\n");
        r.write("tracked.txt", b"hello\n");
        r.commit_all();
        r.write("untracked.txt", b"new\n");
        r.write("ignored.txt", b"artifact\n");
        let bin: Vec<u8> = vec![0, 1, 2, 3];
        r.write("bin.dat", &bin);

        let listing = list_files_full(r.root(), false).unwrap();
        let files = &listing.files;
        assert!(files.contains(&"tracked.txt".to_string()));
        assert!(files.contains(&"untracked.txt".to_string()));
        assert!(files.contains(&"bin.dat".to_string()));
        assert!(
            files.contains(&"ignored.txt".to_string()),
            ".gitignore'd files in the tracked tree (local.properties style) must still be listed"
        );
        assert_eq!(listing.ignored, vec!["ignored.txt".to_string()], "…but flagged as ignored");
        assert!(files.is_sorted());

        // show_ignored=true keeps the file and still flags it
        let with_ignored = list_files_full(r.root(), true).unwrap();
        assert!(with_ignored.files.contains(&"tracked.txt".to_string()));
        assert!(with_ignored.files.contains(&"ignored.txt".to_string()));
        assert_eq!(with_ignored.ignored, vec!["ignored.txt".to_string()]);

        // Hide drops every ignored path, tracked/untracked untouched
        let hidden = list_files_mode(r.root(), IgnoredMode::Hide).unwrap();
        assert!(hidden.files.contains(&"tracked.txt".to_string()));
        assert!(hidden.files.contains(&"untracked.txt".to_string()));
        assert!(!hidden.files.contains(&"ignored.txt".to_string()), "Hide must drop ignored files");
        assert!(hidden.ignored.is_empty());

        let c = read_file(r.root(), "tracked.txt".into()).unwrap();
        assert_eq!(c.content.as_deref(), Some("hello\n"));
        assert!(!c.is_binary);

        let b = read_file(r.root(), "bin.dat".into()).unwrap();
        assert!(b.is_binary);
        assert!(b.content.is_none());

        assert!(read_file(r.root(), "missing.txt".into()).is_err());
    }

    #[test]
    fn search_text_basics() {
        let r = TempRepo::new("search");
        r.write("a.ts", b"function hello() {}\nconst helloWorld = 1;\n");
        r.commit_all();
        r.write("b.ts", b"hello();\n"); // untracked must be searched too

        let hits = search_text(r.root(), "hello".into(), false, 100).unwrap();
        assert_eq!(hits.len(), 3);

        let word_hits = search_text(r.root(), "hello".into(), true, 100).unwrap();
        assert_eq!(word_hits.len(), 2, "-w must exclude helloWorld");
        assert!(word_hits.iter().any(|h| h.path == "b.ts" && h.line == 1));

        assert!(search_text(r.root(), "nomatch_xyz".into(), false, 100).unwrap().is_empty());
        assert_eq!(search_text(r.root(), "hello".into(), false, 1).unwrap().len(), 1);
    }

    #[test]
    fn open_repo_rejects_non_repo() {
        let dir = std::env::temp_dir().join(format!("ai-diff-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(open_repo(dir.to_string_lossy().to_string()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_upstream_reports_none_not_error() {
        let r = TempRepo::new("no-upstream");
        r.write("a.txt", b"hi\n");
        r.commit_all();
        let info = open_repo(r.root()).unwrap();
        assert_eq!(info.upstream, None);
        assert_eq!(info.ahead, None);
        assert_eq!(info.behind, None);
        assert_eq!(info.operation, RepoOperation::None);
    }

    #[test]
    fn ahead_behind_against_upstream() {
        // bare repo as the "remote" — no working tree, only reachable via push/fetch
        let bare_dir = std::env::temp_dir().join(format!("ai-diff-test-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare_dir);
        std::fs::create_dir_all(&bare_dir).unwrap();
        run_git(&bare_dir, &["init", "-q", "--bare"], None).unwrap();
        let bare = bare_dir.to_string_lossy().to_string();

        let local = TempRepo::new("ahead-behind-local");
        local.write("a.txt", b"one\n");
        local.commit_all();
        let branch =
            String::from_utf8(run_git(local.path(), &["branch", "--show-current"], None).unwrap())
                .unwrap()
                .trim()
                .to_string();
        run_git(local.path(), &["remote", "add", "origin", &bare], None).unwrap();
        run_git(local.path(), &["push", "-q", "-u", "origin", &branch], None).unwrap();

        // a second clone pushes a commit `local` doesn't have yet (-> behind=1)
        let other_dir = std::env::temp_dir().join(format!("ai-diff-test-other-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other_dir);
        run_git(
            &std::env::temp_dir(),
            &["clone", "-q", &bare, other_dir.to_str().unwrap()],
            None,
        )
        .unwrap();
        run_git(&other_dir, &["config", "user.email", "t@test"], None).unwrap();
        run_git(&other_dir, &["config", "user.name", "t"], None).unwrap();
        std::fs::write(other_dir.join("b.txt"), b"other commit\n").unwrap();
        run_git(&other_dir, &["add", "-A"], None).unwrap();
        run_git(&other_dir, &["commit", "-qm", "other"], None).unwrap();
        run_git(&other_dir, &["push", "-q"], None).unwrap();
        let _ = std::fs::remove_dir_all(&other_dir);

        // local commits its own unpushed change (-> ahead=1)
        local.write("c.txt", b"local only\n");
        run_git(local.path(), &["add", "-A"], None).unwrap();
        run_git(local.path(), &["commit", "-qm", "local only"], None).unwrap();

        // fetch (not pull/merge) so the remote-tracking ref advances without
        // touching local HEAD — that's what makes ahead AND behind both nonzero
        run_git(local.path(), &["fetch", "-q", "origin"], None).unwrap();

        let info = open_repo(local.root()).unwrap();
        assert!(info.upstream.is_some());
        assert_eq!(info.ahead, Some(1));
        assert_eq!(info.behind, Some(1));

        let _ = std::fs::remove_dir_all(&bare_dir);
    }

    #[test]
    fn merge_conflict_detected_as_operation_and_conflicted_path() {
        let r = TempRepo::new("merge-conflict");
        r.merge_conflict("a.txt", "base\n", "ours\n", "theirs\n");

        let info = open_repo(r.root()).unwrap();
        assert_eq!(info.operation, RepoOperation::Merge);

        let files = get_status(r.root()).unwrap();
        let conflicted = TempRepo::find(&files, "a.txt");
        assert_eq!(conflicted.kind, ChangeKind::Conflicted);
        assert_eq!(conflicted.conflict, Some(ConflictSide::BothModified));

        // abort restores a clean, non-conflicted state
        run_git(r.path(), &["merge", "--abort"], None).unwrap();
        let info2 = open_repo(r.root()).unwrap();
        assert_eq!(info2.operation, RepoOperation::None);
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn stage_unstage_file_round_trip() {
        let r = TempRepo::new("stage-file");
        r.write("a.txt", b"base\n");
        r.commit_all();
        r.write("a.txt", b"changed\n");

        let before = get_status(r.root()).unwrap();
        assert!(!TempRepo::find(&before, "a.txt").staged);

        stage_file(r.root(), "a.txt".into(), None).unwrap();
        let staged = get_status(r.root()).unwrap();
        assert_eq!(staged.len(), 1, "fully staged, no unstaged remainder");
        assert!(TempRepo::find(&staged, "a.txt").staged);

        unstage_file(r.root(), "a.txt".into(), None, ChangeKind::Modified).unwrap();
        let after = get_status(r.root()).unwrap();
        assert_eq!(after.len(), 1);
        assert!(!TempRepo::find(&after, "a.txt").staged);
        assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"changed\n");
    }

    #[test]
    fn stage_and_unstage_rename() {
        let r = TempRepo::new("stage-rename");
        r.write("old.txt", b"content\n");
        r.commit_all();
        // a plain filesystem rename (NOT `git mv`, which stages immediately) so
        // the rename starts out unstaged, matching what `stage_file` needs to handle
        std::fs::rename(r.path().join("old.txt"), r.path().join("new.txt")).unwrap();

        stage_file(r.root(), "new.txt".into(), Some("old.txt".into())).unwrap();
        let files = get_status(r.root()).unwrap();
        let renamed = TempRepo::find(&files, "new.txt");
        assert_eq!(renamed.kind, ChangeKind::Renamed);
        assert!(renamed.staged);

        unstage_file(r.root(), "new.txt".into(), Some("old.txt".into()), ChangeKind::Renamed).unwrap();
        // index-only: worktree still only has new.txt (git mv already moved it on disk)
        assert!(r.path().join("new.txt").is_file());
        assert!(!r.path().join("old.txt").is_file());
        // index now matches HEAD again: old.txt reads as an unstaged deletion,
        // new.txt as untracked — the plain (non-rename) inverse of staging a move
        let after = get_status(r.root()).unwrap();
        assert_eq!(TempRepo::find(&after, "old.txt").kind, ChangeKind::Deleted);
        assert_eq!(TempRepo::find(&after, "new.txt").kind, ChangeKind::Untracked);
    }

    #[test]
    fn partial_stage_shows_twice_then_commit_only_staged_hunk() {
        let r = TempRepo::new("partial-stage");
        let base: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        r.write("a.txt", base.as_bytes());
        r.commit_all();

        // two separate hunks
        let edited = base.replace("line2\n", "LINE2\n").replace("line18\n", "LINE18\n");
        r.write("a.txt", edited.as_bytes());

        let unstaged_diff = get_file_diff(r.root(), "a.txt".into(), None, ChangeKind::Modified, false).unwrap();
        assert_eq!(unstaged_diff.hunks.len(), 2);
        let hunk0_patch = format!("{}{}", unstaged_diff.file_header, unstaged_diff.hunks[0].text);
        stage_hunk(r.root(), "a.txt".into(), hunk0_patch).unwrap();

        let files = get_status(r.root()).unwrap();
        let a_entries: Vec<_> = files.iter().filter(|f| f.path == "a.txt").collect();
        assert_eq!(a_entries.len(), 2, "partially-staged file appears once per side");
        assert!(a_entries.iter().any(|f| f.staged));
        assert!(a_entries.iter().any(|f| !f.staged));

        create_commit(r.root(), "stage hunk 0".into(), false).unwrap();
        // still one unstaged hunk left (line18) — repo isn't clean yet
        let after_commit = get_status(r.root()).unwrap();
        assert_eq!(after_commit.len(), 1);
        assert!(!after_commit[0].staged);

        // read the actual committed tree, not the worktree file — the worktree
        // still carries the unstaged line18 edit on top regardless of what got committed
        let committed = run_git_text(r.path(), &["show", "HEAD:a.txt"]).unwrap();
        assert!(committed.contains("LINE2\n"), "staged hunk must be committed");
        assert!(committed.contains("line18\n"), "unstaged hunk must NOT be committed yet");

        stage_all(r.root()).unwrap();
        create_commit(r.root(), "stage the rest".into(), false).unwrap();
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn unstage_hunk_round_trip() {
        let r = TempRepo::new("unstage-hunk");
        r.write("a.txt", b"one\ntwo\nthree\n");
        r.commit_all();
        r.write("a.txt", b"one\nTWO\nthree\n");
        stage_file(r.root(), "a.txt".into(), None).unwrap();

        let staged_diff = get_file_diff(r.root(), "a.txt".into(), None, ChangeKind::Modified, true).unwrap();
        assert_eq!(staged_diff.hunks.len(), 1);
        let patch = format!("{}{}", staged_diff.file_header, staged_diff.hunks[0].text);
        unstage_hunk(r.root(), "a.txt".into(), patch).unwrap();

        let files = get_status(r.root()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(!files[0].staged, "unstaged after unstage_hunk");
        // worktree untouched by an index-only operation
        assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"one\nTWO\nthree\n");
    }

    #[test]
    fn amend_changes_message_not_tree() {
        let r = TempRepo::new("amend");
        r.write("a.txt", b"content\n");
        r.commit_all();
        let before = log_commits(r.root(), 0, 1).unwrap();
        let tree_before =
            run_git_text(r.path(), &["rev-parse", "HEAD^{tree}"]).unwrap();

        create_commit(r.root(), "amended message".into(), true).unwrap();

        let after = log_commits(r.root(), 0, 1).unwrap();
        let tree_after = run_git_text(r.path(), &["rev-parse", "HEAD^{tree}"]).unwrap();
        assert_eq!(after[0].subject, "amended message");
        assert_ne!(after[0].subject, before[0].subject);
        assert_eq!(tree_before, tree_after, "amend must not change the tree");
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn stage_all_unstage_all_round_trip() {
        let r = TempRepo::new("stage-all");
        r.write("a.txt", b"base\n");
        r.commit_all();
        r.write("a.txt", b"changed\n");
        r.write("b.txt", b"new file\n");

        stage_all(r.root()).unwrap();
        let staged = get_status(r.root()).unwrap();
        assert_eq!(staged.len(), 2);
        assert!(staged.iter().all(|f| f.staged));

        unstage_all(r.root()).unwrap();
        let unstaged = get_status(r.root()).unwrap();
        assert_eq!(unstaged.len(), 2);
        assert!(unstaged.iter().all(|f| !f.staged));
    }

    #[test]
    fn commit_paths_commits_only_picked_files() {
        let r = TempRepo::new("commit-paths");
        r.write("a.txt", b"base\n");
        r.commit_all();
        r.write("a.txt", b"changed\n");
        r.write("b.txt", b"new file\n"); // untracked — must be added at commit time
        r.write("c.txt", b"left out\n");

        commit_paths(r.root(), "picked two".into(), vec!["a.txt".into(), "b.txt".into()]).unwrap();

        let head = log_commits(r.root(), 0, 1).unwrap();
        assert_eq!(head[0].subject, "picked two");
        let shown = run_git_text(r.path(), &["show", "--name-only", "--pretty=format:", "HEAD"]).unwrap();
        assert!(shown.contains("a.txt") && shown.contains("b.txt"));
        assert!(!shown.contains("c.txt"));
        // the unpicked file stays as an untracked working-tree change
        let files = get_status(r.root()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "c.txt");
        assert!(!files[0].staged);
    }

    #[test]
    fn commit_paths_ignores_other_staged_content() {
        let r = TempRepo::new("commit-paths-staged");
        r.write("a.txt", b"base\n");
        r.commit_all();
        r.write("a.txt", b"changed\n");
        r.write("d.txt", b"staged but unpicked\n");
        run_git(r.path(), &["add", "d.txt"], None).unwrap();

        commit_paths(r.root(), "only a".into(), vec!["a.txt".into()]).unwrap();

        let shown = run_git_text(r.path(), &["show", "--name-only", "--pretty=format:", "HEAD"]).unwrap();
        assert!(shown.contains("a.txt"));
        assert!(!shown.contains("d.txt"), "unpicked staged file must not be committed");
        // d.txt keeps its staged content for a later commit
        let files = get_status(r.root()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "d.txt");
        assert!(files[0].staged);
    }

    #[test]
    fn replace_in_files_plain_scoped_and_whole_word() {
        let r = TempRepo::new("replace-files");
        r.write("a.txt", b"foo bar foobar\nfoo\n");
        r.write("b.txt", b"foo here\n");
        r.write("c.txt", b"no match\n");
        r.commit_all();

        // scoped to a.txt only — b.txt must stay untouched
        let res = replace_in_files(r.root(), "foo".into(), "qux".into(), false, Some(vec!["a.txt".into()])).unwrap();
        assert_eq!((res.files, res.replacements), (1, 3), "foo, foobar's prefix and the lone foo");
        assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"qux bar quxbar\nqux\n");
        assert_eq!(std::fs::read(r.path().join("b.txt")).unwrap(), b"foo here\n");

        // repo-wide whole-word: quxbar must NOT match qux
        let res = replace_in_files(r.root(), "qux".into(), "foo".into(), true, None).unwrap();
        assert_eq!((res.files, res.replacements), (1, 2));
        assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"foo bar quxbar\nfoo\n");
    }

    #[test]
    fn replace_whole_word_boundaries() {
        let (out, n) = replace_whole_word("cat cats concat cat_x (cat)", "cat", "dog");
        assert_eq!(out, "dog cats concat cat_x (dog)");
        assert_eq!(n, 2);
    }

    #[test]
    fn shelve_unshelve_round_trip() {
        let r = TempRepo::new("shelve");
        r.write("a.txt", b"base\n");
        r.commit_all();
        r.write("a.txt", b"changed\n");
        r.write("b.txt", b"untracked\n");
        r.write("c.txt", b"left in worktree\n");

        stash_push(r.root(), "my shelf".into(), vec!["a.txt".into(), "b.txt".into()]).unwrap();

        // shelved files are gone from the worktree, the unpicked one stays
        let files = get_status(r.root()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "c.txt");
        let list = list_stashes(r.root()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].message.contains("my shelf"), "got: {}", list[0].message);

        stash_pop(r.root(), 0).unwrap();
        let files = get_status(r.root()).unwrap();
        assert_eq!(files.len(), 3, "both shelved files restored");
        assert!(list_stashes(r.root()).unwrap().is_empty(), "pop drops the entry");
    }

    #[test]
    fn stash_drop_discards_entry() {
        let r = TempRepo::new("shelve-drop");
        r.write("a.txt", b"base\n");
        r.commit_all();
        r.write("a.txt", b"changed\n");
        stash_push(r.root(), "to drop".into(), vec!["a.txt".into()]).unwrap();
        stash_drop(r.root(), 0).unwrap();
        assert!(list_stashes(r.root()).unwrap().is_empty());
        // dropped — the change is not restored
        assert_eq!(get_status(r.root()).unwrap().len(), 0);
    }

    #[test]
    fn commit_paths_rejects_empty_selection() {
        let r = TempRepo::new("commit-paths-empty");
        r.write("a.txt", b"content\n");
        assert!(commit_paths(r.root(), "msg".into(), vec![]).is_err());
    }

    #[test]
    fn create_commit_rejects_empty_message() {
        let r = TempRepo::new("empty-message");
        r.write("a.txt", b"content\n");
        stage_all(r.root()).unwrap();
        assert!(create_commit(r.root(), "   ".into(), false).is_err());
    }

    fn current_branch(r: &TempRepo) -> String {
        String::from_utf8(run_git(r.path(), &["branch", "--show-current"], None).unwrap())
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn create_checkout_delete_branch_round_trip() {
        let r = TempRepo::new("branch-lifecycle");
        r.write("a.txt", b"one\n");
        r.commit_all();
        let start = current_branch(&r);

        create_branch(r.root(), "feature".into(), None, true).unwrap();
        assert_eq!(current_branch(&r), "feature");

        let branches = list_branches(r.root()).unwrap();
        assert!(branches.iter().any(|b| b.name == "feature" && b.is_current && !b.is_remote));
        assert!(branches.iter().any(|b| b.name == start && !b.is_current));

        checkout_branch(r.root(), start.clone(), false).unwrap();
        assert_eq!(current_branch(&r), start);

        delete_branch(r.root(), "feature".into(), false).unwrap();
        let after = list_branches(r.root()).unwrap();
        assert!(!after.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn delete_unmerged_branch_requires_force() {
        let r = TempRepo::new("branch-unmerged");
        r.write("a.txt", b"one\n");
        r.commit_all();
        let start = current_branch(&r);

        create_branch(r.root(), "unmerged".into(), None, true).unwrap();
        r.write("a.txt", b"two\n");
        r.commit_all();
        checkout_branch(r.root(), start, false).unwrap();

        assert!(
            delete_branch(r.root(), "unmerged".into(), false).is_err(),
            "plain -d must refuse to drop unmerged commits"
        );
        delete_branch(r.root(), "unmerged".into(), true).unwrap();
        let after = list_branches(r.root()).unwrap();
        assert!(!after.iter().any(|b| b.name == "unmerged"));
    }

    #[test]
    fn checkout_blocked_by_dirty_worktree() {
        let r = TempRepo::new("branch-dirty-checkout");
        r.write("a.txt", b"one\n");
        r.commit_all();
        let start = current_branch(&r);

        // "other" must commit a DIFFERENT a.txt, or its content wouldn't
        // actually conflict with the dirty change below and switch would
        // succeed (git only blocks when checkout would clobber the diff)
        create_branch(r.root(), "other".into(), None, true).unwrap();
        r.write("a.txt", b"other-version\n");
        r.commit_all();
        checkout_branch(r.root(), start, false).unwrap();

        r.write("a.txt", b"dirty, uncommitted\n");
        assert!(
            checkout_branch(r.root(), "other".into(), false).is_err(),
            "git itself must refuse to switch over conflicting dirty changes"
        );
    }

    #[test]
    fn rename_branch_works() {
        let r = TempRepo::new("branch-rename");
        r.write("a.txt", b"one\n");
        r.commit_all();
        let start = current_branch(&r);
        rename_branch(r.root(), start.clone(), "renamed".into()).unwrap();
        assert_eq!(current_branch(&r), "renamed");
        let branches = list_branches(r.root()).unwrap();
        assert!(!branches.iter().any(|b| b.name == start));
        assert!(branches.iter().any(|b| b.name == "renamed" && b.is_current));
    }

    #[test]
    fn branch_names_with_slashes_parse_correctly() {
        let r = TempRepo::new("branch-slash");
        r.write("a.txt", b"one\n");
        r.commit_all();
        create_branch(r.root(), "feature/foo/bar".into(), None, false).unwrap();
        let branches = list_branches(r.root()).unwrap();
        assert!(branches.iter().any(|b| b.name == "feature/foo/bar" && !b.is_remote));
    }

    #[test]
    fn checkout_remote_branch_creates_local_tracking_branch() {
        let bare_dir = std::env::temp_dir().join(format!("ai-diff-test-bare-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare_dir);
        std::fs::create_dir_all(&bare_dir).unwrap();
        run_git(&bare_dir, &["init", "-q", "--bare"], None).unwrap();
        let bare = bare_dir.to_string_lossy().to_string();

        let seed = TempRepo::new("branch-remote-seed");
        seed.write("a.txt", b"one\n");
        seed.commit_all();
        let default_branch = current_branch(&seed);
        run_git(seed.path(), &["remote", "add", "origin", &bare], None).unwrap();
        run_git(seed.path(), &["push", "-q", "-u", "origin", &default_branch], None).unwrap();
        // a second branch the clone does NOT auto-checkout (unlike the default
        // branch, which `git clone` already creates a local tracking branch
        // for) — this is the one we actually test checkout_branch against
        run_git(seed.path(), &["switch", "-c", "feature"], None).unwrap();
        seed.write("b.txt", b"feature work\n");
        seed.commit_all();
        run_git(seed.path(), &["push", "-q", "-u", "origin", "feature"], None).unwrap();

        let local_dir = std::env::temp_dir().join(format!("ai-diff-test-clone-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&local_dir);
        run_git(&std::env::temp_dir(), &["clone", "-q", &bare, local_dir.to_str().unwrap()], None).unwrap();
        run_git(&local_dir, &["config", "user.email", "t@test"], None).unwrap();
        run_git(&local_dir, &["config", "user.name", "t"], None).unwrap();

        let remote_name = "origin/feature".to_string();
        let branches = list_branches(local_dir.to_string_lossy().to_string()).unwrap();
        assert!(branches.iter().any(|b| b.name == remote_name && b.is_remote));
        assert!(!branches.iter().any(|b| b.name == "feature" && !b.is_remote));

        checkout_branch(local_dir.to_string_lossy().to_string(), remote_name, true).unwrap();
        let local_branch = String::from_utf8(
            run_git(&local_dir, &["branch", "--show-current"], None).unwrap(),
        )
        .unwrap()
        .trim()
        .to_string();
        assert_eq!(local_branch, "feature");

        let _ = std::fs::remove_dir_all(&bare_dir);
        let _ = std::fs::remove_dir_all(&local_dir);
    }

    /// Bare repo as "remote" + a local clone with a commit already pushed and
    /// upstream tracking configured. Returns (bare_dir, local TempRepo, branch).
    fn bare_remote_with_clone(tag: &str) -> (PathBuf, TempRepo, String) {
        let bare_dir = std::env::temp_dir().join(format!("ai-diff-test-bare-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare_dir);
        std::fs::create_dir_all(&bare_dir).unwrap();
        run_git(&bare_dir, &["init", "-q", "--bare"], None).unwrap();
        let bare = bare_dir.to_string_lossy().to_string();

        let seed = TempRepo::new(&format!("{tag}-seed"));
        seed.write("a.txt", b"one\n");
        seed.commit_all();
        let branch = current_branch(&seed);
        run_git(seed.path(), &["push", "-q", &bare, &format!("HEAD:{branch}")], None).unwrap();

        let local = TempRepo::new(&format!("{tag}-local"));
        run_git(local.path(), &["remote", "add", "origin", &bare], None).unwrap();
        run_git(local.path(), &["fetch", "-q", "origin"], None).unwrap();
        run_git(local.path(), &["checkout", "-q", "-b", &branch, "--track", &format!("origin/{branch}")], None)
            .unwrap();
        (bare_dir, local, branch)
    }

    #[test]
    fn fetch_updates_remote_tracking_ref_without_touching_head() {
        let (bare_dir, local, branch) = bare_remote_with_clone("fetch");

        // a second clone pushes a commit the first clone doesn't have yet
        let other_dir = std::env::temp_dir().join(format!("ai-diff-test-other-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other_dir);
        run_git(&std::env::temp_dir(), &["clone", "-q", bare_dir.to_str().unwrap(), other_dir.to_str().unwrap()], None).unwrap();
        run_git(&other_dir, &["config", "user.email", "t@test"], None).unwrap();
        run_git(&other_dir, &["config", "user.name", "t"], None).unwrap();
        std::fs::write(other_dir.join("b.txt"), b"other\n").unwrap();
        run_git(&other_dir, &["add", "-A"], None).unwrap();
        run_git(&other_dir, &["commit", "-qm", "other"], None).unwrap();
        run_git(&other_dir, &["push", "-q"], None).unwrap();

        let head_before = run_git_text(local.path(), &["rev-parse", "HEAD"]).unwrap();
        fetch_remote(local.root(), None, false).unwrap();
        let head_after = run_git_text(local.path(), &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(head_before, head_after, "fetch must never move local HEAD");

        let remote_ref = run_git_text(local.path(), &["rev-parse", &format!("origin/{branch}")]).unwrap();
        let other_head = run_git_text(&other_dir, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(remote_ref, other_head, "remote-tracking ref must reflect the fetched commit");

        let _ = std::fs::remove_dir_all(&bare_dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    #[test]
    fn pull_fast_forwards_local_branch() {
        let (bare_dir, local, branch) = bare_remote_with_clone("pull");

        let other_dir = std::env::temp_dir().join(format!("ai-diff-test-other-pull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other_dir);
        run_git(&std::env::temp_dir(), &["clone", "-q", bare_dir.to_str().unwrap(), other_dir.to_str().unwrap()], None).unwrap();
        run_git(&other_dir, &["config", "user.email", "t@test"], None).unwrap();
        run_git(&other_dir, &["config", "user.name", "t"], None).unwrap();
        std::fs::write(other_dir.join("b.txt"), b"other\n").unwrap();
        run_git(&other_dir, &["add", "-A"], None).unwrap();
        run_git(&other_dir, &["commit", "-qm", "other"], None).unwrap();
        run_git(&other_dir, &["push", "-q"], None).unwrap();

        pull_branch(local.root(), None, false).unwrap();
        assert!(local.path().join("b.txt").is_file(), "pull must merge the remote commit into the worktree");
        assert_eq!(local.porcelain(), "");
        let _ = branch;

        let _ = std::fs::remove_dir_all(&bare_dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    /// Diagnostic: exercises the Windows suspend/assign/resume + Job Object
    /// mechanism directly against a plain native process tree (cmd.exe spawns
    /// ping.exe), with no git/MSYS involved at all — isolates whether the
    /// mechanism itself is race-free from whatever MSYS bash's own fork
    /// emulation does when git invokes a shell-script hook.
    #[cfg(windows)]
    #[test]
    fn windows_job_object_kills_native_process_tree() {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "ping -n 30 127.0.0.1 >NUL"]);
        cmd.creation_flags(CREATE_NO_WINDOW | proc_tree::CREATE_SUSPENDED);
        let child = cmd.spawn().unwrap();
        let pid = child.id();
        let job = proc_tree::JobGuard::new(&child);
        proc_tree::resume_main_thread(pid);
        // give cmd.exe time to actually spawn ping.exe as its child
        std::thread::sleep(Duration::from_millis(800));
        job.expect("job assignment must succeed on a freshly suspended process").kill_tree();
        std::thread::sleep(Duration::from_millis(500));

        let out = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq ping.exe"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains("ping.exe"), "ping.exe child must not survive TerminateJobObject: {text}");
    }

    /// Proves `run_git_timeout` actually kills the whole process tree instead
    /// of just the top-level git.exe. Uses git's `ext::` transport (a remote
    /// helper git spawns as a plain native child process — the exact same
    /// mechanism it uses for `ssh`) pointed at `ping`, redirected to NUL so it
    /// never writes the pkt-line data git expects: git blocks reading from
    /// the pipe, `ping`/its `cmd.exe` parent run for ~30s, a real stand-in for
    /// a stalled fetch/pull/push spawning an ssh/askpass child that never
    /// returns. (An earlier version of this test used a pre-commit hook
    /// spawning `sh`/`sleep` instead; that hit an MSYS-fork-emulation quirk
    /// unrelated to this fix — see `windows_job_object_kills_native_process_tree`
    /// for proof the actual kill mechanism is race-free against real native
    /// process trees, which is what git spawning ssh.exe actually is.)
    #[cfg(windows)]
    #[test]
    fn run_git_timeout_kills_whole_process_tree_on_hang() {
        let repo = TempRepo::new("timeout-kill");

        let start = std::time::Instant::now();
        let result = run_git_timeout(
            repo.path(),
            &[
                "-c",
                "protocol.ext.allow=always",
                "fetch",
                "ext::cmd /c ping -n 30 127.0.0.1 >NUL",
            ],
            Duration::from_secs(2),
        );
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a remote helper blocked for 30s past a 2s timeout must be killed, not complete");
        assert!(
            result.unwrap_err().contains("超时"),
            "the error should say this was a timeout, not some other git failure"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "must return shortly after the 2s timeout ({elapsed:?}), not wait out the full 30s hang"
        );

        std::thread::sleep(Duration::from_millis(300));
        let out = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq ping.exe"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        assert!(!text.contains("ping.exe"), "ping.exe must not survive as an orphan: {text}");
    }

    #[test]
    fn push_uploads_commit_to_remote() {
        let (bare_dir, local, branch) = bare_remote_with_clone("push");

        local.write("c.txt", b"local work\n");
        run_git(local.path(), &["add", "-A"], None).unwrap();
        run_git(local.path(), &["commit", "-qm", "local work"], None).unwrap();
        push_branch(local.root(), "origin".into(), branch.clone(), false, ForceMode::None).unwrap();

        let bare_head = run_git_text(&bare_dir, &["rev-parse", &branch]).unwrap();
        let local_head = run_git_text(local.path(), &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(bare_head, local_head, "pushed commit must land on the bare remote");

        let _ = std::fs::remove_dir_all(&bare_dir);
    }

    #[test]
    fn push_rejected_on_diverge_then_force_with_lease_succeeds() {
        let (bare_dir, local, branch) = bare_remote_with_clone("force-push");

        // someone else pushes to the remote first, diverging history
        let other_dir = std::env::temp_dir().join(format!("ai-diff-test-other-force-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other_dir);
        run_git(&std::env::temp_dir(), &["clone", "-q", bare_dir.to_str().unwrap(), other_dir.to_str().unwrap()], None).unwrap();
        run_git(&other_dir, &["config", "user.email", "t@test"], None).unwrap();
        run_git(&other_dir, &["config", "user.name", "t"], None).unwrap();
        std::fs::write(other_dir.join("b.txt"), b"other\n").unwrap();
        run_git(&other_dir, &["add", "-A"], None).unwrap();
        run_git(&other_dir, &["commit", "-qm", "other"], None).unwrap();
        run_git(&other_dir, &["push", "-q"], None).unwrap();

        // local also commits, unaware of the remote's new commit
        local.write("c.txt", b"local, conflicting history\n");
        run_git(local.path(), &["add", "-A"], None).unwrap();
        run_git(local.path(), &["commit", "-qm", "local diverged"], None).unwrap();

        assert!(
            push_branch(local.root(), "origin".into(), branch.clone(), false, ForceMode::None).is_err(),
            "a plain push must be rejected on non-fast-forward"
        );
        // --force-with-lease refuses on stale remote-tracking knowledge (by
        // design — that's the whole safety property) so a real fetch has to
        // happen first, exactly like a real user would after a rejected push
        fetch_remote(local.root(), None, false).unwrap();
        push_branch(local.root(), "origin".into(), branch, false, ForceMode::Lease).unwrap();

        let bare_head = run_git_text(&bare_dir, &["rev-parse", "HEAD"]).unwrap();
        let local_head = run_git_text(local.path(), &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(bare_head, local_head, "force-with-lease push must win and update the remote");

        let _ = std::fs::remove_dir_all(&bare_dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    #[test]
    fn list_files_hides_ignored_directories_unless_asked() {
        let r = TempRepo::new("listing-ignored-dirs");
        r.write(".gitignore", b"build/\nlocal.properties\n*.log\n");
        r.write("src.txt", b"src\n");
        r.commit_all();
        std::fs::create_dir_all(r.path().join("build/out")).unwrap();
        r.write("build/out/a.o", b"obj\n");
        r.write("local.properties", b"sdk.dir=/x\n");
        std::fs::create_dir_all(r.path().join("logs")).unwrap();
        r.write("logs/run.log", b"log\n");

        let l = list_files_full(r.root(), false).unwrap();
        assert!(l.files.contains(&"local.properties".to_string()), "ignored file at top level shows");
        assert!(l.files.contains(&"logs/run.log".to_string()), "ignored file in a non-ignored dir shows");
        assert!(
            !l.files.iter().any(|f| f.starts_with("build/")),
            "contents of an ignored directory stay hidden: {:?}",
            l.files
        );
        assert!(!l.files.iter().any(|f| f.ends_with('/')), "no bare directory entries leak through");
        let mut ig = l.ignored.clone();
        ig.sort();
        assert_eq!(ig, vec!["local.properties".to_string(), "logs/run.log".to_string()]);

        let all = list_files_full(r.root(), true).unwrap();
        assert!(all.files.contains(&"build/out/a.o".to_string()), "show_ignored reveals ignored dirs' contents");
        assert!(all.ignored.contains(&"build/out/a.o".to_string()));
    }

    #[test]
    fn checkout_remote_branch_reuses_existing_local_branch() {
        let (bare_dir, local, branch) = bare_remote_with_clone("checkout-remote-existing");
        // a second local branch that also exists on the remote
        run_git(local.path(), &["switch", "-c", "feature"], None).unwrap();
        local.write("f.txt", b"f\n");
        local.commit_all();
        run_git(local.path(), &["push", "-q", "-u", "origin", "feature"], None).unwrap();
        run_git(local.path(), &["switch", &branch], None).unwrap();

        // picking "origin/feature" while local "feature" exists must not fail
        // with "a branch named 'feature' already exists"
        checkout_branch(local.root(), "origin/feature".to_string(), true).unwrap();
        assert_eq!(current_branch(&local), "feature");
        let _ = std::fs::remove_dir_all(&bare_dir);
    }

    #[test]
    fn list_branches_flags_gone_upstream() {
        let (bare_dir, local, _branch) = bare_remote_with_clone("gone-upstream");
        run_git(local.path(), &["switch", "-c", "feature"], None).unwrap();
        local.write("f.txt", b"f\n");
        local.commit_all();
        run_git(local.path(), &["push", "-q", "-u", "origin", "feature"], None).unwrap();
        // delete on the remote, then prune locally — the local branch's
        // upstream now points at nothing
        run_git(local.path(), &["push", "-q", "origin", "--delete", "feature"], None).unwrap();
        run_git(local.path(), &["fetch", "-q", "--prune"], None).unwrap();

        let branches = list_branches(local.root()).unwrap();
        let feature = branches.iter().find(|b| b.name == "feature" && !b.is_remote).unwrap();
        assert!(feature.upstream_gone, "upstream deleted+pruned must be flagged: {feature:?}");
        assert_eq!(feature.upstream.as_deref(), Some("origin/feature"));
        assert!(
            !branches.iter().any(|b| b.is_remote && b.name == "origin/feature"),
            "pruned remote-tracking branch must not be listed"
        );
        let _ = std::fs::remove_dir_all(&bare_dir);
    }

    #[test]
    fn parse_track_variants() {
        assert_eq!(parse_track(""), (None, None, false));
        assert_eq!(parse_track("[ahead 2, behind 1]"), (Some(2), Some(1), false));
        assert_eq!(parse_track("[behind 3]"), (None, Some(3), false));
        assert_eq!(parse_track("[gone]"), (None, None, true));
    }

    #[test]
    fn list_remotes_reports_name_and_url() {
        let (bare_dir, local, _branch) = bare_remote_with_clone("list-remotes");
        let remotes = list_remotes(local.root()).unwrap();
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(remotes[0].url, bare_dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&bare_dir);
    }

    #[test]
    fn cherry_pick_applies_cleanly() {
        let r = TempRepo::new("cherry-clean");
        r.write("a.txt", b"base\n");
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("b.txt", b"feature file\n");
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "add b"], None).unwrap();
        let feature_commit = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        checkout_branch(r.root(), start, false).unwrap();

        let outcome = cherry_pick_commit(r.root(), feature_commit).unwrap();
        assert_eq!(outcome, OpOutcome::Applied);
        assert!(r.path().join("b.txt").is_file());
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn cherry_pick_conflict_then_abort() {
        let r = TempRepo::new("cherry-conflict");
        r.write("a.txt", b"base\n");
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("a.txt", b"feature-version\n");
        r.commit_all();
        let feature_commit = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        checkout_branch(r.root(), start, false).unwrap();
        r.write("a.txt", b"main-version\n");
        r.commit_all();

        let outcome = cherry_pick_commit(r.root(), feature_commit).unwrap();
        assert_eq!(outcome, OpOutcome::Conflict);
        let info = open_repo(r.root()).unwrap();
        assert_eq!(info.operation, RepoOperation::CherryPick);

        run_git(r.path(), &["cherry-pick", "--abort"], None).unwrap();
        let info2 = open_repo(r.root()).unwrap();
        assert_eq!(info2.operation, RepoOperation::None);
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn revert_commit_applies_cleanly() {
        let r = TempRepo::new("revert-clean");
        r.write("a.txt", b"base\n");
        r.commit_all();
        r.write("a.txt", b"changed\n");
        r.commit_all();
        let commit_to_revert = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        let outcome = revert_commit(r.root(), commit_to_revert).unwrap();
        assert_eq!(outcome, OpOutcome::Applied);
        assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"base\n");
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn revert_commit_conflict_then_abort() {
        let r = TempRepo::new("revert-conflict");
        r.write("a.txt", b"orig\n");
        r.commit_all();
        r.write("a.txt", b"v1\n");
        r.commit_all();
        let first_edit = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        r.write("a.txt", b"v2\n");
        r.commit_all();

        let outcome = revert_commit(r.root(), first_edit).unwrap();
        assert_eq!(outcome, OpOutcome::Conflict);
        let info = open_repo(r.root()).unwrap();
        assert_eq!(info.operation, RepoOperation::Revert);

        run_git(r.path(), &["revert", "--abort"], None).unwrap();
        let info2 = open_repo(r.root()).unwrap();
        assert_eq!(info2.operation, RepoOperation::None);
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn reset_soft_mixed_hard_semantics() {
        // soft: HEAD moves, but index AND worktree keep the newer content staged
        {
            let r = TempRepo::new("reset-soft");
            r.write("a.txt", b"v1\n");
            r.commit_all();
            let first = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
            r.write("a.txt", b"v2\n");
            r.commit_all();
            reset_to(r.root(), first.clone(), ResetMode::Soft).unwrap();
            assert_eq!(run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim(), first);
            assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"v2\n", "worktree keeps the newer content");
            let status = r.porcelain();
            assert!(status.starts_with("M "), "soft reset must leave the diff staged: {status:?}");
        }
        // mixed: HEAD moves, worktree keeps content, index un-stages it
        {
            let r = TempRepo::new("reset-mixed");
            r.write("a.txt", b"v1\n");
            r.commit_all();
            let first = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
            r.write("a.txt", b"v2\n");
            r.commit_all();
            reset_to(r.root(), first.clone(), ResetMode::Mixed).unwrap();
            assert_eq!(run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim(), first);
            assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"v2\n");
            let status = r.porcelain();
            assert!(status.starts_with(" M"), "mixed reset must leave the diff unstaged: {status:?}");
        }
        // hard: HEAD moves, index AND worktree revert to match it exactly
        {
            let r = TempRepo::new("reset-hard");
            r.write("a.txt", b"v1\n");
            r.commit_all();
            let first = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
            r.write("a.txt", b"v2\n");
            r.commit_all();
            reset_to(r.root(), first.clone(), ResetMode::Hard).unwrap();
            assert_eq!(run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim(), first);
            assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"v1\n");
            assert_eq!(r.porcelain(), "");
        }
    }

    #[test]
    fn count_commits_between_works() {
        let r = TempRepo::new("count-commits");
        r.write("a.txt", b"1\n");
        r.commit_all();
        let first = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        r.write("a.txt", b"2\n");
        r.commit_all();
        r.write("a.txt", b"3\n");
        r.commit_all();
        let head = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let n = count_commits_between(r.root(), first, head).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn log_commits_reports_parents() {
        let r = TempRepo::new("parents");
        r.write("a.txt", b"base\n");
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("b.txt", b"feature\n");
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "feature commit"], None).unwrap();
        checkout_branch(r.root(), start.clone(), false).unwrap();
        run_git(r.path(), &["merge", "--no-ff", "-m", "merge feature", "feature"], None).unwrap();

        let commits = log_commits(r.root(), 0, 10).unwrap();
        assert_eq!(commits[0].parents.len(), 2, "merge commit must report 2 parents");
        assert_eq!(commits.last().unwrap().parents.len(), 0, "root commit has no parents");

        assert!(
            commits[0].refs.iter().any(|name| name == &start),
            "the merge commit (HEAD) should be decorated with the current branch name: {:?}",
            commits[0].refs
        );
        let feature_commit = commits.iter().find(|c| c.subject == "feature commit").unwrap();
        assert!(
            feature_commit.refs.iter().any(|name| name == "feature"),
            "the feature branch's tip commit should be decorated with its branch name: {:?}",
            feature_commit.refs
        );
        assert!(
            commits.last().unwrap().refs.is_empty(),
            "the root commit has no ref pointing at it directly"
        );
    }

    #[test]
    fn log_commits_scoped_to_branch() {
        let r = TempRepo::new("branch-scope");
        r.write("a.txt", b"base\n");
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("b.txt", b"feature\n");
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "feature commit"], None).unwrap();
        checkout_branch(r.root(), start.clone(), false).unwrap();
        r.write("c.txt", b"main only\n");
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "main-only commit"], None).unwrap();

        let feature_log = log_commits_on(r.root(), 0, 10, "feature").unwrap();
        assert!(feature_log.iter().any(|c| c.subject == "feature commit"));
        assert!(!feature_log.iter().any(|c| c.subject == "main-only commit"));

        let main_log = log_commits_on(r.root(), 0, 10, &start).unwrap();
        assert!(main_log.iter().any(|c| c.subject == "main-only commit"));
        assert!(!main_log.iter().any(|c| c.subject == "feature commit"));

        // no branch filter (None) logs HEAD, which is `start` right now
        let head_log = log_commits(r.root(), 0, 10).unwrap();
        assert_eq!(head_log.len(), main_log.len());
    }

    #[test]
    fn merge_fast_forward_applies_cleanly() {
        let r = TempRepo::new("merge-ff");
        r.write("a.txt", b"base\n");
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("b.txt", b"feature\n");
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "feature work"], None).unwrap();
        checkout_branch(r.root(), start, false).unwrap();

        let outcome = merge_branch(r.root(), "feature".into(), false).unwrap();
        assert_eq!(outcome, OpOutcome::Applied);
        assert!(r.path().join("b.txt").is_file());
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn merge_no_ff_creates_merge_commit() {
        let r = TempRepo::new("merge-noff");
        r.write("a.txt", b"base\n");
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("b.txt", b"feature\n");
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "feature work"], None).unwrap();
        checkout_branch(r.root(), start, false).unwrap();

        let outcome = merge_branch(r.root(), "feature".into(), true).unwrap();
        assert_eq!(outcome, OpOutcome::Applied);
        let parents = run_git_text(r.path(), &["rev-parse", "HEAD^1", "HEAD^2"]);
        assert!(parents.is_ok(), "--no-ff must create an actual 2-parent merge commit");
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn merge_conflict_full_resolve_continue_cycle() {
        let r = TempRepo::new("merge-resolve");
        r.merge_conflict("a.txt", "base\n", "ours\n", "theirs\n");
        assert_eq!(open_repo(r.root()).unwrap().operation, RepoOperation::Merge);

        let sides = get_conflict_sides(r.root(), "a.txt".into()).unwrap();
        assert_eq!(sides.base.as_deref(), Some("base\n"));
        assert_eq!(sides.ours.as_deref(), Some("ours\n"));
        assert_eq!(sides.theirs.as_deref(), Some("theirs\n"));
        assert!(!sides.is_binary && !sides.too_large);

        resolve_conflict(r.root(), "a.txt".into(), "resolved\n".into()).unwrap();
        let files = get_status(r.root()).unwrap();
        assert!(
            !files.iter().any(|f| f.kind == ChangeKind::Conflicted),
            "no more conflicted entries once resolved and staged"
        );
        assert!(TempRepo::find(&files, "a.txt").staged, "resolved content is staged, ready for continue");

        let outcome = continue_operation(r.root()).unwrap();
        assert_eq!(outcome, OpOutcome::Applied);
        assert_eq!(open_repo(r.root()).unwrap().operation, RepoOperation::None);
        assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"resolved\n");
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn cherry_pick_conflict_resolve_continue() {
        let r = TempRepo::new("cherry-resolve");
        r.write("a.txt", b"base\n");
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("a.txt", b"feature-version\n");
        r.commit_all();
        let feature_commit = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        checkout_branch(r.root(), start, false).unwrap();
        r.write("a.txt", b"main-version\n");
        r.commit_all();

        assert_eq!(cherry_pick_commit(r.root(), feature_commit).unwrap(), OpOutcome::Conflict);
        resolve_conflict(r.root(), "a.txt".into(), "resolved\n".into()).unwrap();
        let outcome = continue_operation(r.root()).unwrap();
        assert_eq!(outcome, OpOutcome::Applied);
        assert_eq!(open_repo(r.root()).unwrap().operation, RepoOperation::None);
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn revert_conflict_resolve_continue() {
        let r = TempRepo::new("revert-resolve");
        r.write("a.txt", b"orig\n");
        r.commit_all();
        r.write("a.txt", b"v1\n");
        r.commit_all();
        let first_edit = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        r.write("a.txt", b"v2\n");
        r.commit_all();

        assert_eq!(revert_commit(r.root(), first_edit).unwrap(), OpOutcome::Conflict);
        resolve_conflict(r.root(), "a.txt".into(), "resolved\n".into()).unwrap();
        let outcome = continue_operation(r.root()).unwrap();
        assert_eq!(outcome, OpOutcome::Applied);
        assert_eq!(open_repo(r.root()).unwrap().operation, RepoOperation::None);
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn abort_operation_restores_clean_state() {
        let r = TempRepo::new("abort-op");
        r.merge_conflict("a.txt", "base\n", "ours\n", "theirs\n");
        assert_eq!(open_repo(r.root()).unwrap().operation, RepoOperation::Merge);

        abort_operation(r.root()).unwrap();
        assert_eq!(open_repo(r.root()).unwrap().operation, RepoOperation::None);
        assert_eq!(r.porcelain(), "");
        assert_eq!(std::fs::read(r.path().join("a.txt")).unwrap(), b"ours\n");
    }

    #[test]
    fn add_add_conflict_has_no_base() {
        let r = TempRepo::new("add-add-conflict");
        r.write("base.txt", b"base\n");
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("new.txt", b"feature version\n");
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "add new (feature)"], None).unwrap();
        checkout_branch(r.root(), start, false).unwrap();
        r.write("new.txt", b"main version\n");
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "add new (main)"], None).unwrap();

        let _ = run_git(r.path(), &["merge", "feature"], None); // expected to conflict
        assert_eq!(open_repo(r.root()).unwrap().operation, RepoOperation::Merge);
        let files = get_status(r.root()).unwrap();
        let conflicted = TempRepo::find(&files, "new.txt");
        assert_eq!(conflicted.conflict, Some(ConflictSide::BothAdded));

        let sides = get_conflict_sides(r.root(), "new.txt".into()).unwrap();
        assert!(sides.base.is_none(), "add/add conflicts have no common-ancestor version");
        assert_eq!(sides.ours.as_deref(), Some("main version\n"));
        assert_eq!(sides.theirs.as_deref(), Some("feature version\n"));

        resolve_conflict(r.root(), "new.txt".into(), "merged\n".into()).unwrap();
        assert_eq!(continue_operation(r.root()).unwrap(), OpOutcome::Applied);
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn delete_modify_conflict_keep_deletion() {
        let r = TempRepo::new("delete-modify-conflict");
        r.write("a.txt", b"base\n");
        r.write("keep.txt", b"keep\n");
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("a.txt", b"modified on feature\n");
        r.commit_all();
        checkout_branch(r.root(), start, false).unwrap();
        std::fs::remove_file(r.path().join("a.txt")).unwrap();
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "delete a.txt on main"], None).unwrap();

        let _ = run_git(r.path(), &["merge", "feature"], None); // expected to conflict
        let files = get_status(r.root()).unwrap();
        let conflicted = TempRepo::find(&files, "a.txt");
        assert_eq!(conflicted.conflict, Some(ConflictSide::DeletedByUs));

        let sides = get_conflict_sides(r.root(), "a.txt".into()).unwrap();
        assert!(sides.ours.is_none(), "deleted-by-us: no stage 2 version");
        assert_eq!(sides.theirs.as_deref(), Some("modified on feature\n"));

        // "keep mine" = confirm the deletion
        resolve_conflict_delete(r.root(), "a.txt".into()).unwrap();
        assert_eq!(continue_operation(r.root()).unwrap(), OpOutcome::Applied);
        assert!(!r.path().join("a.txt").exists());
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn binary_conflict_resolution_keeps_chosen_side() {
        let r = TempRepo::new("binary-conflict");
        let ours_bytes: Vec<u8> = vec![0u8, 1, 2, 3, 255];
        let theirs_bytes: Vec<u8> = vec![255u8, 254, 253, 0];
        r.write("bin.dat", &[0u8, 9, 9]);
        r.commit_all();
        let start = current_branch(&r);
        create_branch(r.root(), "feature".into(), None, true).unwrap();
        r.write("bin.dat", &theirs_bytes);
        r.commit_all();
        checkout_branch(r.root(), start, false).unwrap();
        r.write("bin.dat", &ours_bytes);
        r.commit_all();

        let _ = run_git(r.path(), &["merge", "feature"], None); // expected to conflict
        let sides = get_conflict_sides(r.root(), "bin.dat".into()).unwrap();
        assert!(sides.is_binary);

        resolve_conflict_binary(r.root(), "bin.dat".into(), ConflictTake::Ours).unwrap();
        assert_eq!(continue_operation(r.root()).unwrap(), OpOutcome::Applied);
        assert_eq!(std::fs::read(r.path().join("bin.dat")).unwrap(), ours_bytes);
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn create_file_and_dir() {
        let r = TempRepo::new("create-entries");
        r.write("seed.txt", b"seed\n");
        r.commit_all();

        create_file(r.root(), "new.txt".into()).unwrap();
        assert!(r.path().join("new.txt").exists());
        assert_eq!(std::fs::read(r.path().join("new.txt")).unwrap(), b"");

        // parent directories are created as needed
        create_file(r.root(), "nested/dir/file.txt".into()).unwrap();
        assert!(r.path().join("nested/dir/file.txt").exists());

        create_dir(r.root(), "empty/subdir".into()).unwrap();
        assert!(r.path().join("empty/subdir").is_dir());

        // creating on top of an existing path is an error, not a silent overwrite
        assert!(create_file(r.root(), "new.txt".into()).is_err());
        assert!(create_dir(r.root(), "empty/subdir".into()).is_err());

        // new files are untracked, never auto-staged
        let files = list_files(r.root(), false).unwrap();
        assert!(files.contains(&"new.txt".to_string()));
        assert!(files.contains(&"nested/dir/file.txt".to_string()));
        let status = get_status(r.root()).unwrap();
        assert!(status.iter().all(|f| f.kind != ChangeKind::Added || f.path != "new.txt"));
    }

    #[test]
    fn console_log_records_git_invocations_per_repo() {
        let a = TempRepo::new("console-a");
        let b = TempRepo::new("console-b");
        a.write("seed.txt", b"seed\n");
        a.commit_all();
        b.write("seed.txt", b"seed\n");
        b.commit_all();

        let log_a = get_console_log(a.root()).unwrap();
        assert!(!log_a.is_empty(), "commands run against repo a should be recorded");
        assert!(log_a.iter().all(|e| e.root == a.root()), "must not leak entries from other repos");
        assert!(log_a.iter().any(|e| e.args.contains("commit")));
        assert!(log_a.iter().all(|e| e.ok));

        let log_b = get_console_log(b.root()).unwrap();
        assert!(log_b.iter().all(|e| e.root == b.root()));

        // a failing command is recorded too, with ok=false
        let _ = checkout_branch(a.root(), "does-not-exist".into(), false);
        let log_a2 = get_console_log(a.root()).unwrap();
        assert!(log_a2.iter().any(|e| !e.ok));
    }

    #[test]
    fn search_commits_matches_subject_author_and_hash() {
        let r = TempRepo::new("search-commits");
        r.write("a.txt", b"a\n");
        r.commit_all();
        run_git(r.path(), &["commit", "--amend", "-qm", "add alpha feature"], None).unwrap();
        r.write("b.txt", b"b\n");
        r.commit_all();
        run_git(r.path(), &["commit", "--amend", "-qm", "fix beta bug"], None).unwrap();

        let by_subject = search_commits(r.root(), None, "alpha".into(), 10).unwrap();
        assert_eq!(by_subject.len(), 1);
        assert_eq!(by_subject[0].subject, "add alpha feature");

        // case-insensitive, matches the seeded test author's email
        let by_author = search_commits(r.root(), None, "T@TEST".into(), 10).unwrap();
        assert_eq!(by_author.len(), 2);

        let by_hash = search_commits(r.root(), None, by_subject[0].short_hash.clone(), 10).unwrap();
        assert_eq!(by_hash.len(), 1);
        assert_eq!(by_hash[0].hash, by_subject[0].hash);

        let none = search_commits(r.root(), None, "nomatch_xyz".into(), 10).unwrap();
        assert!(none.is_empty());

        // max_results caps the returned matches even when more exist
        let capped = search_commits(r.root(), None, "t@test".into(), 1).unwrap();
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn drop_commit_removes_linear_ancestor_cleanly() {
        let r = TempRepo::new("drop-commit");
        r.write("a.txt", b"1\n");
        r.commit_all();
        r.write("b.txt", b"2\n");
        r.commit_all();
        r.write("c.txt", b"3\n");
        r.commit_all();

        let commits = log_commits(r.root(), 0, 10).unwrap();
        assert_eq!(commits.len(), 3);
        let to_drop = &commits[1]; // the middle ("b.txt") commit

        let outcome = drop_commit(r.root(), to_drop.hash.clone()).unwrap();
        assert_eq!(outcome, OpOutcome::Applied);

        let after = log_commits(r.root(), 0, 10).unwrap();
        assert_eq!(after.len(), 2);
        assert!(after.iter().all(|c| c.hash != to_drop.hash));
        assert!(r.path().join("a.txt").exists());
        assert!(!r.path().join("b.txt").exists(), "the dropped commit's own change must be gone");
        assert!(r.path().join("c.txt").exists(), "commits after the dropped one must survive, replayed");
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn drop_commit_conflict_then_abort() {
        let r = TempRepo::new("drop-commit-conflict");
        r.write("f.txt", b"orig\n");
        r.commit_all();
        r.write("f.txt", b"commit2-value\n");
        r.commit_all();
        r.write("f.txt", b"commit3-value\n");
        r.commit_all();

        let commits = log_commits(r.root(), 0, 10).unwrap();
        let to_drop = &commits[1]; // the "commit2-value" commit; commit3 edits the same line

        let outcome = drop_commit(r.root(), to_drop.hash.clone()).unwrap();
        assert_eq!(outcome, OpOutcome::Conflict);
        assert_eq!(open_repo(r.root()).unwrap().operation, RepoOperation::Rebase);

        abort_operation(r.root()).unwrap();
        assert_eq!(open_repo(r.root()).unwrap().operation, RepoOperation::None);
        assert_eq!(r.porcelain(), "");
    }

    #[test]
    fn pull_branch_with_rebase_replays_local_commit_on_top() {
        let (bare_dir, local, _branch) = bare_remote_with_clone("pull-rebase");

        // a second clone pushes a commit to the shared remote
        let other_dir = std::env::temp_dir().join(format!("ai-diff-test-other-pull-rebase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other_dir);
        run_git(&std::env::temp_dir(), &["clone", "-q", bare_dir.to_str().unwrap(), other_dir.to_str().unwrap()], None).unwrap();
        run_git(&other_dir, &["config", "user.email", "t@test"], None).unwrap();
        run_git(&other_dir, &["config", "user.name", "t"], None).unwrap();
        std::fs::write(other_dir.join("remote.txt"), b"remote\n").unwrap();
        run_git(&other_dir, &["add", "-A"], None).unwrap();
        run_git(&other_dir, &["commit", "-qm", "remote change"], None).unwrap();
        run_git(&other_dir, &["push", "-q"], None).unwrap();

        // meanwhile the local clone makes its own unpushed commit
        local.write("local.txt", b"local\n");
        local.commit_all();

        let outcome = pull_branch(local.root(), None, true).unwrap();
        assert_eq!(outcome, OpOutcome::Applied);
        assert!(local.path().join("remote.txt").is_file());
        assert!(local.path().join("local.txt").is_file());
        // a rebase replays the local commit on top instead of merging — the
        // tip must still be a plain single-parent commit, not a merge commit
        let commits = log_commits(local.root(), 0, 5).unwrap();
        assert_eq!(commits[0].parents.len(), 1, "rebase must not create a merge commit");

        let _ = std::fs::remove_dir_all(&bare_dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    #[test]
    fn clone_repo_full_history() {
        let bare_dir = std::env::temp_dir().join(format!("ai-diff-test-clonebare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare_dir);
        std::fs::create_dir_all(&bare_dir).unwrap();
        run_git(&bare_dir, &["init", "-q", "--bare"], None).unwrap();

        let seed = TempRepo::new("clone-seed");
        seed.write("a.txt", b"1\n");
        seed.commit_all();
        seed.write("b.txt", b"2\n");
        seed.commit_all();
        let branch = current_branch(&seed);
        run_git(seed.path(), &["push", "-q", bare_dir.to_str().unwrap(), &format!("HEAD:{branch}")], None).unwrap();

        let dest_parent = std::env::temp_dir().join(format!("ai-diff-test-clonedest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest_parent);
        std::fs::create_dir_all(&dest_parent).unwrap();
        let dest = dest_parent.join("cloned");

        clone_repo(bare_dir.to_str().unwrap().to_string(), dest.to_str().unwrap().to_string(), None, None).unwrap();
        assert!(dest.join(".git").exists());
        assert!(dest.join("a.txt").exists());
        assert!(dest.join("b.txt").exists());
        let log = run_git_text(&dest, &["log", "--oneline"]).unwrap();
        assert_eq!(log.lines().count(), 2, "full clone must have both commits");

        let _ = std::fs::remove_dir_all(&bare_dir);
        let _ = std::fs::remove_dir_all(&dest_parent);
    }

    #[test]
    fn clone_repo_shallow_and_single_branch() {
        let bare_dir = std::env::temp_dir().join(format!("ai-diff-test-clonebare2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare_dir);
        std::fs::create_dir_all(&bare_dir).unwrap();
        run_git(&bare_dir, &["init", "-q", "--bare"], None).unwrap();

        let seed = TempRepo::new("clone-seed2");
        seed.write("a.txt", b"1\n");
        seed.commit_all();
        let branch = current_branch(&seed);
        run_git(seed.path(), &["push", "-q", bare_dir.to_str().unwrap(), &format!("HEAD:{branch}")], None).unwrap();
        seed.write("b.txt", b"2\n");
        seed.commit_all();
        run_git(seed.path(), &["push", "-q", bare_dir.to_str().unwrap(), &format!("HEAD:{branch}")], None).unwrap();
        // a second branch on the remote, which single-branch clone must not fetch
        run_git(seed.path(), &["branch", "other-branch"], None).unwrap();
        run_git(seed.path(), &["push", "-q", bare_dir.to_str().unwrap(), "other-branch"], None).unwrap();

        let dest_parent = std::env::temp_dir().join(format!("ai-diff-test-clonedest2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest_parent);
        std::fs::create_dir_all(&dest_parent).unwrap();
        let dest = dest_parent.join("cloned");

        // git silently ignores --depth for a plain local-path clone ("use
        // file:// instead") — use a file:// URL so shallow clone actually applies
        let file_url = format!("file:///{}", bare_dir.to_str().unwrap().replace('\\', "/"));
        clone_repo(file_url, dest.to_str().unwrap().to_string(), Some(branch.clone()), Some(1)).unwrap();
        assert!(dest.join(".git").exists());
        let log = run_git_text(&dest, &["log", "--oneline"]).unwrap();
        assert_eq!(log.lines().count(), 1, "depth=1 must only fetch the tip commit");
        let branches = run_git_text(&dest, &["branch", "-r"]).unwrap();
        assert!(!branches.contains("other-branch"), "single-branch clone must not fetch other branches");

        let _ = std::fs::remove_dir_all(&bare_dir);
        let _ = std::fs::remove_dir_all(&dest_parent);
    }

    #[test]
    fn write_file_overwrites_and_shows_as_modified() {
        let r = TempRepo::new("write-file");
        r.write("a.txt", b"original\n");
        r.commit_all();

        write_file(r.root(), "a.txt".into(), "edited\n".into()).unwrap();

        let on_disk = std::fs::read_to_string(r.path().join("a.txt")).unwrap();
        assert_eq!(on_disk, "edited\n");

        let status = get_status(r.root()).unwrap();
        let f = status.iter().find(|f| f.path == "a.txt").expect("a.txt must show up as changed");
        assert_eq!(f.kind, ChangeKind::Modified);
    }

    #[test]
    fn get_commit_message_returns_full_body_not_just_subject() {
        let r = TempRepo::new("full-msg");
        r.write("a.txt", b"1\n");
        run_git(r.path(), &["add", "-A"], None).unwrap();
        run_git(r.path(), &["commit", "-qm", "subject line\n\nbody line 1\nbody line 2"], None).unwrap();

        let hash = run_git_text(r.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let msg = get_commit_message(r.root(), hash).unwrap();

        assert_eq!(msg, "subject line\n\nbody line 1\nbody line 2");
    }

    #[test]
    fn repo_stats_counts_lines_per_language_and_skips_untracked() {
        let r = TempRepo::new("stats");
        r.write("main.rs", b"fn main() {\n    println!(\"hi\");\n}\n"); // 3 lines
        r.write("lib.rs", b"pub fn add(a: i32, b: i32) -> i32 { a + b }"); // 1 line, no trailing newline
        r.write("index.ts", b"export const x = 1;\nexport const y = 2;\n"); // 2 lines
        r.write("README.md", b"# stats\n"); // 1 line, excluded via .gitignore below
        r.write(".gitignore", b"README.md\n");
        r.commit_all();
        // untracked — must not be counted at all
        r.write("scratch.rs", b"fn scratch() {}\nfn more() {}\nfn even_more() {}\n");

        let stats = repo_stats(r.root()).unwrap();

        assert_eq!(stats.total_files, 3, "2 tracked .rs files + index.ts (README is gitignored, scratch.rs is untracked)");
        assert_eq!(stats.total_lines, 6, "3 (main.rs) + 1 (lib.rs) + 2 (index.ts)");

        let rust = stats.languages.iter().find(|l| l.language == "Rust").expect("Rust must be in the breakdown");
        assert_eq!(rust.files, 2);
        assert_eq!(rust.lines, 4, "lib.rs has no trailing newline but its one line must still count");

        assert!(
            !stats.languages.iter().any(|l| l.language == "Markdown"),
            "README.md is gitignored and must not appear despite being on disk"
        );
        // index.ts wasn't committed as part of `commit_all` staging — it was
        // written before commit_all() ran, so it IS tracked; confirm it landed
        // in its own TypeScript bucket rather than being merged into Rust
        let ts = stats.languages.iter().find(|l| l.language == "TypeScript");
        assert!(ts.is_some(), "index.ts must appear as TypeScript");
        assert_eq!(ts.unwrap().lines, 2);
    }
}
