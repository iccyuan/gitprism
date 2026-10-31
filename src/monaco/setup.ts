import * as monaco from "monaco-editor";
import EditorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";
import TsWorker from "monaco-editor/esm/vs/language/typescript/ts.worker?worker";
import { registerQml } from "./qml";

// fonts ship with the app (woff2 bundled by vite) — no system fonts required
// every family needs its real 400/700 + italics bundled: themes bold keywords
// and italicize comments, and a missing face gets synthesized by the browser
// with wrong advance widths — Monaco lays out by measured char width, so faux
// bold/italic shows up as misaligned columns and a drifting caret
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/400-italic.css";
import "@fontsource/jetbrains-mono/700.css";
import "@fontsource/jetbrains-mono/700-italic.css";
// Fira Code ships no italic — the synthesized oblique keeps glyph widths, so it's safe
import "@fontsource/fira-code/400.css";
import "@fontsource/fira-code/700.css";
import "@fontsource/cascadia-code/400.css";
import "@fontsource/cascadia-code/400-italic.css";
import "@fontsource/cascadia-code/700.css";
import "@fontsource/cascadia-code/700-italic.css";
import "@fontsource/source-code-pro/400.css";
import "@fontsource/source-code-pro/400-italic.css";
import "@fontsource/source-code-pro/700.css";
import "@fontsource/source-code-pro/700-italic.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/400-italic.css";
import "@fontsource/ibm-plex-mono/700.css";
import "@fontsource/ibm-plex-mono/700-italic.css";

// theme definitions vendored from monaco-themes (MIT) — its package exports
// don't expose ./themes/*.json so they can't be imported from the package
import githubLight from "./themes/github-light.json";
import githubDark from "./themes/github-dark.json";
import monokai from "./themes/monokai.json";
import solarizedLight from "./themes/solarized-light.json";
import solarizedDark from "./themes/solarized-dark.json";
import dracula from "./themes/dracula.json";
import nord from "./themes/nord.json";
import tomorrow from "./themes/tomorrow.json";
import xcode from "./themes/xcode.json";
import chromeDevtools from "./themes/chrome-devtools.json";
import dawn from "./themes/dawn.json";
import clouds from "./themes/clouds.json";

// without this Monaco freezes: it cannot create its editor worker under Vite.
// the TS worker powers in-file semantic navigation (F12) and type hovers.
self.MonacoEnvironment = {
  getWorker(_id: string, label: string) {
    if (label === "typescript" || label === "javascript") return new TsWorker();
    return new EditorWorker();
  },
};

// this app is a Git tool, not an IDE: keep completion, hovers and semantic
// navigation (F12), but suppress ALL compile/lint diagnostics — a review
// surface must not paint red squiggles over code it only partially loads.
// (root export types languages.typescript as a deprecated stub; the runtime
// API is present once the full package is imported, so reach it via any.)
const ts = (monaco.languages as unknown as { typescript?: any }).typescript;
if (ts) {
  for (const d of [ts.typescriptDefaults, ts.javascriptDefaults]) {
    d.setDiagnosticsOptions({
      noSemanticValidation: true,
      noSyntaxValidation: true,
      noSuggestionDiagnostics: true,
    });
    d.setCompilerOptions({
      target: ts.ScriptTarget.ESNext,
      module: ts.ModuleKind.ESNext,
      moduleResolution: ts.ModuleResolutionKind.NodeJs,
      jsx: ts.JsxEmit.Preserve,
      allowJs: true,
      allowNonTsExtensions: true,
    });
  }
}
// same for the other built-in language services that validate by default
const jsonLang = (monaco.languages as unknown as { json?: any }).json;
jsonLang?.jsonDefaults?.setDiagnosticsOptions({ validate: false });
const cssLang = (monaco.languages as unknown as { css?: any }).css;
for (const d of [cssLang?.cssDefaults, cssLang?.lessDefaults, cssLang?.scssDefaults]) {
  d?.setOptions({ validate: false });
}

registerQml();

const CUSTOM_THEMES: Record<string, unknown> = {
  "github-light": githubLight,
  "github-dark": githubDark,
  monokai,
  "solarized-light": solarizedLight,
  "solarized-dark": solarizedDark,
  dracula,
  nord,
  tomorrow,
  xcode,
  "chrome-devtools": chromeDevtools,
  dawn,
  clouds,
};
for (const [id, data] of Object.entries(CUSTOM_THEMES)) {
  monaco.editor.defineTheme(id, data as monaco.editor.IStandaloneThemeData);
}

export interface FontMeta {
  id: string;
  label: string;
  family: string;
}

// "Microsoft YaHei" in each stack keeps CJK comments readable
export const EDITOR_FONTS: FontMeta[] = [
  {
    id: "jetbrains-mono",
    label: "JetBrains Mono（默认）",
    family: '"JetBrains Mono", "Microsoft YaHei", monospace',
  },
  { id: "fira-code", label: "Fira Code（编程连字）", family: '"Fira Code", "Microsoft YaHei", monospace' },
  { id: "cascadia-code", label: "Cascadia Code", family: '"Cascadia Code", "Microsoft YaHei", monospace' },
  {
    id: "source-code-pro",
    label: "Source Code Pro",
    family: '"Source Code Pro", "Microsoft YaHei", monospace',
  },
  { id: "ibm-plex-mono", label: "IBM Plex Mono", family: '"IBM Plex Mono", "Microsoft YaHei", monospace' },
  { id: "system", label: "跟随系统（Consolas）", family: 'Consolas, "Courier New", "Microsoft YaHei", monospace' },
];

export const DEFAULT_EDITOR_FONT_ID = "jetbrains-mono";

export function fontFamilyFor(id: string): string {
  return (EDITOR_FONTS.find((f) => f.id === id) ?? EDITOR_FONTS[0]).family;
}

// Monaco measures glyph widths at editor creation; remeasure once the
// bundled webfonts finish loading or text renders with wrong metrics
document.fonts?.ready.then(() => monaco.editor.remeasureFonts());

export interface ThemeMeta {
  id: string;
  label: string;
  kind: "light" | "dark";
}

export const THEMES: ThemeMeta[] = [
  { id: "vs", label: "VS Light", kind: "light" },
  { id: "github-light", label: "GitHub Light", kind: "light" },
  { id: "xcode", label: "Xcode", kind: "light" },
  { id: "tomorrow", label: "Tomorrow", kind: "light" },
  { id: "chrome-devtools", label: "Chrome DevTools", kind: "light" },
  { id: "solarized-light", label: "Solarized Light", kind: "light" },
  { id: "dawn", label: "Dawn", kind: "light" },
  { id: "clouds", label: "Clouds", kind: "light" },
  { id: "vs-dark", label: "VS Dark", kind: "dark" },
  { id: "github-dark", label: "GitHub Dark", kind: "dark" },
  { id: "monokai", label: "Monokai", kind: "dark" },
  { id: "solarized-dark", label: "Solarized Dark", kind: "dark" },
  { id: "dracula", label: "Dracula", kind: "dark" },
  { id: "nord", label: "Nord", kind: "dark" },
  { id: "hc-black", label: "High Contrast", kind: "dark" },
];

/** Sets the Monaco theme and flips the app chrome (CSS variables) to match. */
export function applyTheme(id: string) {
  monaco.editor.setTheme(id);
  const kind = THEMES.find((t) => t.id === id)?.kind ?? "dark";
  document.documentElement.dataset.theme = kind;
}

/** Monaco ships no groovy/toml/etc — map common build & config formats to the
 *  closest tokenizer so gradle/maven/npm/cargo projects all highlight. */
const NAME_LANG: Record<string, string> = {
  dockerfile: "dockerfile",
  makefile: "shell",
  gnumakefile: "shell",
  "cmakelists.txt": "shell",
  "pom.xml": "xml",
  ".gitignore": "ini",
  ".gitattributes": "ini",
  ".gitmodules": "ini",
  ".dockerignore": "ini",
  ".npmrc": "ini",
  ".editorconfig": "ini",
  ".env": "ini",
};

const EXT_LANG: Record<string, string> = {
  ".vue": "html",
  ".svelte": "html",
  ".gradle": "java", // groovy DSL: java tokens are the closest fit
  ".groovy": "java",
  ".aidl": "java", // Android IDL: Java-style interfaces/parcelables
  ".hidl": "java",
  ".kts": "kotlin",
  // Qt (QML has its own grammar in ./qml.ts): qmake files are shell-like key=value;
  // .ui/.qrc are XML; .qss is CSS. (.ts is left to TypeScript, not Qt Linguist.)
  ".pro": "shell",
  ".pri": "shell",
  ".prf": "shell",
  ".ui": "xml",
  ".qrc": "xml",
  ".qss": "css",
  ".cmake": "shell",
  ".ipp": "cpp",
  ".inl": "cpp",
  ".toml": "ini",
  ".lock": "ini",
  ".properties": "ini",
  ".conf": "ini",
  ".cfg": "ini",
  ".zsh": "shell",
  ".jsonc": "json",
  ".json5": "json",
  ".mdx": "markdown",
};

/** File path -> Monaco language id, manual overrides first, then Monaco's registry. */
export function languageForPath(path: string): string {
  const name = path.split("/").pop()!.toLowerCase();
  if (NAME_LANG[name]) return NAME_LANG[name];
  if (name.startsWith("dockerfile.")) return "dockerfile";
  if (name.startsWith(".env.")) return "ini";
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return "plaintext";
  const ext = name.slice(dot);
  if (EXT_LANG[ext]) return EXT_LANG[ext];
  for (const lang of monaco.languages.getLanguages()) {
    if (lang.extensions?.some((e) => e.toLowerCase() === ext)) return lang.id;
  }
  return "plaintext";
}

export { monaco };
