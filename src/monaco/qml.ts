import * as monaco from "monaco-editor";

// Monaco ships no QML grammar. QML is JavaScript plus declarative object
// blocks: `import QtQuick 2.15`, `Rectangle { id: r; width: 10 }`,
// `property int count: 0`, `signal clicked()`, `on<Signal>:` handlers.
// This Monarch grammar layers those on top of JS-style tokens.
export function registerQml() {
  if (monaco.languages.getLanguages().some((l) => l.id === "qml")) return;
  monaco.languages.register({ id: "qml", extensions: [".qml"], aliases: ["QML"] });

  monaco.languages.setLanguageConfiguration("qml", {
    comments: { lineComment: "//", blockComment: ["/*", "*/"] },
    brackets: [
      ["{", "}"],
      ["[", "]"],
      ["(", ")"],
    ],
    autoClosingPairs: [
      { open: "{", close: "}" },
      { open: "[", close: "]" },
      { open: "(", close: ")" },
      { open: '"', close: '"', notIn: ["string"] },
      { open: "'", close: "'", notIn: ["string"] },
      { open: "`", close: "`", notIn: ["string"] },
    ],
    surroundingPairs: [
      { open: "{", close: "}" },
      { open: "[", close: "]" },
      { open: "(", close: ")" },
      { open: '"', close: '"' },
      { open: "'", close: "'" },
    ],
    folding: { markers: { start: /^\s*\/\/\s*#?region\b/, end: /^\s*\/\/\s*#?endregion\b/ } },
  });

  monaco.languages.setMonarchTokensProvider("qml", {
    defaultToken: "",
    keywords: [
      "import", "as", "property", "readonly", "default", "required", "alias", "signal",
      "component", "pragma", "enum", "function", "var", "let", "const", "if", "else",
      "for", "while", "do", "switch", "case", "break", "continue", "return", "new",
      "delete", "typeof", "instanceof", "in", "of", "try", "catch", "finally", "throw",
      "this", "parent", "true", "false", "null", "undefined",
    ],
    types: [
      "int", "real", "double", "bool", "string", "url", "color", "date", "point", "rect",
      "size", "list", "variant", "font", "matrix4x4", "quaternion", "vector2d",
      "vector3d", "vector4d",
    ],
    operators: [
      "=", ">", "<", "!", "~", "?", ":", "==", "<=", ">=", "!=", "===", "!==", "&&",
      "||", "??", "++", "--", "+", "-", "*", "/", "&", "|", "^", "%", "<<", ">>", ">>>",
      "+=", "-=", "*=", "/=", "&=", "|=", "^=", "%=", "=>",
    ],
    symbols: /[=><!~?:&|+\-*\/\^%]+/,
    escapes: /\\(?:[abfnrtv\\"'`]|x[0-9A-Fa-f]{2}|u[0-9A-Fa-f]{4}|u\{[0-9A-Fa-f]+\})/,

    tokenizer: {
      root: [
        // `import QtQuick.Controls 2.15 as QQC`
        [/^(\s*)(import)(\s+)([A-Za-z_][\w.]*)/, ["", "keyword", "", "namespace"]],
        [/^(\s*)(pragma)(\s+)(\w+)/, ["", "keyword", "", "annotation"]],
        // `id: root` — the object's id is worth calling out
        [/\b(id)(\s*)(:)(\s*)([A-Za-z_]\w*)/, ["keyword", "", "delimiter", "", "variable.predefined"]],
        // signal handlers: `onClicked:`, `Component.onCompleted:`
        [/\b(on[A-Z]\w*)(\s*)(?=:)/, ["attribute.name", ""]],
        // object type before a block: `Rectangle {`, `QQC.Button {`
        [/\b([A-Z]\w*(?:\.[A-Z]\w*)*)(\s*)(?=\{)/, ["type.identifier", ""]],
        // property name before a colon: `width:`, `anchors.fill:`
        [/\b([a-z_]\w*(?:\.[a-z_]\w*)*)(\s*)(?=:(?!:))/, ["attribute.name", ""]],
        [
          /[a-zA-Z_$][\w$]*/,
          {
            cases: {
              "@keywords": "keyword",
              "@types": "type",
              "[A-Z][\\w$]*": "type.identifier",
              "@default": "identifier",
            },
          },
        ],
        { include: "@whitespace" },
        [/[{}()\[\]]/, "@brackets"],
        [/[;,.]/, "delimiter"],
        [/@symbols/, { cases: { "@operators": "operator", "@default": "" } }],
        [/0[xX][0-9a-fA-F]+/, "number.hex"],
        [/\d*\.\d+([eE][\-+]?\d+)?/, "number.float"],
        [/\d+([eE][\-+]?\d+)?/, "number"],
        [/"/, "string", "@string_double"],
        [/'/, "string", "@string_single"],
        [/`/, "string", "@string_backtick"],
      ],
      whitespace: [
        [/[ \t\r\n]+/, ""],
        [/\/\*/, "comment", "@comment"],
        [/\/\/.*$/, "comment"],
      ],
      comment: [
        [/[^\/*]+/, "comment"],
        [/\*\//, "comment", "@pop"],
        [/[\/*]/, "comment"],
      ],
      string_double: [
        [/[^\\"]+/, "string"],
        [/@escapes/, "string.escape"],
        [/\\./, "string.escape.invalid"],
        [/"/, "string", "@pop"],
      ],
      string_single: [
        [/[^\\']+/, "string"],
        [/@escapes/, "string.escape"],
        [/\\./, "string.escape.invalid"],
        [/'/, "string", "@pop"],
      ],
      string_backtick: [
        [/\$\{/, { token: "delimiter.bracket", next: "@interp" }],
        [/[^\\`$]+/, "string"],
        [/@escapes/, "string.escape"],
        [/\\./, "string.escape.invalid"],
        [/`/, "string", "@pop"],
      ],
      interp: [
        [/\}/, { token: "delimiter.bracket", next: "@pop" }],
        { include: "root" },
      ],
    },
  } as monaco.languages.IMonarchLanguage);
}
