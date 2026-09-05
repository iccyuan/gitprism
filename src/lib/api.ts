import { invoke } from "@tauri-apps/api/core";

export type ChangeKind = "modified" | "added" | "deleted" | "renamed" | "untracked" | "conflicted";

export type RepoOperation = "none" | "merge" | "cherryPick" | "revert" | "rebase";

export type ConflictSide =
  | "bothModified"
  | "addedByUs"
  | "addedByThem"
  | "deletedByUs"
  | "deletedByThem"
  | "bothAdded"
  | "bothDeleted";

export interface RepoInfo {
  root: string;
  branch: string | null;
  hasHead: boolean;
  upstream: string | null;
  ahead: number | null;
  behind: number | null;
  operation: RepoOperation;
}

export interface ConflictSides {
  base: string | null;
  ours: string | null;
  theirs: string | null;
  isBinary: boolean;
  tooLarge: boolean;
}

export type ConflictTake = "ours" | "theirs";

export interface FileStatus {
  path: string;
  oldPath: string | null;
  kind: ChangeKind;
  additions: number | null;
  deletions: number | null;
  conflict: ConflictSide | null;
  /** index-vs-HEAD (true) or worktree-vs-index (false); a partially-staged
   * file legitimately appears twice, once with each value */
  staged: boolean;
}

export interface BranchInfo {
  name: string;
  isCurrent: boolean;
  isRemote: boolean;
  upstream: string | null;
  /** upstream configured but deleted on the remote (and pruned locally) */
  upstreamGone: boolean;
  ahead: number | null;
  behind: number | null;
}

/** how much of the gitignored set the 全部文件 tree lists:
 * hide = none, files = ignored files in the normal tree (dimmed) but not
 * ignored directories, all = everything incl. node_modules-style dirs */
export type IgnoredMode = "hide" | "files" | "all";

/** list_files result: every path in the project view plus the gitignored subset */
export interface FileListing {
  files: string[];
  ignored: string[];
}

/** `repo-changed` watcher event; indexOnly = only .git/index was rewritten,
 * which our own refreshes (git status) also do */
export interface RepoChangedEvent {
  root: string;
  indexOnly: boolean;
}

export interface RemoteInfo {
  name: string;
  url: string;
}

export type ForceMode = "none" | "lease" | "force";

export interface CommitInfo {
  hash: string;
  shortHash: string;
  author: string;
  email: string;
  date: string;
  subject: string;
  additions: number;
  deletions: number;
  /** >1 means a merge commit — cherry-pick/revert are disabled for these */
  parents: string[];
  /** branch/tag names pointing at this commit, e.g. ["main", "origin/main"] */
  refs: string[];
}

export type OpOutcome = "applied" | "conflict";
export type ResetMode = "soft" | "mixed" | "hard";

export interface Hunk {
  index: number;
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  text: string;
}

export interface FileDiff {
  original: string | null;
  modified: string | null;
  isBinary: boolean;
  tooLarge: boolean;
  fileHeader: string;
  hunks: Hunk[];
}

export interface FileContent {
  content: string | null;
  isBinary: boolean;
  tooLarge: boolean;
}

export interface FileInfo {
  name: string;
  path: string;
  fullPath: string;
  exists: boolean;
  isDir: boolean;
  isSymlink: boolean;
  size: number;
  readonly: boolean;
  /** modification / creation time as unix-epoch milliseconds */
  modified: number | null;
  created: number | null;
  lines: number | null;
}

export interface ImageDiff {
  /** data URL of the HEAD version (null for added/untracked) */
  original: string | null;
  /** data URL of the working-tree version (null for deleted) */
  modified: string | null;
}

export interface ConsoleEntry {
  root: string;
  args: string;
  ok: boolean;
  atMs: number;
}

export interface LangStat {
  language: string;
  color: string;
  lines: number;
  files: number;
}

export interface RepoStats {
  totalFiles: number;
  totalLines: number;
  languages: LangStat[];
  skippedBinary: number;
  skippedTooLarge: number;
}

export const api = {
  openRepo: (path: string) => invoke<RepoInfo>("open_repo", { path }),
  getStatus: (repo: string) => invoke<FileStatus[]>("get_status", { repo }),
  getFileDiff: (repo: string, f: FileStatus) =>
    invoke<FileDiff>("get_file_diff", {
      repo,
      path: f.path,
      oldPath: f.oldPath,
      kind: f.kind,
      staged: f.staged,
    }),
  revertFile: (repo: string, f: FileStatus) =>
    invoke<void>("revert_file", { repo, path: f.path, oldPath: f.oldPath, kind: f.kind }),
  deletePaths: (repo: string, paths: string[]) => invoke<void>("delete_paths", { repo, paths }),
  listStashes: (repo: string) => invoke<StashInfo[]>("list_stashes", { repo }),
  stashPush: (repo: string, message: string, paths: string[]) =>
    invoke<void>("stash_push", { repo, message, paths }),
  stashPop: (repo: string, index: number) => invoke<void>("stash_pop", { repo, index }),
  stashDrop: (repo: string, index: number) => invoke<void>("stash_drop", { repo, index }),
  revertHunk: (repo: string, path: string, patch: string) =>
    invoke<void>("revert_hunk", { repo, path, patch }),
  revertAll: (repo: string) => invoke<void>("revert_all", { repo }),
  stageFile: (repo: string, f: FileStatus) =>
    invoke<void>("stage_file", { repo, path: f.path, oldPath: f.oldPath }),
  unstageFile: (repo: string, f: FileStatus) =>
    invoke<void>("unstage_file", { repo, path: f.path, oldPath: f.oldPath, kind: f.kind }),
  stageAll: (repo: string) => invoke<void>("stage_all", { repo }),
  unstageAll: (repo: string) => invoke<void>("unstage_all", { repo }),
  stageHunk: (repo: string, path: string, patch: string) =>
    invoke<void>("stage_hunk", { repo, path, patch }),
  unstageHunk: (repo: string, path: string, patch: string) =>
    invoke<void>("unstage_hunk", { repo, path, patch }),
  createCommit: (repo: string, message: string, amend: boolean) =>
    invoke<void>("create_commit", { repo, message, amend }),
  commitPaths: (repo: string, message: string, paths: string[]) =>
    invoke<void>("commit_paths", { repo, message, paths }),
  listBranches: (repo: string) => invoke<BranchInfo[]>("list_branches", { repo }),
  createBranch: (repo: string, name: string, startPoint: string | null, checkout: boolean) =>
    invoke<void>("create_branch", { repo, name, startPoint, checkout }),
  checkoutBranch: (repo: string, name: string, isRemote: boolean) =>
    invoke<void>("checkout_branch", { repo, name, isRemote }),
  deleteBranch: (repo: string, name: string, force: boolean) =>
    invoke<void>("delete_branch", { repo, name, force }),
  renameBranch: (repo: string, oldName: string, newName: string) =>
    invoke<void>("rename_branch", { repo, old: oldName, new: newName }),
  listRemotes: (repo: string) => invoke<RemoteInfo[]>("list_remotes", { repo }),
  cloneRepo: (url: string, dest: string, branch: string | null, depth: number | null) =>
    invoke<void>("clone_repo", { url, dest, branch, depth }),
  fetchRemote: (repo: string, remote: string | null, prune: boolean) =>
    invoke<void>("fetch_remote", { repo, remote, prune }),
  pullBranch: (repo: string, remote: string | null, rebase: boolean) =>
    invoke<OpOutcome>("pull_branch", { repo, remote, rebase }),
  pushBranch: (repo: string, remote: string, branch: string, setUpstream: boolean, force: ForceMode) =>
    invoke<void>("push_branch", { repo, remote, branch, setUpstream, force }),
  cherryPickCommit: (repo: string, hash: string) => invoke<OpOutcome>("cherry_pick_commit", { repo, hash }),
  revertCommit: (repo: string, hash: string) => invoke<OpOutcome>("revert_commit", { repo, hash }),
  resetTo: (repo: string, hash: string, mode: ResetMode) => invoke<void>("reset_to", { repo, hash, mode }),
  countCommitsBetween: (repo: string, from: string, to: string) =>
    invoke<number>("count_commits_between", { repo, from, to }),
  dropCommit: (repo: string, hash: string) => invoke<OpOutcome>("drop_commit", { repo, hash }),
  mergeBranch: (repo: string, source: string, noFf: boolean) =>
    invoke<OpOutcome>("merge_branch", { repo, source, noFf }),
  getConflictSides: (repo: string, path: string) => invoke<ConflictSides>("get_conflict_sides", { repo, path }),
  resolveConflict: (repo: string, path: string, content: string) =>
    invoke<void>("resolve_conflict", { repo, path, content }),
  resolveConflictBinary: (repo: string, path: string, take: ConflictTake) =>
    invoke<void>("resolve_conflict_binary", { repo, path, take }),
  resolveConflictDelete: (repo: string, path: string) => invoke<void>("resolve_conflict_delete", { repo, path }),
  continueOperation: (repo: string) => invoke<OpOutcome>("continue_operation", { repo }),
  abortOperation: (repo: string) => invoke<void>("abort_operation", { repo }),
  watchRepo: (repo: string) => invoke<void>("watch_repo", { repo }),
  unwatchRepo: (repo: string) => invoke<void>("unwatch_repo", { repo }),
  listFiles: (repo: string, mode: IgnoredMode) => invoke<FileListing>("list_files", { repo, mode }),
  readFile: (repo: string, path: string) => invoke<FileContent>("read_file", { repo, path }),
  writeFile: (repo: string, path: string, content: string) => invoke<void>("write_file", { repo, path, content }),
  repoStats: (repo: string) => invoke<RepoStats>("repo_stats", { repo }),
  logCommits: (repo: string, skip: number, count: number, branch: string | null = null, author: string | null = null) =>
    invoke<CommitInfo[]>("log_commits", { repo, skip, count, branch, author }),
  searchCommits: (
    repo: string,
    branch: string | null,
    query: string,
    maxResults = 200,
    author: string | null = null,
    subject: string | null = null,
    hash: string | null = null,
  ) => invoke<CommitInfo[]>("search_commits", { repo, branch, query, maxResults, author, subject, hash }),
  listAuthors: (repo: string) => invoke<AuthorInfo[]>("list_authors", { repo }),
  replaceInFiles: (repo: string, query: string, replacement: string, wholeWord: boolean, paths: string[] | null) =>
    invoke<ReplaceResult>("replace_in_files", { repo, query, replacement, wholeWord, paths }),
  commitFiles: (repo: string, hash: string) => invoke<FileStatus[]>("commit_files", { repo, hash }),
  getCommitMessage: (repo: string, hash: string) => invoke<string>("get_commit_message", { repo, hash }),
  blameFile: (repo: string, path: string) => invoke<BlameLine[]>("blame_file", { repo, path }),
  getCommitFileDiff: (repo: string, hash: string, f: FileStatus) =>
    invoke<FileDiff>("get_commit_file_diff", {
      repo,
      hash,
      path: f.path,
      oldPath: f.oldPath,
      kind: f.kind,
    }),
  searchText: (repo: string, query: string, wholeWord: boolean, max = 500) =>
    invoke<SearchHit[]>("search_text", { repo, query, wholeWord, max }),
  getImageDiff: (repo: string, path: string, oldPath: string | null, kind: ChangeKind) =>
    invoke<ImageDiff>("get_image_diff", { repo, path, oldPath, kind }),
  fileInfo: (repo: string, path: string) => invoke<FileInfo>("file_info", { repo, path }),
  createFile: (repo: string, path: string) => invoke<void>("create_file", { repo, path }),
  createDir: (repo: string, path: string) => invoke<void>("create_dir", { repo, path }),
  getConsoleLog: (repo: string) => invoke<ConsoleEntry[]>("get_console_log", { repo }),
};

export interface SearchHit {
  path: string;
  line: number;
  text: string;
}

/** outcome of a Replace-in-Files run */
export interface ReplaceResult {
  files: number;
  replacements: number;
}

/** distinct commit author (IDEA-style log author filter) */
export interface AuthorInfo {
  name: string;
  email: string;
  commits: number;
}

/** IDEA-shelve-style stash entry; `hash` is the stash commit (diff vs its
 * first parent = the shelved tracked changes), `untrackedHash` the commit
 * holding shelved untracked files, when any */
export interface StashInfo {
  index: number;
  message: string;
  date: string;
  hash: string;
  untrackedHash: string | null;
}

/** one working-tree line's blame record (IDEA-style gutter annotate) */
export interface BlameLine {
  hash: string;
  author: string;
  /** author date as unix seconds; 0 for not-yet-committed lines */
  time: number;
  summary: string;
  committed: boolean;
}
