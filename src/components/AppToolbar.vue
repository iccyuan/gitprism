<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useRepoStore } from "../stores/repo";
import { useSettingsStore } from "../stores/settings";
import { cloneDialog } from "../lib/cloneDialog";
import { checkForUpdate, updateState } from "../lib/update";
import { EDITOR_FONTS, THEMES } from "../monaco/setup";

const repo = useRepoStore();
const settings = useSettingsStore();

// IDEA-style merged title bar: native window decorations are off
// (tauri.conf.json), so this header doubles as the draggable title bar and
// draws its own minimize/maximize/close buttons.
const win = getCurrentWindow();
const isMaximized = ref(false);
async function refreshMaximized() {
  isMaximized.value = await win.isMaximized();
}
let unlistenResize: (() => void) | undefined;
onMounted(async () => {
  await refreshMaximized();
  unlistenResize = await win.onResized(() => refreshMaximized());
});
onBeforeUnmount(() => unlistenResize?.());

function minimizeWin() {
  win.minimize();
}
function toggleMaximizeWin() {
  win.toggleMaximize();
}
function closeWin() {
  win.close();
}

// three dropdown menus (文件 / 视图 / 首选项) — 首选项 holds every setting
// inline for quick access, replacing the old settings dialog
type MenuId = "file" | "view" | "prefs";
const openMenuId = ref<MenuId | null>(null);
const fileBtn = ref<HTMLElement | null>(null);
const viewBtn = ref<HTMLElement | null>(null);
const prefsBtn = ref<HTMLElement | null>(null);
const menuEl = ref<HTMLElement | null>(null);
const menuPos = ref({ x: 0, y: 0 });

function toggleMenu(id: MenuId, triggerEl: HTMLElement | null) {
  clearTimeout(hoverTimer);
  submenu.value = null;
  if (openMenuId.value === id) {
    openMenuId.value = null;
    return;
  }
  if (triggerEl) {
    const r = triggerEl.getBoundingClientRect();
    menuPos.value = { x: r.left, y: r.bottom + 4 };
  }
  openMenuId.value = id;
}

// top-level menus open on hover, native-menubar style — with a short delay
// so sliding the pointer across the toolbar doesn't pop menus by accident;
// once one menu is open, moving between triggers switches instantly
let hoverTimer: ReturnType<typeof setTimeout> | undefined;
function openMenuAt(id: MenuId, triggerEl: HTMLElement | null) {
  submenu.value = null;
  if (triggerEl) {
    const r = triggerEl.getBoundingClientRect();
    menuPos.value = { x: r.left, y: r.bottom + 4 };
  }
  openMenuId.value = id;
}
function hoverMenu(id: MenuId, triggerEl: HTMLElement | null) {
  clearTimeout(hoverTimer);
  if (openMenuId.value === id) return;
  if (openMenuId.value !== null) {
    openMenuAt(id, triggerEl);
    return;
  }
  hoverTimer = setTimeout(() => openMenuAt(id, triggerEl), 400);
}
function cancelHoverMenu() {
  clearTimeout(hoverTimer);
}
function closeMenu() {
  openMenuId.value = null;
  submenu.value = null;
}

// side flyouts (最近打开 / 主题 / 字体) open next to their parent item,
// instead of inlining potentially long lists into the dropdown itself
type SubId = "recent" | "theme" | "font";
const submenu = ref<SubId | null>(null);
const submenuEl = ref<HTMLElement | null>(null);
const submenuPos = ref({ x: 0, y: 0 });

// flyouts also open on a short hover delay, cancelled when the pointer
// moves on to a sibling item before it fires
let subTimer: ReturnType<typeof setTimeout> | undefined;
function openSub(id: SubId, e: MouseEvent) {
  clearTimeout(subTimer);
  if (submenu.value === id) return;
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  subTimer = setTimeout(() => {
    submenuPos.value = { x: r.right + 4, y: r.top };
    submenu.value = id;
  }, 300);
}
function toggleSub(id: SubId, e: MouseEvent) {
  clearTimeout(subTimer);
  if (submenu.value === id) {
    submenu.value = null;
    return;
  }
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  submenuPos.value = { x: r.right + 4, y: r.top };
  submenu.value = id;
}
function cancelSub() {
  clearTimeout(subTimer);
  submenu.value = null;
}

function onDocPointer(e: MouseEvent) {
  const t = e.target as Node;
  const inTriggers = fileBtn.value?.contains(t) || viewBtn.value?.contains(t) || prefsBtn.value?.contains(t);
  const inMenu = menuEl.value?.contains(t);
  const inSubmenu = submenuEl.value?.contains(t);
  if (!inTriggers && !inMenu && !inSubmenu) closeMenu();
}
function onEsc(e: KeyboardEvent) {
  if (e.key === "Escape") closeMenu();
}
onMounted(() => {
  window.addEventListener("click", onDocPointer);
  window.addEventListener("keydown", onEsc);
});
onBeforeUnmount(() => {
  window.removeEventListener("click", onDocPointer);
  window.removeEventListener("keydown", onEsc);
});

// split a path (either separator) into its folder name and parent dir
function projName(p: string): string {
  return p.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || p;
}
function projDir(p: string): string {
  const trimmed = p.replace(/[\\/]+$/, "");
  const i = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return i > 0 ? trimmed.slice(0, i) : "";
}

async function pickFolder() {
  closeMenu();
  const dir = await open({ directory: true, title: "选择 git 仓库目录" });
  if (typeof dir === "string") await repo.openRepo(dir);
}
async function pickRecent(path: string) {
  closeMenu();
  await repo.openRepo(path);
}
async function onClone() {
  closeMenu();
  await cloneDialog();
}
function onSummary() {
  closeMenu();
  repo.clearActiveView();
}
function onRefresh() {
  closeMenu();
  repo.refresh();
}

/* ----- 首选项 menu: every setting inline ----- */
const themeLabel = computed(() => THEMES.find((t) => t.id === settings.monacoTheme)?.label ?? settings.monacoTheme);
const fontLabel = computed(() => EDITOR_FONTS.find((f) => f.id === settings.editorFont)?.label ?? settings.editorFont);
const lightThemes = THEMES.filter((t) => t.kind === "light");
const darkThemes = THEMES.filter((t) => t.kind === "dark");

function pickTheme(id: string) {
  settings.setTheme(id);
  closeMenu();
}
function pickFont(id: string) {
  settings.setEditorFont(id);
  closeMenu();
}
function bumpFontSize(d: number) {
  settings.setEditorFontSize(Math.min(24, Math.max(10, settings.editorFontSize + d)));
}
function bumpLineHeight(d: number) {
  settings.setEditorLineHeight(Math.round(Math.min(1.8, Math.max(1.2, settings.editorLineHeight + d)) * 100) / 100);
}
// displayed value is transparency (100 - opacity); higher = more see-through
function bumpGlassTransparency(d: number) {
  settings.setGlassOpacity(Math.min(75, Math.max(25, settings.glassOpacity - d)));
}
function onUpdateCheck() {
  closeMenu();
  checkForUpdate(true);
}
</script>

<template>
  <header class="toolbar" data-tauri-drag-region>
    <nav class="menu-bar">
      <button
        ref="fileBtn"
        class="menu-trigger"
        :class="{ active: openMenuId === 'file' }"
        @mouseenter="hoverMenu('file', fileBtn)"
        @mouseleave="cancelHoverMenu"
        @click="toggleMenu('file', fileBtn)"
      >
        文件
      </button>
      <button
        ref="viewBtn"
        class="menu-trigger"
        :class="{ active: openMenuId === 'view' }"
        @mouseenter="hoverMenu('view', viewBtn)"
        @mouseleave="cancelHoverMenu"
        @click="toggleMenu('view', viewBtn)"
      >
        视图
      </button>
      <button
        ref="prefsBtn"
        class="menu-trigger"
        :class="{ active: openMenuId === 'prefs' }"
        @mouseenter="hoverMenu('prefs', prefsBtn)"
        @mouseleave="cancelHoverMenu"
        @click="toggleMenu('prefs', prefsBtn)"
      >
        首选项
      </button>
    </nav>

    <div class="spacer" data-tauri-drag-region></div>

    <div class="win-controls">
      <button class="win-btn" title="最小化" @click="minimizeWin">
        <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M0 5h10" stroke="currentColor" stroke-width="1" /></svg>
      </button>
      <button class="win-btn" :title="isMaximized ? '还原' : '最大化'" @click="toggleMaximizeWin">
        <svg v-if="!isMaximized" viewBox="0 0 10 10" aria-hidden="true">
          <rect x="0.5" y="0.5" width="9" height="9" fill="none" stroke="currentColor" stroke-width="1" />
        </svg>
        <svg v-else viewBox="0 0 10 10" aria-hidden="true">
          <rect x="0.5" y="2.5" width="7" height="7" fill="none" stroke="currentColor" stroke-width="1" />
          <path d="M2.5 2.5V0.5h7v7h-2" fill="none" stroke="currentColor" stroke-width="1" />
        </svg>
      </button>
      <button class="win-btn win-close" title="关闭" @click="closeWin">
        <svg viewBox="0 0 10 10" aria-hidden="true">
          <path d="M0 0l10 10M10 0L0 10" stroke="currentColor" stroke-width="1" />
        </svg>
      </button>
    </div>

    <Teleport to="body">
      <div v-if="openMenuId === 'file'" ref="menuEl" class="recent-menu" :style="{ left: menuPos.x + 'px', top: menuPos.y + 'px' }">
        <!-- hovering a sibling item closes the flyout, like a native menu -->
        <button class="recent-item" @click="pickFolder" @mouseenter="cancelSub">
          <span class="recent-icon">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <path d="M2 4.5c0-.55.45-1 1-1h2.8l1.4 1.6H13c.55 0 1 .45 1 1V11c0 .83-.67 1.5-1.5 1.5h-9A1.5 1.5 0 0 1 2 11z" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" />
            </svg>
          </span>
          <span class="recent-text"><span class="recent-name">打开项目</span></span>
        </button>
        <button class="recent-item" @click="onClone" @mouseenter="cancelSub">
          <span class="recent-icon">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <path d="M8 2.5v7M5 6.5l3 3 3-3" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
              <path d="M3.5 11v1.5h9V11" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
          </span>
          <span class="recent-text"><span class="recent-name">克隆仓库</span></span>
        </button>
        <div class="ctx-sep"></div>
        <button
          class="recent-item"
          :class="{ active: submenu === 'recent' }"
          @mouseenter="openSub('recent', $event)"
          @click.stop="toggleSub('recent', $event)"
        >
          <span class="recent-icon">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <circle cx="8" cy="8" r="5.5" fill="none" stroke="currentColor" stroke-width="1.4" />
              <path d="M8 5.2V8l2 1.4" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
          </span>
          <span class="recent-text"><span class="recent-name">最近打开</span></span>
          <span class="submenu-caret">›</span>
        </button>
        <div class="ctx-sep"></div>
        <button class="recent-item" title="编辑文件后约 1 秒自动保存" @mouseenter="cancelSub" @click.stop="settings.toggleAutoSave()">
          <span class="menu-check">{{ settings.autoSave ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">自动保存</span></span>
        </button>
      </div>

      <div v-if="openMenuId === 'view'" ref="menuEl" class="recent-menu" :style="{ left: menuPos.x + 'px', top: menuPos.y + 'px' }">
        <button class="recent-item" :disabled="!repo.repo" @click="onSummary">
          <span class="recent-icon">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <path d="M3.5 13V8.5M8 13V4M12.5 13V6.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" />
            </svg>
          </span>
          <span class="recent-text"><span class="recent-name">摘要</span></span>
        </button>
        <button class="recent-item" :disabled="!repo.repo" @click="onRefresh">
          <span class="recent-icon">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <path d="M13 8a5 5 0 1 1-1.47-3.54" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" />
              <path d="M13.2 2.6v3h-3" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
          </span>
          <span class="recent-text"><span class="recent-name">刷新</span></span>
        </button>
      </div>

      <div v-if="openMenuId === 'prefs'" ref="menuEl" class="recent-menu prefs-menu" :style="{ left: menuPos.x + 'px', top: menuPos.y + 'px' }">
        <button
          class="recent-item"
          :class="{ active: submenu === 'theme' }"
          @mouseenter="openSub('theme', $event)"
          @click.stop="toggleSub('theme', $event)"
        >
          <span class="recent-icon">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <circle cx="8" cy="8" r="5.5" fill="none" stroke="currentColor" stroke-width="1.4" />
              <path d="M8 2.5a5.5 5.5 0 0 1 0 11z" fill="currentColor" />
            </svg>
          </span>
          <span class="recent-text">
            <span class="recent-name">代码主题</span>
            <span class="recent-dir">{{ themeLabel }}</span>
          </span>
          <span class="submenu-caret">›</span>
        </button>
        <button
          class="recent-item"
          :class="{ active: submenu === 'font' }"
          @mouseenter="openSub('font', $event)"
          @click.stop="toggleSub('font', $event)"
        >
          <span class="recent-icon">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <path d="M3.5 13 8 3.5 12.5 13M5.1 9.8h5.8" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
          </span>
          <span class="recent-text">
            <span class="recent-name">代码字体</span>
            <span class="recent-dir">{{ fontLabel }}</span>
          </span>
          <span class="submenu-caret">›</span>
        </button>
        <div class="ctx-sep"></div>
        <div class="menu-stepper" @mouseenter="cancelSub">
          <span class="stepper-label">代码字号</span>
          <button :disabled="settings.editorFontSize <= 10" @click.stop="bumpFontSize(-1)">−</button>
          <span class="stepper-val">{{ settings.editorFontSize }}px</span>
          <button :disabled="settings.editorFontSize >= 24" @click.stop="bumpFontSize(1)">＋</button>
        </div>
        <div class="menu-stepper" @mouseenter="cancelSub">
          <span class="stepper-label">代码行高</span>
          <button :disabled="settings.editorLineHeight <= 1.2" @click.stop="bumpLineHeight(-0.05)">−</button>
          <span class="stepper-val">{{ settings.editorLineHeight.toFixed(2) }}</span>
          <button :disabled="settings.editorLineHeight >= 1.8" @click.stop="bumpLineHeight(0.05)">＋</button>
        </div>
        <div class="ctx-sep"></div>
        <button class="recent-item" @mouseenter="cancelSub" @click.stop="settings.setSideBySide(!settings.renderSideBySide)">
          <span class="menu-check">{{ settings.renderSideBySide ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">并排显示 diff</span></span>
        </button>
        <div class="ctx-sep"></div>
        <div class="menu-group-label">被忽略的文件（.gitignore）</div>
        <button class="recent-item" @mouseenter="cancelSub" @click.stop="repo.setIgnoredMode('hide')">
          <span class="menu-check">{{ settings.ignoredMode === "hide" ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">不显示</span></span>
        </button>
        <button class="recent-item" @mouseenter="cancelSub" @click.stop="repo.setIgnoredMode('files')">
          <span class="menu-check">{{ settings.ignoredMode === "files" ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">只显示文件（如 local.properties，灰色）</span></span>
        </button>
        <button class="recent-item" @mouseenter="cancelSub" @click.stop="repo.setIgnoredMode('all')">
          <span class="menu-check">{{ settings.ignoredMode === "all" ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">全部显示（含 node_modules、build 等目录）</span></span>
        </button>
        <div class="ctx-sep"></div>
        <button class="recent-item" @mouseenter="cancelSub" @click.stop="settings.setGlassEffect(!settings.glassEffect)">
          <span class="menu-check">{{ settings.glassEffect ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">毛玻璃效果</span></span>
        </button>
        <div class="menu-stepper" :class="{ disabled: !settings.glassEffect }" @mouseenter="cancelSub">
          <span class="stepper-label">透明度</span>
          <button :disabled="!settings.glassEffect || 100 - settings.glassOpacity <= 25" @click.stop="bumpGlassTransparency(-5)">−</button>
          <span class="stepper-val">{{ 100 - settings.glassOpacity }}%</span>
          <button :disabled="!settings.glassEffect || 100 - settings.glassOpacity >= 75" @click.stop="bumpGlassTransparency(5)">＋</button>
        </div>
        <div class="ctx-sep"></div>
        <div class="menu-group-label">Pull 默认策略</div>
        <button class="recent-item" @mouseenter="cancelSub" @click.stop="settings.setPullStrategy('merge')">
          <span class="menu-check">{{ settings.pullStrategy === "merge" ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">Merge（创建合并提交）</span></span>
        </button>
        <button class="recent-item" @mouseenter="cancelSub" @click.stop="settings.setPullStrategy('rebase')">
          <span class="menu-check">{{ settings.pullStrategy === "rebase" ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">Rebase（历史保持线性）</span></span>
        </button>
        <div class="ctx-sep"></div>
        <button class="recent-item" :disabled="updateState.busy" @mouseenter="cancelSub" @click="onUpdateCheck">
          <span class="recent-icon">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <circle cx="8" cy="8" r="5.5" fill="none" stroke="currentColor" stroke-width="1.4" />
              <path d="M8 10.8V5.6M5.8 7.6 8 5.4l2.2 2.2" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
          </span>
          <span class="recent-text"><span class="recent-name">{{ updateState.busy ? "检查中…" : "检查更新" }}</span></span>
        </button>
      </div>

      <div
        v-if="submenu === 'recent'"
        ref="submenuEl"
        class="recent-menu"
        :style="{ left: submenuPos.x + 'px', top: submenuPos.y + 'px' }"
      >
        <div class="recent-head">最近打开</div>
        <button v-for="p in settings.recentRepos" :key="p" class="recent-item" :title="p" @click="pickRecent(p)">
          <span class="recent-icon">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <path d="M2 4.5c0-.55.45-1 1-1h2.8l1.4 1.6H13c.55 0 1 .45 1 1V11c0 .83-.67 1.5-1.5 1.5h-9A1.5 1.5 0 0 1 2 11z" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" />
            </svg>
          </span>
          <span class="recent-text">
            <span class="recent-name">{{ projName(p) }}</span>
            <span v-if="projDir(p)" class="recent-dir">{{ projDir(p) }}</span>
          </span>
        </button>
        <div v-if="!settings.recentRepos.length" class="recent-empty">暂无最近打开的项目</div>
      </div>

      <div
        v-if="submenu === 'theme'"
        ref="submenuEl"
        class="recent-menu submenu-scroll"
        :style="{ left: submenuPos.x + 'px', top: submenuPos.y + 'px' }"
      >
        <div class="recent-head">浅色</div>
        <button v-for="t in lightThemes" :key="t.id" class="recent-item" @click="pickTheme(t.id)">
          <span class="menu-check">{{ settings.monacoTheme === t.id ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">{{ t.label }}</span></span>
        </button>
        <div class="recent-head">深色</div>
        <button v-for="t in darkThemes" :key="t.id" class="recent-item" @click="pickTheme(t.id)">
          <span class="menu-check">{{ settings.monacoTheme === t.id ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name">{{ t.label }}</span></span>
        </button>
      </div>

      <div
        v-if="submenu === 'font'"
        ref="submenuEl"
        class="recent-menu"
        :style="{ left: submenuPos.x + 'px', top: submenuPos.y + 'px' }"
      >
        <button v-for="f in EDITOR_FONTS" :key="f.id" class="recent-item" @click="pickFont(f.id)">
          <span class="menu-check">{{ settings.editorFont === f.id ? "✓" : "" }}</span>
          <span class="recent-text"><span class="recent-name" :style="{ fontFamily: f.family }">{{ f.label }}</span></span>
        </button>
      </div>
    </Teleport>
  </header>
</template>
