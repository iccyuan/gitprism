import { acceptHMRUpdate, defineStore } from "pinia";
import type { IgnoredMode } from "../lib/api";
import { LazyStore } from "@tauri-apps/plugin-store";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { applyTheme, DEFAULT_EDITOR_FONT_ID, EDITOR_FONTS, fontFamilyFor, THEMES } from "../monaco/setup";

const persist = new LazyStore("settings.json");

/** flip the frosted-glass styling (CSS) and the native acrylic material on/off */
function applyGlass(on: boolean) {
  document.documentElement.dataset.glass = on ? "on" : "off";
  getCurrentWindow()
    .setEffects({ effects: on ? ["acrylic"] : [] } as never)
    .catch(() => {});
  // a transparent webview renders all text with grayscale antialiasing —
  // ClearType needs an opaque backdrop. With glass off nothing shows through
  // anyway, so an opaque webview background buys back subpixel-crisp text
  const bg = getComputedStyle(document.documentElement).getPropertyValue("--bg").trim();
  const m = /^#([0-9a-f]{6})$/i.exec(bg);
  const rgb: [number, number, number] = m
    ? [parseInt(m[1].slice(0, 2), 16), parseInt(m[1].slice(2, 4), 16), parseInt(m[1].slice(4, 6), 16)]
    : [35, 35, 38];
  getCurrentWebview()
    .setBackgroundColor(on ? [0, 0, 0, 0] : [rgb[0], rgb[1], rgb[2], 255])
    .catch(() => {});
}

/** base glass opacity (percent 25–75) → the --glass-op CSS knob */
function applyGlassOpacity(percent: number) {
  document.documentElement.style.setProperty("--glass-op", String(percent / 100));
}

export const useSettingsStore = defineStore("settings", {
  state: () => ({
    monacoTheme: "vs-dark",
    renderSideBySide: true,
    recentRepos: [] as string[],
    editorFont: DEFAULT_EDITOR_FONT_ID,
    editorFontSize: 13,
    /** line height as a ratio of font size — VS Code uses ~1.35 on Windows */
    editorLineHeight: 1.5,
    sidebarWidth: 300,
    activeLeftPanel: "project" as "project" | "commit",
    /** clicking the active ActivityBar icon again collapses the Project/Commit
     * panel instead of switching — IDEA does the same with its tool windows */
    sidebarCollapsed: false,
    /** bottom-docked Git tool window (日志/分支/控制台) — separate from the
     * left Project/Commit switch since it needs the window's full width */
    gitPanelHeight: 320,
    gitPanelOpen: false,
    commitMessageHeight: 110,
    /** Git log table's Author/Date columns — Subject always flexes to fill
     * whatever's left, like IDEA's resizable log table columns */
    logAuthorColWidth: 32,
    logDateColWidth: 130,
    /** extra, opt-in git log columns toggled from the log header's right-click menu */
    logShowHashCol: false,
    logShowEmailCol: false,
    logShowChangesCol: false,
    /** default strategy for the toolbar's Pull button — "支持选择 rebase 和 merge" */
    pullStrategy: "merge" as "merge" | "rebase",
    glassEffect: true,
    glassOpacity: 54,
    /** 全部文件 view: how much of what .gitignore excludes is listed */
    ignoredMode: "files" as IgnoredMode,
    /** 全部文件 editor: save automatically shortly after the last edit */
    autoSave: false,
  }),
  getters: {
    editorFontFamily(state): string {
      return fontFamilyFor(state.editorFont);
    },
    editorLineHeightPx(state): number {
      return Math.round(state.editorFontSize * state.editorLineHeight);
    },
  },
  actions: {
    async load() {
      try {
        const theme = await persist.get<string>("monacoTheme");
        const side = await persist.get<boolean>("renderSideBySide");
        const recent = await persist.get<string[]>("recentRepos");
        const font = await persist.get<string>("editorFont");
        const size = await persist.get<number>("editorFontSize");
        const lineH = await persist.get<number>("editorLineHeight");
        const width = await persist.get<number>("sidebarWidth");
        const panel = await persist.get<string>("activeLeftPanel");
        const collapsed = await persist.get<boolean>("sidebarCollapsed");
        const gitH = await persist.get<number>("gitPanelHeight");
        const commitMsgH = await persist.get<number>("commitMessageHeight");
        // v2 key: the pre-narrowing default (110px) was persisted for anyone
        // who ever dragged the column — abandon the old key so the new
        // compact default actually lands, drags after this still stick
        const authorColW = await persist.get<number>("logAuthorColWidth2");
        const dateColW = await persist.get<number>("logDateColWidth");
        const showHashCol = await persist.get<boolean>("logShowHashCol");
        const showEmailCol = await persist.get<boolean>("logShowEmailCol");
        const showChangesCol = await persist.get<boolean>("logShowChangesCol");
        const pullStrategy = await persist.get<string>("pullStrategy");
        const glass = await persist.get<boolean>("glassEffect");
        const glassOp = await persist.get<number>("glassOpacity");
        const ignoredMode = await persist.get<string>("ignoredMode");
        // pre-0.3.5 boolean "显示隐藏的文件和文件夹"
        const legacyHidden = await persist.get<boolean>("showHidden");
        const autoSave = await persist.get<boolean>("autoSave");
        if (theme && THEMES.some((t) => t.id === theme)) this.monacoTheme = theme;
        if (typeof side === "boolean") this.renderSideBySide = side;
        if (Array.isArray(recent)) this.recentRepos = recent;
        if (font && EDITOR_FONTS.some((f) => f.id === font)) this.editorFont = font;
        if (typeof size === "number" && size >= 10 && size <= 24) this.editorFontSize = size;
        if (typeof lineH === "number" && lineH >= 1.2 && lineH <= 1.8) this.editorLineHeight = lineH;
        if (typeof width === "number" && width >= 200 && width <= 600) this.sidebarWidth = width;
        if (panel === "project" || panel === "commit") this.activeLeftPanel = panel;
        if (typeof collapsed === "boolean") this.sidebarCollapsed = collapsed;
        if (typeof gitH === "number" && gitH >= 140 && gitH <= 800) this.gitPanelHeight = gitH;
        if (typeof commitMsgH === "number" && commitMsgH >= 50 && commitMsgH <= 600) this.commitMessageHeight = commitMsgH;
        if (typeof authorColW === "number" && authorColW >= 24 && authorColW <= 260) this.logAuthorColWidth = authorColW;
        if (typeof dateColW === "number" && dateColW >= 80 && dateColW <= 220) this.logDateColWidth = dateColW;
        if (typeof showHashCol === "boolean") this.logShowHashCol = showHashCol;
        if (typeof showEmailCol === "boolean") this.logShowEmailCol = showEmailCol;
        if (typeof showChangesCol === "boolean") this.logShowChangesCol = showChangesCol;
        if (pullStrategy === "merge" || pullStrategy === "rebase") this.pullStrategy = pullStrategy;
        if (typeof autoSave === "boolean") this.autoSave = autoSave;
        if (typeof glass === "boolean") this.glassEffect = glass;
        if (typeof glassOp === "number" && glassOp >= 25 && glassOp <= 75) this.glassOpacity = glassOp;
        if (ignoredMode === "hide" || ignoredMode === "files" || ignoredMode === "all") this.ignoredMode = ignoredMode;
        else if (legacyHidden === true) this.ignoredMode = "all";
      } catch {
        // first launch / unreadable store: keep defaults
      }
      applyTheme(this.monacoTheme);
      applyGlass(this.glassEffect);
      applyGlassOpacity(this.glassOpacity);
    },
    async setGlassEffect(v: boolean) {
      this.glassEffect = v;
      applyGlass(v);
      await persist.set("glassEffect", v);
      await persist.save();
    },
    async setGlassOpacity(percent: number) {
      this.glassOpacity = Math.min(75, Math.max(25, Math.round(percent) || 54));
      applyGlassOpacity(this.glassOpacity);
      await persist.set("glassOpacity", this.glassOpacity);
      await persist.save();
    },
    async setTheme(id: string) {
      this.monacoTheme = id;
      applyTheme(id);
      // the opaque no-glass webview background tracks the theme's --bg
      applyGlass(this.glassEffect);
      await persist.set("monacoTheme", id);
      await persist.save();
    },
    async setSideBySide(v: boolean) {
      this.renderSideBySide = v;
      await persist.set("renderSideBySide", v);
      await persist.save();
    },
    async addRecent(path: string) {
      this.recentRepos = [path, ...this.recentRepos.filter((p) => p !== path)].slice(0, 10);
      await persist.set("recentRepos", [...this.recentRepos]);
      await persist.save();
    },
    async setEditorFont(id: string) {
      if (!EDITOR_FONTS.some((f) => f.id === id)) return;
      this.editorFont = id;
      await persist.set("editorFont", id);
      await persist.save();
    },
    async setEditorFontSize(size: number) {
      this.editorFontSize = Math.min(24, Math.max(10, Math.round(size) || 13));
      await persist.set("editorFontSize", this.editorFontSize);
      await persist.save();
    },
    async setEditorLineHeight(ratio: number) {
      this.editorLineHeight = Math.min(1.8, Math.max(1.2, Math.round(ratio * 20) / 20 || 1.5));
      await persist.set("editorLineHeight", this.editorLineHeight);
      await persist.save();
    },
    async setIgnoredMode(mode: IgnoredMode) {
      this.ignoredMode = mode;
      await persist.set("ignoredMode", mode);
      await persist.save();
    },
    async saveSidebarWidth() {
      await persist.set("sidebarWidth", this.sidebarWidth);
      await persist.save();
    },
    async setActiveLeftPanel(panel: "project" | "commit") {
      this.activeLeftPanel = panel;
      this.sidebarCollapsed = false;
      await persist.set("activeLeftPanel", panel);
      await persist.set("sidebarCollapsed", false);
      await persist.save();
    },
    /** clicking the already-active ActivityBar icon calls this instead */
    async toggleSidebarCollapsed() {
      this.sidebarCollapsed = !this.sidebarCollapsed;
      await persist.set("sidebarCollapsed", this.sidebarCollapsed);
      await persist.save();
    },
    async saveGitPanelHeight() {
      await persist.set("gitPanelHeight", this.gitPanelHeight);
      await persist.save();
    },
    toggleGitPanel() {
      this.gitPanelOpen = !this.gitPanelOpen;
    },
    setGitPanelOpen(v: boolean) {
      this.gitPanelOpen = v;
    },
    async saveCommitMessageHeight() {
      await persist.set("commitMessageHeight", this.commitMessageHeight);
      await persist.save();
    },
    async saveLogColumnWidths() {
      await persist.set("logAuthorColWidth2", this.logAuthorColWidth);
      await persist.set("logDateColWidth", this.logDateColWidth);
      await persist.save();
    },
    async toggleAutoSave() {
      this.autoSave = !this.autoSave;
      await persist.set("autoSave", this.autoSave);
      await persist.save();
    },
    async toggleLogCol(col: "hash" | "email" | "changes") {
      const key = col === "hash" ? "logShowHashCol" : col === "email" ? "logShowEmailCol" : "logShowChangesCol";
      this[key] = !this[key];
      await persist.set(key, this[key]);
      await persist.save();
    },
    async setPullStrategy(strategy: "merge" | "rebase") {
      this.pullStrategy = strategy;
      await persist.set("pullStrategy", strategy);
      await persist.save();
    },
  },
});

if (import.meta.hot) {
  import.meta.hot.accept(acceptHMRUpdate(useSettingsStore, import.meta.hot));
}
