import { acceptHMRUpdate, defineStore } from "pinia";
import { listen } from "@tauri-apps/api/event";
import {
  api,
  type BranchInfo,
  type CommitInfo,
  type FileContent,
  type FileDiff,
  type FileStatus,
  type ForceMode,
  type Hunk,
  type IgnoredMode,
  type RepoChangedEvent,
  type RepoInfo,
  type ResetMode,
} from "../lib/api";
import { toast, updateToast } from "../lib/toast";
import { hasLogQuery, parseLogQuery } from "../lib/logQuery";
import { useSettingsStore } from "./settings";

let watcherHooked = false;
// our own refreshes touch .git/index (git status refreshes stat info) and
// echo back as index-only watcher events; those are dropped for this long
let suppressAutoRefreshUntil = 0;
// one refresh per workspace at a time; a request that lands mid-refresh runs
// once more afterwards instead of being dropped — a commit/push that finishes
// while we're still reading must not leave stale branch/ahead state behind
const refreshRuns = new Map<string, { promise: Promise<void>; again: boolean }>();
// revealLogCommit re-entry guard: rapid blame clicks must not stack loads
let revealInFlight = false;

export interface ViewTab {
  id: string;
  title: string;
  path: string;
  /** commit hash for history tabs, null for working-tree / file-content tabs */
  commit: string | null;
  /** status snapshot needed by get_commit_file_diff */
  file: FileStatus | null;
  /** which side of a partially-staged file this working-tree tab shows;
   * absent/false for the common (not-partially-staged) case */
  staged?: boolean;
}

/** everything one open project needs; the window can hold several of these */
export interface Workspace {
  repo: RepoInfo;
  files: FileStatus[];
  allFiles: string[];
  /** subset of allFiles that .gitignore excludes — shown dimmed */
  ignoredFiles: string[];
  selectedPath: string | null;
  diff: FileDiff | null;
  content: FileContent | null;
  loadingStatus: boolean;
  loadingDiff: boolean;
  /** bumped each time refreshWs completes — an explicit "data just changed"
   * signal for consumers (e.g. the Git panel's console tab) instead of
   * inferring it from the loadingStatus spinner flag */
  refreshSeq: number;
  commitPageSize: number;
  commits: CommitInfo[];
  commitsExhausted: boolean;
  loadingCommits: boolean;
  /** when set, the 日志 view is scoped to this branch/ref instead of HEAD */
  logBranchFilter: string | null;
  /** when set, the 日志 view only shows commits by this author */
  logAuthorFilter: string | null;
  /** when set, the 日志 view shows search_commits matches instead of a plain
   * paginated log — cleared to go back to normal browsing */
  logSearchQuery: string | null;
  /** commit highlighted in the 日志 table — its files show in the right-hand
   * panel, IDEA-style (left: graph/table, right: changed files) */
  logActiveCommit: string | null;
  commitFiles: Record<string, FileStatus[]>;
  selectedCommit: string | null;
  selectedCommitPath: string | null;
  tabs: ViewTab[];
  activeTabId: string | null;
  pendingRevealLine: number | null;
  branches: BranchInfo[];
  /** per-workspace so a long-running network op on one project doesn't lock the others */
  busyOp: "fetch" | "pull" | "push" | null;
}

function blankWorkspace(info: RepoInfo): Workspace {
  return {
    repo: info,
    files: [],
    allFiles: [],
    ignoredFiles: [],
    selectedPath: null,
    diff: null,
    content: null,
    loadingStatus: false,
    loadingDiff: false,
    refreshSeq: 0,
    commitPageSize: 30,
    commits: [],
    commitsExhausted: false,
    loadingCommits: false,
    logBranchFilter: null,
    logAuthorFilter: null,
    logSearchQuery: null,
    logActiveCommit: null,
    commitFiles: {},
    selectedCommit: null,
    selectedCommitPath: null,
    tabs: [],
    activeTabId: null,
    pendingRevealLine: null,
    branches: [],
    busyOp: null,
  };
}

function basename(path: string): string {
  return path.split("/").pop()!;
}

/** live-updates a "正在 fetch/pull…" toast with git's own --progress lines
 *  (received objects, compression, …) as they arrive, instead of leaving it
 *  static until the whole operation finishes. Caller must invoke the
 *  returned unlisten fn once the operation settles. */
async function listenGitProgress(root: string, toastId: number) {
  return listen<[string, string]>("git-progress", (e) => {
    const [evRoot, line] = e.payload;
    if (evRoot === root) updateToast(toastId, line);
  });
}

export const useRepoStore = defineStore("repo", {
  state: () => ({
    workspaces: [] as Workspace[],
    active: -1,
    /** bumped by revealLogCommit — GitPanel switches to 日志 and
     * HistoryPanel scrolls the selected commit into view */
    logRevealSeq: 0,
    /** bumped after shelving — GitPanel switches to the 搁置 tab so the
     * fresh entry is immediately visible */
    shelfRevealSeq: 0,
    /** sidebar view mode is global: one switch for every project */
    viewMode: "changes" as "changes" | "all",
  }),
  getters: {
    ws(state): Workspace | null {
      return state.workspaces[state.active] ?? null;
    },
    repo(): RepoInfo | null {
      return this.ws?.repo ?? null;
    },
    files(): FileStatus[] {
      return this.ws?.files ?? [];
    },
    allFiles(): string[] {
      return this.ws?.allFiles ?? [];
    },
    ignoredFiles(): Set<string> {
      return new Set(this.ws?.ignoredFiles ?? []);
    },
    mode(state): "changes" | "all" {
      return state.viewMode;
    },
    selectedPath(): string | null {
      return this.ws?.selectedPath ?? null;
    },
    diff(): FileDiff | null {
      return this.ws?.diff ?? null;
    },
    content(): FileContent | null {
      return this.ws?.content ?? null;
    },
    loadingStatus(): boolean {
      return this.ws?.loadingStatus ?? false;
    },
    loadingDiff(): boolean {
      return this.ws?.loadingDiff ?? false;
    },
    refreshSeq(): number {
      return this.ws?.refreshSeq ?? 0;
    },
    commitPageSize(): number {
      return this.ws?.commitPageSize ?? 30;
    },
    commits(): CommitInfo[] {
      return this.ws?.commits ?? [];
    },
    commitsExhausted(): boolean {
      return this.ws?.commitsExhausted ?? false;
    },
    loadingCommits(): boolean {
      return this.ws?.loadingCommits ?? false;
    },
    logBranchFilter(): string | null {
      return this.ws?.logBranchFilter ?? null;
    },
    logAuthorFilter(): string | null {
      return this.ws?.logAuthorFilter ?? null;
    },
    logSearchQuery(): string | null {
      return this.ws?.logSearchQuery ?? null;
    },
    logActiveCommit(): string | null {
      return this.ws?.logActiveCommit ?? null;
    },
    commitFiles(): Record<string, FileStatus[]> {
      return this.ws?.commitFiles ?? {};
    },
    selectedCommit(): string | null {
      return this.ws?.selectedCommit ?? null;
    },
    selectedCommitPath(): string | null {
      return this.ws?.selectedCommitPath ?? null;
    },
    tabs(): ViewTab[] {
      return this.ws?.tabs ?? [];
    },
    activeTabId(): string | null {
      return this.ws?.activeTabId ?? null;
    },
    pendingRevealLine(): number | null {
      return this.ws?.pendingRevealLine ?? null;
    },
    selected(): FileStatus | null {
      const w = this.ws;
      if (!w) return null;
      const activeTab = w.tabs.find((t) => t.id === w.activeTabId);
      const staged = !!activeTab?.staged;
      return w.files.find((f) => f.path === w.selectedPath && !!f.staged === staged) ?? null;
    },
    /** whether the currently-open working-tree diff is the staged (vs unstaged) side */
    activeTabStaged(): boolean {
      const w = this.ws;
      return !!w?.tabs.find((t) => t.id === w.activeTabId)?.staged;
    },
  },
  actions: {
    /** open a repo as a workspace; re-opening an existing one just activates it */
    async openRepo(path: string) {
      try {
        const info = await api.openRepo(path);
        const existing = this.workspaces.findIndex((w) => w.repo.root === info.root);
        if (existing >= 0) {
          this.active = existing;
          this.workspaces[existing].repo = info;
          await this.refreshWs(this.workspaces[existing]);
          return;
        }
        this.workspaces.push(blankWorkspace(info));
        this.active = this.workspaces.length - 1;
        // use the reactive proxy from the array, NOT the raw pushed object —
        // mutating the raw object updates values without triggering reactivity
        const w = this.workspaces[this.active];
        await useSettingsStore().addRecent(info.root);
        await this.refreshWs(w);
        await api.watchRepo(info.root);
        if (!watcherHooked) {
          watcherHooked = true;
          await listen<RepoChangedEvent>("repo-changed", (e) => {
            const target = this.workspaces.find((x) => x.repo.root === e.payload.root);
            if (!target) return;
            // an index-only event during/right after our own refresh is that
            // refresh's echo. Anything else (HEAD, refs, worktree files) is a
            // real external change and always triggers a refresh — queued if
            // one is in flight, never dropped
            if (e.payload.indexOnly && (target.loadingStatus || Date.now() < suppressAutoRefreshUntil)) return;
            this.refreshWs(target);
          });
        }
      } catch (e) {
        toast(String(e), "error");
      }
    },
    activateWorkspace(i: number) {
      if (i < 0 || i >= this.workspaces.length) return;
      this.active = i;
      if (this.viewMode === "all") this.ensureAllFiles();
    },
    moveWorkspace(from: number, to: number) {
      const n = this.workspaces.length;
      if (from === to || from < 0 || to < 0 || from >= n || to >= n) return;
      const act = this.workspaces[this.active] ?? null;
      const [w] = this.workspaces.splice(from, 1);
      this.workspaces.splice(to, 0, w);
      if (act) this.active = this.workspaces.indexOf(act);
    },
    closeWorkspace(i: number) {
      const w = this.workspaces[i];
      if (!w) return;
      api.unwatchRepo(w.repo.root).catch(() => {});
      this.workspaces.splice(i, 1);
      if (this.active >= this.workspaces.length) this.active = this.workspaces.length - 1;
      else if (this.active > i) this.active--;
    },
    async refresh() {
      if (this.ws) await this.refreshWs(this.ws);
    },
    /**
     * The single consistency rule: every mutation (any revert) must end here,
     * so hunk offsets are always re-derived from the current file state.
     */
    async refreshWs(w: Workspace): Promise<void> {
      const key = w.repo.root;
      const running = refreshRuns.get(key);
      if (running) {
        running.again = true;
        return running.promise;
      }
      const run = { again: false, promise: Promise.resolve() };
      run.promise = (async () => {
        try {
          do {
            run.again = false;
            await this.refreshOnce(w);
          } while (run.again);
        } finally {
          refreshRuns.delete(key);
        }
      })();
      refreshRuns.set(key, run);
      return run.promise;
    },
    async refreshOnce(w: Workspace) {
      w.loadingStatus = true;
      try {
        // re-read repo info so a branch switch (or HEAD change) reflects live
        w.repo = await api.openRepo(w.repo.root);
        w.files = await api.getStatus(w.repo.root);
        w.branches = await api.listBranches(w.repo.root);
        if (this.viewMode === "all") {
          await this.loadAllFiles(w);
        }
        if (w.commits.length) {
          // re-fetch the already-loaded depth so new commits surface on top;
          // skipped until the Git panel's Log tab has loaded commits at least once
          const count = Math.max(w.commits.length, w.commitPageSize);
          const list = await api.logCommits(w.repo.root, 0, count, w.logBranchFilter, w.logAuthorFilter);
          w.commits = list;
          w.commitsExhausted = list.length < count;
        }
        const active = w.tabs.find((t) => t.id === w.activeTabId);
        if (active) {
          // worktree tabs auto-degrade to read-only content once the file is clean
          await this.loadForTab(w, active);
        }
      } catch (e) {
        toast(String(e), "error");
      } finally {
        w.loadingStatus = false;
        w.refreshSeq++;
        suppressAutoRefreshUntil = Date.now() + 800;
      }
    },
    async setMode(mode: "changes" | "all") {
      if (this.viewMode === mode) return;
      this.viewMode = mode;
      if (mode === "all") await this.ensureAllFiles();
      // the active working-tree tab flips diff <-> content with the mode
      const w = this.ws;
      const active = w?.tabs.find((t) => t.id === w.activeTabId);
      if (w && active && !active.commit) await this.loadForTab(w, active);
    },
    async loadAllFiles(w: Workspace) {
      const listing = await api.listFiles(w.repo.root, useSettingsStore().ignoredMode);
      w.allFiles = listing.files;
      w.ignoredFiles = listing.ignored;
    },
    async ensureAllFiles(force = false) {
      const w = this.ws;
      if (!w || (w.allFiles.length && !force)) return;
      try {
        await this.loadAllFiles(w);
      } catch (e) {
        toast(String(e), "error");
      }
    },
    /** pick how much of the gitignored set the tree lists and rebuild the
     * all-files list everywhere */
    async setIgnoredMode(mode: IgnoredMode) {
      const settings = useSettingsStore();
      if (settings.ignoredMode === mode) return;
      await settings.setIgnoredMode(mode);
      // every workspace's cached list is now stale; clear so it re-fetches lazily
      for (const w of this.workspaces) {
        w.allFiles = [];
        w.ignoredFiles = [];
      }
      if (this.viewMode !== "all") this.viewMode = "all"; // make the effect visible
      await this.ensureAllFiles();
    },
    /** open a file (worktree view) and scroll to a specific line */
    async openAtLine(path: string, line: number) {
      const w = this.ws;
      if (!w) return;
      w.pendingRevealLine = line;
      await this.selectPath(path);
    },
    clearPendingReveal() {
      if (this.ws) this.ws.pendingRevealLine = null;
    },
    async selectFile(f: FileStatus) {
      await this.selectPath(f.path, f.staged);
    },
    /** open (or focus) a working-tree tab: 更改 shows a diff for changed files, 全部文件 shows content.
     * `staged` distinguishes the two tabs a partially-staged file can have;
     * the plain `wt:${path}` id is left untouched for every other caller. */
    async selectPath(path: string, staged = false) {
      await this.openTab({
        id: staged ? `wt:staged:${path}` : `wt:${path}`,
        title: basename(path),
        path,
        commit: null,
        file: null,
        staged,
      });
    },
    async selectCommitFile(hash: string, f: FileStatus) {
      await this.openTab({
        id: `${hash.slice(0, 12)}:${f.path}`,
        title: basename(f.path),
        path: f.path,
        commit: hash,
        file: f,
      });
    },
    /** ----- viewer tabs ----- */
    async openTab(tab: ViewTab) {
      const w = this.ws;
      if (!w) return;
      if (!w.tabs.some((t) => t.id === tab.id)) w.tabs.push(tab);
      await this.activateTab(tab.id);
    },
    async activateTab(id: string) {
      const w = this.ws;
      const tab = w?.tabs.find((t) => t.id === id);
      if (!w || !tab) return;
      w.activeTabId = id;
      if (tab.commit) {
        w.selectedPath = null;
        w.selectedCommit = tab.commit;
        w.selectedCommitPath = tab.path;
      } else {
        w.selectedCommit = null;
        w.selectedCommitPath = null;
        w.selectedPath = tab.path;
      }
      await this.loadForTab(w, tab);
    },
    async loadForTab(w: Workspace, tab: ViewTab) {
      if (tab.commit && tab.file) {
        w.loadingDiff = true;
        w.content = null;
        try {
          w.diff = await api.getCommitFileDiff(w.repo.root, tab.commit, tab.file);
        } catch (e) {
          w.diff = null;
          toast(String(e), "error");
        } finally {
          w.loadingDiff = false;
        }
        return;
      }
      // 更改 view shows a diff for changed files; 全部文件 always shows content.
      // match `staged` too — a partially-staged file has two distinct entries.
      const st = w.files.find((f) => f.path === tab.path && !!f.staged === !!tab.staged);
      if (st && this.viewMode === "changes") {
        await this.loadDiff(w, st);
      } else {
        await this.loadContent(w, tab.path);
      }
    },
    moveTab(from: number, to: number) {
      const w = this.ws;
      if (!w) return;
      const n = w.tabs.length;
      if (from === to || from < 0 || to < 0 || from >= n || to >= n) return;
      const [t] = w.tabs.splice(from, 1);
      w.tabs.splice(to, 0, t);
    },
    closeTab(id: string) {
      const w = this.ws;
      if (!w) return;
      const i = w.tabs.findIndex((t) => t.id === id);
      if (i < 0) return;
      w.tabs.splice(i, 1);
      if (w.activeTabId !== id) return;
      const next = w.tabs[i] ?? w.tabs[i - 1];
      if (next) {
        this.activateTab(next.id);
      } else {
        this.clearActiveView();
      }
    },
    /** Eclipse keymap's Ctrl+PageUp/PageDown ("Previous/Next Editor") */
    cycleTab(direction: 1 | -1) {
      const w = this.ws;
      if (!w || !w.tabs.length) return;
      const i = w.tabs.findIndex((t) => t.id === w.activeTabId);
      const n = w.tabs.length;
      const next = i < 0 ? 0 : (i + direction + n) % n;
      this.activateTab(w.tabs[next].id);
    },
    clearActiveView() {
      const w = this.ws;
      if (!w) return;
      w.activeTabId = null;
      w.selectedPath = null;
      w.selectedCommit = null;
      w.selectedCommitPath = null;
      w.diff = null;
      w.content = null;
    },
    closeAllTabs() {
      const w = this.ws;
      if (!w) return;
      w.tabs = [];
      this.clearActiveView();
    },
    closeOtherTabs(id: string) {
      const w = this.ws;
      if (!w) return;
      const keep = w.tabs.find((t) => t.id === id);
      if (!keep) return;
      w.tabs = [keep];
      if (w.activeTabId !== id) this.activateTab(id);
    },
    closeLeftTabs(id: string) {
      const w = this.ws;
      if (!w) return;
      const i = w.tabs.findIndex((t) => t.id === id);
      if (i <= 0) return;
      const closingActive = w.tabs.slice(0, i).some((t) => t.id === w.activeTabId);
      w.tabs = w.tabs.slice(i);
      if (closingActive) this.activateTab(id);
    },
    closeRightTabs(id: string) {
      const w = this.ws;
      if (!w) return;
      const i = w.tabs.findIndex((t) => t.id === id);
      if (i < 0 || i === w.tabs.length - 1) return;
      const closingActive = w.tabs.slice(i + 1).some((t) => t.id === w.activeTabId);
      w.tabs = w.tabs.slice(0, i + 1);
      if (closingActive) this.activateTab(id);
    },
    async loadDiff(w: Workspace, f: FileStatus) {
      w.loadingDiff = true;
      w.content = null;
      try {
        w.diff = await api.getFileDiff(w.repo.root, f);
      } catch (e) {
        w.diff = null;
        toast(String(e), "error");
      } finally {
        w.loadingDiff = false;
      }
    },
    async loadContent(w: Workspace, path: string) {
      w.loadingDiff = true;
      w.diff = null;
      try {
        w.content = await api.readFile(w.repo.root, path);
      } catch (e) {
        w.content = null;
        toast(String(e), "error");
      } finally {
        w.loadingDiff = false;
      }
    },
    async revertFile(f: FileStatus) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.revertFile(w.repo.root, f);
        toast(f.kind === "untracked" ? `已删除 ${f.path}` : `已还原 ${f.path}`);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** delete files from disk — tracked ones become "deleted" changes */
    async deleteFiles(paths: string[]) {
      const w = this.ws;
      if (!w || !paths.length) return;
      try {
        await api.deletePaths(w.repo.root, paths);
        toast(`已删除 ${paths.length} 个文件`);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** revert several files in one go, refreshing only once at the end */
    async revertFiles(files: FileStatus[]) {
      const w = this.ws;
      if (!w || !files.length) return;
      try {
        for (const f of files) await api.revertFile(w.repo.root, f);
        toast(`已还原 ${files.length} 个文件`);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async revertHunk(hunk: Hunk) {
      const w = this.ws;
      if (!w || !w.diff || !this.selected) return;
      const patch = w.diff.fileHeader + hunk.text;
      try {
        await api.revertHunk(w.repo.root, this.selected.path, patch);
        toast("已还原该修改块");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async revertAll() {
      const w = this.ws;
      if (!w) return;
      try {
        await api.revertAll(w.repo.root);
        toast("已还原全部更改");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** ----- staging + commit ----- */
    async stageFile(f: FileStatus) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.stageFile(w.repo.root, f);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async unstageFile(f: FileStatus) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.unstageFile(w.repo.root, f);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** IDEA-style shelve: park the files' changes on the stash shelf */
    async shelveFiles(message: string, paths: string[]) {
      const w = this.ws;
      if (!w || !paths.length) return;
      try {
        await api.stashPush(w.repo.root, message, paths);
        toast(`已搁置 ${paths.length} 个文件的更改`);
        // reveal the result: open the Git panel on its 搁置 tab
        useSettingsStore().setGitPanelOpen(true);
        this.shelfRevealSeq++;
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** unshelve: restore a shelf entry to the worktree (drops it on success) */
    async unshelve(index: number) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.stashPop(w.repo.root, index);
        toast("已恢复搁置的更改");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async dropShelf(index: number) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.stashDrop(w.repo.root, index);
        toast("已删除该搁置");
      } catch (e) {
        toast(String(e), "error");
      }
    },
    /** stage several files in one go, refreshing only once at the end */
    async stageFiles(files: FileStatus[]) {
      const w = this.ws;
      if (!w || !files.length) return;
      try {
        for (const f of files) await api.stageFile(w.repo.root, f);
        toast(`已暂存 ${files.length} 个文件`);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** unstage several files in one go, refreshing only once at the end */
    async unstageFiles(files: FileStatus[]) {
      const w = this.ws;
      if (!w || !files.length) return;
      try {
        for (const f of files) await api.unstageFile(w.repo.root, f);
        toast(`已取消暂存 ${files.length} 个文件`);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async stageAll() {
      const w = this.ws;
      if (!w) return;
      try {
        await api.stageAll(w.repo.root);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async unstageAll() {
      const w = this.ws;
      if (!w) return;
      try {
        await api.unstageAll(w.repo.root);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async stageHunk(hunk: Hunk) {
      const w = this.ws;
      if (!w || !w.diff || !w.selectedPath) return;
      const patch = w.diff.fileHeader + hunk.text;
      try {
        await api.stageHunk(w.repo.root, w.selectedPath, patch);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async unstageHunk(hunk: Hunk) {
      const w = this.ws;
      if (!w || !w.diff || !w.selectedPath) return;
      const patch = w.diff.fileHeader + hunk.text;
      try {
        await api.unstageHunk(w.repo.root, w.selectedPath, patch);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** returns whether the commit actually went through, so the message box knows to clear */
    async createCommit(message: string, amend: boolean): Promise<boolean> {
      const w = this.ws;
      if (!w) return false;
      try {
        await api.createCommit(w.repo.root, message, amend);
        toast(amend ? "已修补提交" : "已提交");
      } catch (e) {
        toast(String(e), "error");
        return false;
      } finally {
        await this.refreshWs(w);
      }
      return true;
    },
    /** IDEA-style commit: stage + commit exactly the checkbox-picked paths */
    async commitPaths(message: string, paths: string[]): Promise<boolean> {
      const w = this.ws;
      if (!w) return false;
      try {
        await api.commitPaths(w.repo.root, message, paths);
        toast("已提交");
      } catch (e) {
        toast(String(e), "error");
        return false;
      } finally {
        await this.refreshWs(w);
      }
      return true;
    },
    /** ----- branches ----- */
    async loadBranches(w: Workspace) {
      try {
        w.branches = await api.listBranches(w.repo.root);
      } catch (e) {
        toast(String(e), "error");
      }
    },
    async createBranch(name: string, startPoint: string | null, checkout: boolean) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.createBranch(w.repo.root, name, startPoint, checkout);
        toast(`已创建分支 ${name}`);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async checkoutBranch(name: string, isRemote: boolean) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.checkoutBranch(w.repo.root, name, isRemote);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** tries a plain delete first; on "not fully merged" the caller should
     * confirm and retry with force=true — never force silently */
    async deleteBranch(name: string, force: boolean) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.deleteBranch(w.repo.root, name, force);
        toast(`已删除分支 ${name}`);
      } catch (e) {
        toast(String(e), "error");
        throw e; // let the caller decide whether to retry with force
      } finally {
        await this.refreshWs(w);
      }
    },
    async renameBranch(oldName: string, newName: string) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.renameBranch(w.repo.root, oldName, newName);
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** ----- fetch / pull / push (network — can be slow, so a per-workspace
     *  busyOp + progress toast rather than blocking the whole UI) ----- */
    async fetchRemote(remote: string | null = null, prune = false) {
      const w = this.ws;
      if (!w) return;
      w.busyOp = "fetch";
      const id = toast("正在 fetch…", "progress");
      const unlisten = await listenGitProgress(w.repo.root, id);
      try {
        await api.fetchRemote(w.repo.root, remote, prune);
        updateToast(id, "fetch 完成", "ok");
      } catch (e) {
        updateToast(id, String(e), "error");
      } finally {
        unlisten();
        w.busyOp = null;
        await this.refreshWs(w);
      }
    },
    async pullBranch(remote: string | null = null, rebase?: boolean) {
      const w = this.ws;
      if (!w) return;
      const useRebase = rebase ?? (useSettingsStore().pullStrategy === "rebase");
      w.busyOp = "pull";
      const id = toast("正在 pull…", "progress");
      const unlisten = await listenGitProgress(w.repo.root, id);
      try {
        const outcome = await api.pullBranch(w.repo.root, remote, useRebase);
        updateToast(id, outcome === "applied" ? "pull 完成" : "pull 产生冲突，需要解决", outcome === "applied" ? "ok" : "error");
      } catch (e) {
        updateToast(id, String(e), "error");
      } finally {
        unlisten();
        w.busyOp = null;
        await this.refreshWs(w);
      }
    },
    /** returns whether the push actually succeeded, so the caller can offer a force retry */
    async pushBranch(remote: string, branch: string, setUpstream: boolean, force: ForceMode): Promise<boolean> {
      const w = this.ws;
      if (!w) return false;
      w.busyOp = "push";
      const id = toast("正在 push…", "progress");
      try {
        await api.pushBranch(w.repo.root, remote, branch, setUpstream, force);
        updateToast(id, "push 完成", "ok");
        return true;
      } catch (e) {
        updateToast(id, String(e), "error");
        return false;
      } finally {
        w.busyOp = null;
        await this.refreshWs(w);
      }
    },
    /** ----- history actions (cherry-pick / revert / reset) -----
     * a "conflict" outcome isn't an error — refreshWs picks up the resulting
     * RepoOperation and (from PR6 on) routes the view to the conflict resolver */
    async cherryPickCommit(hash: string) {
      const w = this.ws;
      if (!w) return;
      try {
        const outcome = await api.cherryPickCommit(w.repo.root, hash);
        toast(outcome === "applied" ? "已 cherry-pick" : "cherry-pick 产生冲突，需要解决", outcome === "applied" ? "ok" : "error");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async dropCommit(hash: string) {
      const w = this.ws;
      if (!w) return;
      try {
        const outcome = await api.dropCommit(w.repo.root, hash);
        toast(outcome === "applied" ? "已丢弃该提交" : "丢弃提交产生冲突，需要解决", outcome === "applied" ? "ok" : "error");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async revertCommit(hash: string) {
      const w = this.ws;
      if (!w) return;
      try {
        const outcome = await api.revertCommit(w.repo.root, hash);
        toast(outcome === "applied" ? "已回滚该提交" : "回滚产生冲突，需要解决", outcome === "applied" ? "ok" : "error");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async resetTo(hash: string, mode: ResetMode) {
      const w = this.ws;
      if (!w) return;
      try {
        await api.resetTo(w.repo.root, hash, mode);
        toast("已重置分支");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** ----- merge + conflict resolution ----- */
    async mergeBranch(source: string, noFf: boolean) {
      const w = this.ws;
      if (!w) return;
      try {
        const outcome = await api.mergeBranch(w.repo.root, source, noFf);
        toast(outcome === "applied" ? "合并完成" : "合并产生冲突，需要解决", outcome === "applied" ? "ok" : "error");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async continueOperation() {
      const w = this.ws;
      if (!w) return;
      try {
        const outcome = await api.continueOperation(w.repo.root);
        toast(outcome === "applied" ? "已完成" : "仍有冲突未解决", outcome === "applied" ? "ok" : "error");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    async abortOperation() {
      const w = this.ws;
      if (!w) return;
      try {
        await api.abortOperation(w.repo.root);
        toast("已中止");
      } catch (e) {
        toast(String(e), "error");
      }
      await this.refreshWs(w);
    },
    /** ----- history panel ----- */
    setCommitPageSize(n: number) {
      if (this.ws) this.ws.commitPageSize = n;
    },
    /** scope the 日志 view to one branch/ref (or null to go back to HEAD) */
    async setLogBranchFilter(branch: string | null) {
      const w = this.ws;
      if (!w || w.logBranchFilter === branch) return;
      w.logBranchFilter = branch;
      await this.loadCommits(true);
    },
    /** scope the 日志 view to one author (or null for everyone) */
    async setLogAuthorFilter(author: string | null) {
      const w = this.ws;
      if (!w || w.logAuthorFilter === author) return;
      w.logAuthorFilter = author;
      await this.loadCommits(true);
    },
    /** search the full history (not just what's been paged in) by
     * subject/author/email/hash; null/empty goes back to normal browsing */
    async setLogSearchQuery(query: string | null) {
      const w = this.ws;
      const q = query?.trim() || null;
      if (!w || w.logSearchQuery === q) return;
      w.logSearchQuery = q;
      await this.loadCommits(true);
    },
    async loadCommits(reset = false) {
      const w = this.ws;
      if (!w || w.loadingCommits) return;
      if (reset) {
        w.commits = [];
        w.commitsExhausted = false;
        w.commitFiles = {};
        w.logActiveCommit = null;
      }
      if (w.commitsExhausted) return;
      w.loadingCommits = true;
      try {
        const parsed = parseLogQuery(w.logSearchQuery);
        if (w.logSearchQuery && hasLogQuery(parsed)) {
          // a bounded, non-paginated set of matches — "load more" doesn't
          // apply while a search is active. author:/subject:/hash: qualifiers
          // are parsed out of the query and matched field-specifically.
          w.commits = await api.searchCommits(
            w.repo.root,
            w.logBranchFilter,
            parsed.text,
            200,
            parsed.author ?? w.logAuthorFilter,
            parsed.subject,
            parsed.hash,
          );
          w.commitsExhausted = true;
        } else {
          const count = w.commitPageSize;
          const batch = await api.logCommits(w.repo.root, w.commits.length, count, w.logBranchFilter, w.logAuthorFilter);
          w.commits.push(...batch);
          if (batch.length < count) w.commitsExhausted = true;
        }
      } catch (e) {
        toast(String(e), "error");
      } finally {
        w.loadingCommits = false;
      }
    },
    /** highlight a commit in the 日志 table and load its changed files into
     * the right-hand panel (IDEA-style left/right split, not inline expand) */
    async selectLogCommit(hash: string) {
      const w = this.ws;
      if (!w) return;
      w.logActiveCommit = hash;
      if (!w.commitFiles[hash]) {
        try {
          w.commitFiles[hash] = await api.commitFiles(w.repo.root, hash);
        } catch (e) {
          toast(String(e), "error");
        }
      }
    },
    /** jump to a commit in the Git panel's 日志 (blame-annotation click):
     * opens the panel, loads history up to the hash in one go, selects it,
     * and signals GitPanel/HistoryPanel to switch tab + scroll to the row */
    async revealLogCommit(hash: string) {
      const w = this.ws;
      if (!w || revealInFlight) return;
      revealInFlight = true;
      try {
        useSettingsStore().setGitPanelOpen(true);
        // a live search / author filter shows a bounded or scoped result set
        // that likely excludes the hash
        if (w.logSearchQuery) await this.setLogSearchQuery(null);
        if (w.logAuthorFilter) await this.setLogAuthorFilter(null);
        const found = () => w.commits.some((c) => c.hash === hash);
        if (!found() && !w.commitsExhausted) {
          // wait out any in-flight page load — a concurrent skip/count fetch
          // would append duplicate rows
          for (let i = 0; i < 100 && w.loadingCommits; i++) {
            await new Promise((r) => setTimeout(r, 50));
          }
          // one rev-list gives the commit's row index (newest-first), so the
          // gap loads in a single git-log call instead of page-by-page — the
          // paged loop froze the UI on big repos (dozens of sequential git
          // spawns, each append rebuilding the commit graph)
          const target = w.logBranchFilter ?? "HEAD";
          const after = await api.countCommitsBetween(w.repo.root, hash, target);
          // merges can shift git-log order relative to rev-list counting
          const needed = after + 50;
          const requested = needed - w.commits.length;
          if (requested > 0) {
            w.loadingCommits = true;
            try {
              const batch = await api.logCommits(w.repo.root, w.commits.length, requested, w.logBranchFilter, w.logAuthorFilter);
              w.commits.push(...batch);
              if (batch.length < requested) w.commitsExhausted = true;
            } finally {
              w.loadingCommits = false;
            }
          }
        }
        if (!found()) {
          toast("日志中未找到该提交（可能被分支筛选排除）", "error");
          return;
        }
        await this.selectLogCommit(hash);
        this.logRevealSeq++;
      } catch (e) {
        toast(String(e), "error");
      } finally {
        revealInFlight = false;
      }
    },
  },
});

// hot-replace this store's actions on edit instead of keeping the stale instance
if (import.meta.hot) {
  import.meta.hot.accept(acceptHMRUpdate(useRepoStore, import.meta.hot));
}
