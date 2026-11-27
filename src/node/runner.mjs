// Node side of fast-anything. The CLI copies this file to <project>/.fast/runner.mjs and runs
//     node runner.mjs <mode> <job.json>
// Modes: profile (analyse a V8 .cpuprofile), verify. Not meant to be run by hand.
import { readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";

const SKIP_DIRS = new Set([".fast", ".git", "node_modules"]);
const IO_GLOBALS = [
  "console", "fs", "process", "fetch", "require", "XMLHttpRequest", "document", "window",
  "localStorage", "sessionStorage", "setTimeout", "setInterval", "setImmediate", "queueMicrotask", "Bun", "Deno",
];
const IO_GLOBAL_RE = new RegExp(`(?<![\\w$])(${IO_GLOBALS.join("|")})(?![\\w$])(\\s*\\??\\.\\s*([\\w$]+))?`, "g");
const DYNAMIC_IMPORT_RE = /(?<![\w$])import\s*\(/g;
const NONDET_RE =
  /(?<![\w$])(?:(Math)\s*\??\.\s*random|(Date)\s*\??\.\s*now|new\s+(Date)|(performance)\s*\??\.\s*now|(crypto)\s*\??\.\s*([\w$]+))(?![\w$])/g;
const IDENT = "[A-Za-z_$][\\w$]*";
// Built-in modules whose functions do I/O (or read the machine and the process) wherever they're reached
// from, and the members that don't: constants and pure helpers like `net.isIP`.
const IO_MODULES = new Set(["fs", "child_process", "net", "http", "https", "http2", "dgram", "dns", "tls", "readline",
  "process", "os", "worker_threads", "cluster", "inspector", "timers", "tty", "repl", "module", "console", "sqlite",
  "wasi", "v8"]);
const IO_PURE = new Set(["fs.constants", "os.EOL", "os.constants", "os.devNull", "net.isIP", "net.isIPv4", "net.isIPv6",
  "http.STATUS_CODES", "http.METHODS", "module.isBuiltin", "module.builtinModules", "module.stripTypeScriptTypes"]);
// crypto's hashes and ciphers give the same answer every time; its random and key generators, and the
// signatures and encryption that pad or salt at random, don't.
const NONDET_CRYPTO =
  /^(random\w*|pseudoRandom\w*|getRandomValues|generateKey\w*|generatePrime\w*|webcrypto|subtle|createECDH|createDiffieHellman\w*|getDiffieHellman|createSign|sign|publicEncrypt)$/;
const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

// ------------------------------------------------------------------ source scanning

const REGEX_KEYWORDS = new Set([
  "return",
  "case",
  "throw",
  "in",
  "typeof",
  "void",
  "delete",
  "new",
  "yield",
  "await",
  "else",
  "do",
]);

const EXPR_BRACE_KEYWORDS = new Set([
  "return",
  "throw",
  "yield",
  "await",
  "typeof",
  "void",
  "delete",
  "in",
  "new",
  "default",
]);

const ID_START = /[\p{ID_Start}$_]/u;
const ID_CONT = /[\p{ID_Continue}$_\u200c\u200d]/u;

function identStartStep(src, i) {
  const code = src.charCodeAt(i);
  if ((code >= 65 && code <= 90) || (code >= 97 && code <= 122) || code === 95 || code === 36) return 1;
  if (code < 128) return 0;
  const cp = src.codePointAt(i);
  return ID_START.test(String.fromCodePoint(cp)) ? (cp > 0xffff ? 2 : 1) : 0;
}

function identContStep(src, i) {
  const code = src.charCodeAt(i);
  if (
    (code >= 65 && code <= 90) ||
    (code >= 97 && code <= 122) ||
    (code >= 48 && code <= 57) ||
    code === 95 ||
    code === 36
  )
    return 1;
  if (code < 128) return 0;
  const cp = src.codePointAt(i);
  return ID_CONT.test(String.fromCodePoint(cp)) ? (cp > 0xffff ? 2 : 1) : 0;
}

function isJsxCloseTag(src, slashIdx) {
  let k = slashIdx + 1;
  while (k < src.length && (src[k] === " " || src[k] === "\t")) k++;
  if (src[k] === ">") return true;
  const first = k < src.length ? identStartStep(src, k) : 0;
  if (!first) return false;
  k += first;
  while (k < src.length) {
    const ch = src[k];
    if (ch === "." || ch === ":" || ch === "-") {
      k++;
      continue;
    }
    const step = identContStep(src, k);
    if (!step) break;
    k += step;
  }
  while (k < src.length && (src[k] === " " || src[k] === "\t")) k++;
  return src[k] === ">";
}

/** The source with string/regex contents and comments blanked out (same length, same lines). */
function codeOnly(src, { jsx = false } = {}) {
  const out = src.split("");
  const blank = (a, b) => {
    for (let k = a; k < b && k < out.length; k++) if (out[k] !== "\n") out[k] = " ";
  };
  let i = 0;
  let regexAllowed = true;
  let blockBraceNext = true;
  let afterDot = false;
  let afterLt = false;
  let sawNewline = true;
  let noRegexBefore = -1;
  const braceStack = [];
  while (i < src.length) {
    const c = src[i];
    if (c === "/" && src[i + 1] === "/") {
      const end = src.indexOf("\n", i);
      blank(i, end < 0 ? src.length : end);
      i = end < 0 ? src.length : end;
    } else if (c === "/" && src[i + 1] === "*") {
      const end = src.indexOf("*/", i + 2);
      const stop = end < 0 ? src.length : end + 2;
      const nl = src.indexOf("\n", i + 2);
      const cr = src.indexOf("\r", i + 2);
      if ((nl >= 0 && nl < stop) || (cr >= 0 && cr < stop)) sawNewline = true;
      blank(i, stop);
      i = stop;
    } else if (c === '"' || c === "'" || c === "`") {
      let j = i + 1;
      while (j < src.length && src[j] !== c) j += src[j] === "\\" ? 2 : 1;
      blank(i + 1, j);
      i = j + 1;
      regexAllowed = false;
      blockBraceNext = false;
      afterDot = false;
      afterLt = false;
      sawNewline = false;
    } else if (
      c === "/" &&
      regexAllowed &&
      i >= noRegexBefore &&
      !(jsx && (src[i + 1] === ">" || (afterLt && isJsxCloseTag(src, i))))
    ) {
      let j = i + 1;
      let inClass = false;
      let closed = false;
      while (j < src.length) {
        const ch = src[j];
        if (ch === "\n" || ch === "\r") break;
        if (ch === "\\") {
          if (j + 1 >= src.length || src[j + 1] === "\n" || src[j + 1] === "\r") break;
          j += 2;
          continue;
        }
        if (ch === "[") inClass = true;
        else if (ch === "]") inClass = false;
        else if (ch === "/" && !inClass) {
          closed = true;
          break;
        }
        j++;
      }
      if (closed) {
        blank(i + 1, j);
        j++;
        while (j < src.length && /[A-Za-z]/.test(src[j])) j++;
        i = j;
        regexAllowed = false;
        blockBraceNext = false;
        afterDot = false;
        afterLt = false;
        sawNewline = false;
      } else {
        // Unterminated on this line: fall back to treating `/` as division and don't rescan the same tail.
        noRegexBefore = j;
        i++;
        regexAllowed = true;
        blockBraceNext = false;
        afterDot = false;
        afterLt = false;
        sawNewline = false;
      }
    } else if (c === " " || c === "\t" || c === "\r" || c === "\n") {
      if (c === "\n" || c === "\r") sawNewline = true;
      i++;
    } else {
      const startStep = identStartStep(src, i);
      if (startStep) {
        let j = i + startStep;
        while (j < src.length) {
          const step = identContStep(src, j);
          if (!step) break;
          j += step;
        }
        const word = src.slice(i, j);
        regexAllowed = !afterDot && REGEX_KEYWORDS.has(word);
        blockBraceNext = afterDot || !EXPR_BRACE_KEYWORDS.has(word);
        afterDot = false;
        afterLt = false;
        sawNewline = false;
        i = j;
      } else if (c >= "0" && c <= "9") {
        let j = i + 1;
        while (j < src.length && /[\w.]/.test(src[j])) j++;
        regexAllowed = false;
        blockBraceNext = false;
        afterDot = false;
        afterLt = false;
        sawNewline = false;
        i = j;
      } else if ((c === "+" && src[i + 1] === "+") || (c === "-" && src[i + 1] === "-")) {
        regexAllowed = false;
        blockBraceNext = false;
        afterDot = false;
        afterLt = false;
        sawNewline = false;
        i += 2;
      } else if (c === "=" && src[i + 1] === ">") {
        regexAllowed = true;
        blockBraceNext = true;
        afterDot = false;
        afterLt = false;
        sawNewline = false;
        i += 2;
      } else if (c === "{") {
        braceStack.push(blockBraceNext);
        regexAllowed = true;
        blockBraceNext = true;
        afterDot = false;
        afterLt = false;
        sawNewline = false;
        i++;
      } else if (c === "}") {
        const isBlock = braceStack.length > 0 ? braceStack.pop() : true;
        regexAllowed = isBlock;
        blockBraceNext = isBlock;
        afterDot = false;
        afterLt = false;
        sawNewline = false;
        i++;
      } else if (c === "!") {
        // TypeScript postfix non-null assertion (`expr!`) keeps `/` as division; prefix `!` and `!=` allow regex.
        regexAllowed = src[i + 1] === "=" || regexAllowed || sawNewline;
        blockBraceNext = false;
        afterDot = false;
        afterLt = false;
        sawNewline = false;
        i++;
      } else if (c === ".") {
        regexAllowed = false;
        blockBraceNext = false;
        afterDot = true;
        afterLt = false;
        sawNewline = false;
        i++;
      } else {
        regexAllowed = c !== ")" && c !== "]";
        blockBraceNext = c === ";" || c === ")";
        afterDot = false;
        afterLt = c === "<";
        sawNewline = false;
        i++;
      }
    }
  }
  return out.join("");
}

function matching(code, open) {
  const close = { "(": ")", "{": "}", "[": "]" }[code[open]];
  let depth = 0;
  for (let i = open; i < code.length; i++) {
    if (code[i] === code[open]) depth++;
    else if (code[i] === close && --depth === 0) return i;
  }
  return -1;
}

function splitTop(s) {
  const parts = [];
  let depth = 0;
  let cur = "";
  for (const c of s) {
    if ("([{<".includes(c)) depth++;
    if (")]}>".includes(c)) depth--;
    if (c === "," && depth === 0) {
      parts.push(cur);
      cur = "";
    } else cur += c;
  }
  if (cur.trim()) parts.push(cur);
  return parts.map((p) => p.trim()).filter(Boolean);
}

function defaultAt(s) {
  let depth = 0;
  for (let i = 0; i < s.length; i++) {
    if ("([{<".includes(s[i])) depth++;
    else if (")]}>".includes(s[i]) && s[i - 1] !== "=") depth--;
    else if (s[i] === "=" && depth === 0 && s[i + 1] !== ">" && s[i + 1] !== "=") return i;
  }
  return -1;
}

function parseParams(text) {
  return splitTop(text).map((p) => {
    let name = p;
    let annotation = null;
    const eq = defaultAt(p);
    if (eq >= 0) name = p.slice(0, eq).trim();
    const rest = name.startsWith("...");
    if (rest) name = name.slice(3);
    const colon = name.indexOf(":");
    if (colon >= 0) {
      annotation = name.slice(colon + 1).trim();
      name = name.slice(0, colon).trim();
    }
    const optional = name.endsWith("?") || eq >= 0;
    name = name.replace(/\?$/, "").trim();
    if (optional && annotation) annotation += " | undefined";
    return { name, annotation, rest, destructured: /^[[{]/.test(name) };
  });
}

function topLevelFunctions(src) {
  const re = new RegExp(
    "^(?:export\\s+)?(?:default\\s+)?(?:async\\s+)?function\\s*\\*?\\s*(" + IDENT + ")" +
      "|^(?:export\\s+)?(?:let|var|const)\\s+(" + IDENT + ")\\s*(?::[^=]+)?=\\s*(?:async\\s*)?(?:function\\b|\\(|" + IDENT + "\\s*=>)",
    "gm",
  );
  return new Set([...src.matchAll(re)].map((m) => m[1] || m[2]));
}

/** What a built-in module's member is: "io", "nondet" or null. `member` is null for the module itself
 *  (destructured, indexed or passed along), which counts as what the module does. */
function builtin(mod, member) {
  const base = mod.replace(/^node:/, "").split("/")[0];
  if (IO_MODULES.has(base)) return member && IO_PURE.has(base + "." + member) ? null : "io";
  if (base === "perf_hooks") return "nondet";
  if (base === "crypto" && (!member || NONDET_CRYPTO.test(member))) return "nondet";
  return null;
}

/** splitTop, without taking an arrow's `>` or a spaced comparison (`a < b`, `x >= 1`) for a bracket. */
const topParts = (s) => splitTop(s.replace(/=>|\s[<>]=?\s|[<>]=/g, (m) => " ".repeat(m.length)));

/** Where a parameter's or declarator's default starts, or -1. */
const defaultOf = (p) => defaultAt(p.replace(/=>|\s[<>]=?\s|[<>]=/g, (m) => " ".repeat(m.length)));

/** The names a parameter or a declarator binds: `x`, `x = 1`, `...x`, `x: T`, `{ a, b: c, d = 1 }`, `[a, [b]]`. */
function bindingNames(p) {
  p = p.trim();
  const eq = defaultOf(p);
  if (eq >= 0) p = p.slice(0, eq).trim();
  if (p.startsWith("...")) p = p.slice(3).trim();
  if (p[0] === "{" || p[0] === "[") {
    const end = matching(p, 0);
    return topParts(p.slice(1, end < 0 ? p.length : end)).flatMap((part) => {
      if (p[0] === "[") return bindingNames(part);
      const d = defaultOf(part);
      const colon = colonAt(d < 0 ? part : part.slice(0, d));
      return bindingNames(colon >= 0 ? part.slice(colon + 1) : part);
    });
  }
  const id = p.match(/^[A-Za-z_$][\w$]*/);
  return id ? [id[0]] : [];
}

/** Where `key: value` splits in an object pattern part: the first `:` outside brackets. */
function colonAt(s) {
  let depth = 0;
  for (let i = 0; i < s.length; i++) {
    if ("([{".includes(s[i])) depth++;
    else if (")]}".includes(s[i])) depth--;
    else if (s[i] === ":" && depth === 0) return i;
  }
  return -1;
}

/** The bracket that opens the one closing at `close`, looking back. */
function openOf(code, close) {
  const open = { ")": "(", "}": "{", "]": "[" }[code[close]];
  let depth = 0;
  for (let i = close; i >= 0; i--) {
    if (code[i] === code[close]) depth++;
    else if (code[i] === open && --depth === 0) return i;
  }
  return -1;
}

/** The last `n` characters of `s` before `at`, without trailing space: enough to see what comes just before. */
const tail = (s, at, n = 80) => s.slice(Math.max(0, at - n), at).trimEnd();

/** The `{` of the innermost block around `at`, or -1. */
function openAround(body, at) {
  let depth = 0;
  for (let i = at - 1; i >= 0; i--) {
    if (body[i] === "}") depth++;
    else if (body[i] === "{" && depth-- === 0) return i;
  }
  return -1;
}

/** [start, end) of a block opening at `open`, or the whole body when there's none. */
function blockAt(body, open) {
  if (open < 0) return [0, body.length];
  const end = matching(body, open);
  return [open, end < 0 ? body.length : end + 1];
}

/** Whether the block opening at `open` is a function's body: after `) {`, `): T {` or `=> {` that isn't
 *  an if/for/while/switch/catch/with header. */
function isFunctionBody(body, open) {
  const before = tail(body, open, 200);
  if (before.endsWith("=>")) return true;
  const m = before.match(/\)\s*(?::[^(){};=]*)?$/);
  if (!m) return false;
  const paren = openOf(body, trimmedEnd(body, open) - m[0].length);
  return paren >= 0 && !/\b(?:if|for|while|switch|catch|with)$/.test(tail(body, paren, 20));
}

/** The index just past the last non-space character before `at`. */
function trimmedEnd(s, at) {
  while (at > 0 && /\s/.test(s[at - 1])) at--;
  return at;
}

/** Where an expression that starts at `from` ends: a `,` `;` `:` or closing bracket outside brackets, or the
 *  end of its line unless the next line goes on with it. A ternary's own `:` doesn't end it. */
function expressionEnd(body, from) {
  let depth = 0;
  let ternary = 0;
  let last = "";
  for (let i = from; i < body.length; i++) {
    const c = body[i];
    if ("([{".includes(c)) depth++;
    else if (")]}".includes(c) && --depth < 0) return i;
    else if (depth === 0 && c === "?" && body[i + 1] !== "." && body[i + 1] !== "?" && body[i - 1] !== "?") ternary++;
    else if (depth === 0 && c === ":" && ternary > 0) ternary--;
    else if ((c === "," || c === ";" || c === ":") && depth === 0) return i;
    else if (c === "\n" && depth === 0 && !/[-+*/%&|^!=<>?:.,(]$/.test(last)) {
      const next = body.slice(i + 1, i + 40).trimStart()[0] || "";
      if (!/[-+*/%&|^=<>?:.,)\]}]/.test(next) || next === "") return i;
    }
    if (!/\s/.test(c)) last = c;
  }
  return body.length;
}

/** Where a let/const/var's declarators that start at `from` end: `;`, a closing bracket, or a line end where
 *  the declaration doesn't go on. */
function declarationEnd(body, from) {
  let depth = 0;
  let last = "";
  for (let i = from; i < body.length; i++) {
    const c = body[i];
    if ("([{".includes(c)) depth++;
    else if (")]}".includes(c) && --depth < 0) return i;
    else if (depth === 0 && c === ";") return i;
    else if (depth === 0 && c === "\n" && !/[-+*/%&|^!=<>?:.,]$/.test(last)) {
      const next = body.slice(i + 1, i + 40).trimStart()[0] || "";
      if (!/[-+*/%&|^=<>?:.,]/.test(next)) return i;
    }
    if (!/\s/.test(c)) last = c;
  }
  return body.length;
}

const DEFINES = /(?:^|[{},;])\s*(?:(?:get|set|async|static|private|public|protected|readonly|override)\s+)*\*?\s*$/;

/** [start, end) ranges of `body` where `name` stands for something of the function's own: inside a nested
 *  function, arrow, method or catch block that takes it as a parameter, inside a for loop that declares it,
 *  and in the block of a let/const (the function for var), function or class that declares it. */
function shadowedIn(body, name) {
  const ranges = [];
  const n = esc(name);
  for (const d of body.matchAll(/\b(let|const|var)\s+/g)) {
    const beforeDecl = tail(body, d.index, 20);
    if (/\bas$/.test(beforeDecl) || isPropAccess(beforeDecl)) continue; // `as const` or `obj.const`
    const start = d.index + d[0].length;
    const end = declarationEnd(body, start);
    if (!topParts(body.slice(start, end)).flatMap(bindingNames).includes(name)) continue;
    const loop = tail(body, d.index, 40).match(/\bfor\s*(?:await\s*)?\($/);
    if (loop) {
      // the loop's own: its header and its body
      const paren = body.lastIndexOf("(", d.index);
      const close = matching(body, paren);
      let from = close + 1;
      while (/\s/.test(body[from] || "")) from++;
      const stop = body[from] === "{" ? matching(body, from) + 1 : expressionEnd(body, from);
      ranges.push([paren, stop <= 0 ? body.length : stop]);
      continue;
    }
    let open = openAround(body, d.index);
    if (d[1] === "var") while (open >= 0 && !isFunctionBody(body, open)) open = openAround(body, open);
    ranges.push(blockAt(body, open));
  }
  for (const d of body.matchAll(new RegExp("\\b(?:function(?=[\\s*])\\s*(?:\\*\\s*)?|class\\s+)" + n + "(?![\\w$])", "g"))) {
    const before = tail(body, d.index, 20);
    const declaration = before === "" || /[{};]$/.test(before) || /\bexport(?:\s+default)?$/.test(before);
    if (declaration) ranges.push(blockAt(body, openAround(body, d.index)));
    else {
      // a named function or class expression: the name is its own, inside it
      const open = body.indexOf("{", d.index);
      ranges.push(blockAt(body, open));
    }
  }
  // parameters: function expressions and declarations, methods, catch clauses
  const KEYWORDS = new Set(["if", "for", "while", "switch", "with", "return", "typeof", "await", "yield", "new", "catch"]);
  for (const p of body.matchAll(/(?:\bfunction\b(?:\s*\*)?(?:\s*[\w$]+)?|\bcatch|(?<![\w$])([\w$]+))\s*(?:<[^()<>]*(?:<[^()<>]*>[^()<>]*)*>\s*)?\(/g)) {
    if (p[1] && (KEYWORDS.has(p[1]) || !DEFINES.test(tail(body, p.index)))) continue; // a call, not a method
    const open = p.index + p[0].length - 1;
    const close = matching(body, open);
    if (close < 0) continue;
    const after = body.slice(close + 1, close + 300).match(/^\s*(?::[^{=;]*)?\{/);
    if (!after) continue; // a call, not a definition
    if (!topParts(body.slice(open + 1, close)).flatMap(bindingNames).includes(name)) continue;
    ranges.push([open, blockAt(body, close + after[0].length)[1]]);
  }
  // parameters of arrows: (a, b) => ..., (a): T => ..., a => ...
  for (const a of body.matchAll(/=>/g)) {
    const before = tail(body, a.index, 200);
    const at = trimmedEnd(body, a.index);
    let start, params;
    const typed = before.match(/\)\s*(?::[^(){};=]*)?$/);
    if (typed) {
      const close = at - typed[0].length;
      start = openOf(body, close);
      if (start < 0) continue;
      params = topParts(body.slice(start + 1, close)).flatMap(bindingNames);
    } else {
      const id = before.match(/[A-Za-z_$][\w$]*$/);
      if (!id) continue;
      start = at - id[0].length;
      params = [id[0]];
    }
    if (!params.includes(name)) continue;
    let from = a.index + 2;
    while (/\s/.test(body[from] || "")) from++;
    // a function type in an annotation, `(env: string) => number = ...`, binds nothing
    if (/^[\w$.<>[\]|& ]+=(?![=>])/.test(body.slice(from, from + 200)) && /:\s*$/.test(tail(body, start, 20))) continue;
    const end = body[from] === "{" ? matching(body, from) + 1 : expressionEnd(body, from);
    ranges.push([start, end <= 0 ? body.length : end]);
  }
  return ranges;
}

/** Local name -> { mod, member } for the module's top-level imports and requires of a module by name:
 *  `import { readFileSync as read } from "node:fs"` gives read: fs.readFileSync, `import * as fs from "fs"`
 *  and `const cp = require("child_process")` give the module itself (member null), and so does a top-level
 *  `const { readFileSync } = fs` after it. Read from the code with comments blanked out, the module names
 *  from the source. Relative paths are the app's own and left out; so are `import type` and type-only members,
 *  and requires that are indented inside a block or function (IO sees one in the function as `require`). */
function importsOf(src, code) {
  const out = new Map();
  const add = (local, mod, member) => {
    local = local.trim();
    if (/^[A-Za-z_$][\w$]*$/.test(local) && !/^[./]/.test(mod))
      out.set(local, { mod, member: member || null });
  };
  const braces = [];
  for (let i = 0, depth = 0; i < code.length; i++)
    if (code[i] === "{" || code[i] === "}") braces.push([i, (depth += code[i] === "{" ? 1 : -1)]);
  // at the top of the module: not indented, or outside every brace (regex literals can throw the count off)
  const topLevel = (m) => {
    if (!/^[ \t]/.test(m[0])) return true;
    let lo = 0, hi = braces.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (braces[mid][0] < m.index) lo = mid + 1;
      else hi = mid;
    }
    return lo === 0 || braces[lo - 1][1] === 0;
  };
  const spec = (m, g) => src.slice(m.indices[g][0], m.indices[g][1]);
  /** The first member of `.a.b.c` that isn't `default`. */
  const first = (chain) => (chain || "").split(".").map((x) => x.trim()).filter((x) => x && x !== "default")[0] || null;
  /** [member, local] for each name a `{ ... }` pattern takes out of what it destructures; a rest takes the lot. */
  const destructured = (pattern) =>
    topParts(pattern.trim().slice(1, -1)).flatMap((part) => {
      if (part.trim().startsWith("...")) return bindingNames(part).map((local) => [null, local]);
      const d = defaultOf(part);
      const head = d < 0 ? part : part.slice(0, d);
      const colon = colonAt(head);
      const key = (colon >= 0 ? head.slice(0, colon) : head).trim();
      return bindingNames(colon >= 0 ? part.slice(colon + 1) : part).map((local) => [/^[\w$]+$/.test(key) ? key : null, local]);
    });

  // import ... from "x": found by its keyword, then read up to the module name's quote (no backtracking)
  for (const m of code.matchAll(/^[ \t]*import(?=[\s{*])/gm)) {
    const from = m.index + m[0].length;
    const q = code.slice(from, from + 4000).search(/["'`;()]/);
    if (q < 0 || !/["']/.test(code[from + q])) continue;
    const head = code.slice(from, from + q).trim();
    if (!head.endsWith("from")) continue;
    const clause = head.slice(0, -4).trim();
    if (!/^[\w$*{}\s,]+$/.test(clause) || /^type[\s{]/.test(clause)) continue;
    const quote = from + q;
    const close = code.indexOf(code[quote], quote + 1);
    if (close < 0) continue;
    const mod = src.slice(quote + 1, close);
    const named = clause.match(/\{([^}]*)\}/);
    for (const part of splitTop(clause.replace(/\{[^}]*\}/, ""))) {
      const ns = part.match(/^\*\s*as\s+([\w$]+)$/);
      add(ns ? ns[1] : part, mod, null);
    }
    for (const part of named ? splitTop(named[1]) : []) {
      if (/^type\s+(?!as\b)/.test(part)) continue; // `type as x` imports a member named type
      const [member, local = member] = part.split(/\s+as\s+/);
      add(local, mod, member.trim() === "default" ? null : member.trim());
    }
  }
  for (const m of code.matchAll(/^[ \t]*(?:export\s+)?import\s+([\w$]+)\s*=\s*require\s*\(\s*(["'])([^"'\n]*)\2\s*\)/dgm))
    add(m[1], spec(m, 3), null);
  const PATTERN = "(\\{[^;{}]*(?:\\{[^;{}]*\\}[^;{}]*)*\\}|[\\w$]+)";
  const CHAIN = "((?:\\s*\\.\\s*[\\w$]+)*)";
  const req = new RegExp("^[ \\t]*(?:export\\s+)?(?:const|let|var)\\s+" + PATTERN + "\\s*=\\s*require\\s*\\(\\s*([\"'])([^\"'\\n]*)\\2\\s*\\)" + CHAIN, "dgm");
  for (const m of code.matchAll(req)) {
    if (!topLevel(m)) continue;
    const mod = spec(m, 3);
    const prop = first(m[4]);
    if (!m[1].startsWith("{")) add(m[1], mod, prop);
    else for (const [key, local] of destructured(m[1])) add(local, mod, prop || key);
  }
  // a top-level alias of one of them: const { readFileSync } = fs, const read = fs.promises.readFile
  const alias = new RegExp("^[ \\t]*(?:export\\s+)?(?:const|let|var)\\s+" + PATTERN + "\\s*=\\s*([\\w$]+)" + CHAIN + "\\s*(?=;|$)", "gm");
  for (const m of code.matchAll(alias)) {
    const from = out.get(m[2]);
    if (!from || !topLevel(m)) continue;
    const member = from.member ?? first(m[3]);
    if (!m[1].startsWith("{")) add(m[1], from.mod, member);
    else for (const [key, local] of destructured(m[1])) add(local, from.mod, member || key);
  }
  return out;
}

/** Whether `before` ends with a member-access `.` (or `?.`), excluding the spread/rest `...` operator. */
function isPropAccess(before) {
  return before.endsWith(".") && !before.endsWith("...");
}

/** Whether `before` ends with a free `globalThis.` / `global.` (or `?.`) member access, not `x.globalThis.`. */
function isGlobalThisProp(before) {
  const m = before.match(/(?:^|[^\w$])(?:globalThis|global)\s*\??\.$/);
  if (!m) return false;
  const headStart = m.index + (/^[^\w$]/.test(m[0]) ? 1 : 0);
  const pre = before.slice(0, headStart).trimEnd();
  return !isPropAccess(pre);
}

/** Classify an identifier occurrence at `index` in `body`:
 *  - "explicitGlobal": accessed through `globalThis.<name>` / `global.<name>` (including `?.`),
 *    which bypasses local shadowing and imports of `<name>`
 *  - "nonGlobal": property access on another value, parameter, shadowed local, object/type key,
 *    or method shorthand definition
 *  - "global": a free reference in scope. */
function useKind(body, index, name, paramNames, shadowCache) {
  const before = tail(body, index);
  const after = index + name.length;
  const rest = body.slice(after, after + 300);
  if (isPropAccess(before)) return isGlobalThisProp(before) ? "explicitGlobal" : "nonGlobal";
  if (paramNames.has(name)) return "nonGlobal";
  if (/[{,;(]$/.test(before) && /^\s*\??\s*:/.test(rest)) return "nonGlobal"; // a key or a typed name: `{ env: x }`, `(env: T) =>`
  if (DEFINES.test(before) && /^\s*(?:<[^()]*>)?\s*\(/.test(rest)) {
    const open = after + rest.indexOf("(");
    const close = matching(body, open);
    if (close >= 0 && /^\s*(?::[^{=;]*)?\{/.test(body.slice(close + 1, close + 300))) return "nonGlobal"; // a method
  }
  if (!shadowCache.has(name)) shadowCache.set(name, shadowedIn(body, name));
  return shadowCache.get(name).some(([a, b]) => index >= a && index < b) ? "nonGlobal" : "global";
}

/** Direct uses of built-in globals (IO_GLOBALS, dynamic import(), Math.random, Date, performance.now,
 *  and non-deterministic crypto.* members), ignoring parameters, shadowed locals, property accesses,
 *  object keys, method definitions, and known-pure members like `fs.constants` and `crypto.createHash`. */
function globalEffects(body, paramNames, imports, shadowCache) {
  const io = new Set();
  const nondet = new Set();
  for (const m of body.matchAll(IO_GLOBAL_RE)) {
    const name = m[1];
    const prop = m[3] || null;
    const kind = useKind(body, m.index, name, paramNames, shadowCache);
    if (kind === "nonGlobal") continue;
    if (prop && IO_PURE.has(name + "." + prop)) continue;
    if (kind !== "explicitGlobal" && imports.has(name)) {
      const imp = imports.get(name);
      if (builtin(imp.mod, imp.member ?? prop) !== "io") continue;
    }
    io.add(name);
  }
  for (const m of body.matchAll(DYNAMIC_IMPORT_RE)) {
    const before = tail(body, m.index);
    if (isPropAccess(before)) continue;
    const open = m.index + m[0].length - 1;
    const close = matching(body, open);
    if (DEFINES.test(before) && close >= 0 && /^\s*(?::[^{=;]*)?\{/.test(body.slice(close + 1, close + 300))) continue;
    io.add(m[0]);
  }
  for (const m of body.matchAll(NONDET_RE)) {
    const base = m[1] || m[2] || m[3] || m[4] || m[5];
    const baseIndex = m[3] ? m.index + m[0].lastIndexOf("Date") : m.index;
    const kind = useKind(body, baseIndex, base, paramNames, shadowCache);
    if (kind === "nonGlobal") continue;
    if (kind !== "explicitGlobal" && imports.has(base)) {
      const imp = imports.get(base);
      const member = m[5] ? m[6] : m[1] ? "random" : (m[2] || m[4]) ? "now" : null;
      if (builtin(imp.mod, imp.member ?? member) !== "nondet") continue;
    }
    if (m[5]) {
      if (!NONDET_CRYPTO.test(m[6])) continue;
      nondet.add("crypto." + m[6]);
    } else if (m[3]) {
      nondet.add("new Date");
    } else {
      nondet.add(m[0].replace(/\s*\??\.\s*/, "."));
    }
  }
  return { io, nondet };
}

/** [what, label] for each use in `body` of an imported built-in that does I/O or isn't deterministic.
 *  A use where the name is the function's own (a parameter of it, or of a nested function, arrow or catch
 *  around the use, or declared in a block around it) doesn't count, nor does a property, an object key or a
 *  method's name. Names globalEffects already matched (`fs` imported as fs, `performance.now`) aren't
 *  reported twice. */
function importedCalls(imports, body, paramNames, seen, shadowCache) {
  const out = [];
  for (const [local, { mod, member }] of imports) {
    if ([...seen].some((t) => t === local || t.startsWith(local + "."))) continue;
    if (paramNames.has(local)) continue;
    const base = mod.replace(/^node:/, "").split("/")[0];
    for (const use of body.matchAll(new RegExp("(?<![\\w$])" + esc(local) + "(?![\\w$])(\\s*\\??\\.\\s*([\\w$]+))?", "g"))) {
      if (useKind(body, use.index, local, paramNames, shadowCache) !== "global") continue;
      const at = member ?? use[2] ?? null;
      const what = builtin(mod, at);
      if (what) out.push([what, at ? base + "." + at : base]);
    }
  }
  return out;
}

const parsed = new Map();

function describe(file, line0, name, entry, root) {
  if (!parsed.has(file)) {
    try {
      const src = readFileSync(file, "utf8").replace(/^\uFEFF/, "");
      const code = codeOnly(src, { jsx: /\.[jt]sx$/i.test(file) });
      parsed.set(file, { src, code, top: topLevelFunctions(src), imports: importsOf(src, code) });
    } catch {
      parsed.set(file, null);
    }
  }
  const f = parsed.get(file);
  if (!f) return null;
  const { src, code, top, imports } = f;
  const lines = code.split("\n");
  const n = esc(name);
  const patterns = [
    ["function", new RegExp("^(\\s*)(export\\s+)?(default\\s+)?(async\\s+)?function\\s*(\\*)?\\s*" + n + "\\s*(<[^(]*>)?\\s*\\(")],
    ["binding", new RegExp("^(\\s*)(export\\s+)?(const|let|var)\\s+" + n + "\\s*(:[^=]+)?=\\s*(async\\s*)?(function\\s*\\*?\\s*[\\w$]*\\s*\\(|\\()")],
    ["method", new RegExp("^(\\s*)(static\\s+)?(async\\s+)?(\\*)?\\s*" + n + "\\s*\\(")],
  ];
  let hit = null;
  for (const l of [line0, line0 - 1, line0 - 2, line0 + 1]) {
    if (l < 0 || l >= lines.length) continue;
    for (const [kind, re] of patterns) {
      const m = lines[l].match(re);
      if (m) {
        hit = { kind, m, l };
        break;
      }
    }
    if (hit) break;
  }
  if (!hit) return null;
  const { kind, m, l } = hit;
  const lineStart = lines.slice(0, l).reduce((a, s) => a + s.length + 1, 0);
  const open = lineStart + m[0].length - 1;
  const close = matching(code, open);
  if (close < 0) return null;

  const blockers = [];
  const warnings = [];
  let factor = 1;
  const indent = m[1].length;
  const rel = path.relative(root, file);
  const isEntry = entry && path.resolve(entry) === path.resolve(file);
  const module = isEntry ? "__main__" : rel.replace(/\.[cm]?[jt]sx?$/, "").split(path.sep).join(".");
  if (isEntry) {
    // like Python's if __name__ == "__main__": the main code must not run when verify imports the script
    const guard = code.search(/^if\s*\(\s*(import\.meta\.main|require\.main\s*===\s*module|module\s*===\s*require\.main)\s*\)/m);
    if (guard < 0)
      blockers.push("lives in the entry script, which runs on import: put its main code under if (import.meta.main) (or require.main === module)");
    else if (lineStart > guard) blockers.push("defined inside or after the if (import.meta.main) block");
  }
  // a static method directly in the body of a top-level class is called as Class.name(...), like a function
  let owner = null;
  if (kind === "method" && m[2]) {
    for (const c of code.matchAll(new RegExp("^(?:export\\s+)?(?:default\\s+)?class\\s+(" + IDENT + ")[^{]*\\{", "gm"))) {
      const body = c.index + c[0].length - 1;
      if (body > lineStart || matching(code, body) < close) continue;
      let depth = 0;
      for (const ch of code.slice(body, lineStart)) depth += ch === "{" ? 1 : ch === "}" ? -1 : 0;
      if (depth === 1) owner = c[1];
    }
  }
  if ((kind === "method" && !owner) || (kind !== "method" && indent > 0))
    blockers.push("method or nested function (top-level functions and static methods only for now)");
  if (kind === "function" && m[4]) blockers.push("async");
  if (kind === "function" && m[5]) blockers.push("generator (function*)");
  if (kind === "binding" && m[5]) blockers.push("async");
  if (kind === "binding" && m[3] === "const") warnings.push("declared with const, it's declared with let while the port is switched on");

  const after = close + 1;
  const sig = src.slice(after, after + 400).match(/^\s*:\s*([^{=]+?)\s*(\{|=>)/);
  const returns = sig ? sig[1].trim() : null;
  let bodyStart = code.indexOf("{", after);
  const between = code.slice(after, bodyStart < 0 ? code.length : bodyStart + 1);
  let bodyEnd;
  if (bodyStart >= 0 && !/=>\s*[^\s{]/.test(between)) bodyEnd = matching(code, bodyStart);
  else {
    bodyStart = after;
    const semi = code.indexOf(";", after);
    const nl = code.indexOf("\n", after);
    bodyEnd = Math.min(semi < 0 ? code.length : semi, nl < 0 ? code.length : nl);
  }
  if (bodyEnd < 0) return null;
  const body = code.slice(bodyStart, bodyEnd + 1);

  const paramNames = new Set(topParts(code.slice(open + 1, close)).flatMap(bindingNames));
  const shadowCache = new Map();
  const { io, nondet } = globalEffects(body, paramNames, imports, shadowCache);
  for (const [what, label] of importedCalls(imports, body, paramNames, new Set([...io, ...nondet]), shadowCache)) (what === "io" ? io : nondet).add(label);
  if (/\byield\b/.test(body) && !blockers.some((b) => b.startsWith("generator"))) blockers.push("generator (yield)");
  if (/\bawait\b/.test(body) && !blockers.includes("async")) blockers.push("async");
  if (/\bthis\b/.test(body)) blockers.push("uses this");
  if (io.size) blockers.push("does I/O: " + [...io].slice(0, 3).join(", "));
  if (nondet.size) blockers.push("not deterministic: " + [...nondet].slice(0, 3).join(", "));
  const helpers = [...top].filter((h) => h !== name && new RegExp("\\b" + esc(h) + "\\s*\\(").test(body));
  if (helpers.length) warnings.push("calls " + helpers.slice(0, 3).join(", ") + ", they get ported together with it");

  const params = parseParams(src.slice(open + 1, close));
  if (params.some((p) => p.rest)) {
    warnings.push("takes ...rest parameters");
    factor *= 0.7;
  }
  if (params.some((p) => p.destructured)) {
    warnings.push("destructures its parameters");
    factor *= 0.8;
  }
  if (returns === null || params.some((p) => !p.annotation)) {
    warnings.push("no TypeScript types, types are inferred from recorded calls");
    factor *= 0.9;
  }
  return {
    qualname: owner ? owner + "." + name : name,
    module,
    owner,
    kind: kind === "method" || indent > 0 ? "method" : "function",
    end_line: src.slice(0, bodyEnd + 1).split("\n").length,
    params: params.map(({ name, annotation }) => ({ name, annotation })),
    returns,
    blockers,
    warnings,
    portability: blockers.length ? 0 : factor,
    calls: helpers,
    source: src.slice(lineStart, bodyEnd + 1),
  };
}

function projectRelative(root, file) {
  const rel = path.relative(root, file);
  if (!rel || rel.startsWith("..") || path.isAbsolute(rel)) return null;
  if (rel.split(path.sep).slice(0, -1).some((p) => SKIP_DIRS.has(p))) return null;
  return rel;
}

// ------------------------------------------------------------------ profile

function bucketLabel(frame, root) {
  const { url, functionName } = frame;
  if (!url) {
    if (functionName === "(idle)" || functionName === "(program)" || functionName === "(root)") return null;
    if (functionName === "(garbage collector)") return "garbage collection";
    return "V8 / native";
  }
  if (url.startsWith("node:") || url.startsWith("ext:") || url.startsWith("bun:")) {
    return url.split("/")[0];
  }
  if (url.startsWith("file:")) {
    try {
      const file = fileURLToPath(url);
      const rel = path.relative(root, file).split(path.sep);
      const nm = rel.indexOf("node_modules");
      if (nm >= 0 && nm + 1 < rel.length) {
        const pkg = rel[nm + 1];
        return pkg.startsWith("@") && nm + 2 < rel.length ? `${pkg}/${rel[nm + 2]}` : pkg;
      }
      if (!functionName) return "top-level / anonymous code";
    } catch {}
  }
  return null;
}

function profile(job) {
  const names = readdirSync(job.dir).filter((f) => f.endsWith(".cpuprofile"));
  if (!names.length) throw new Error("the runtime wrote no .cpuprofile (Bun and Deno need a version with --cpu-prof)");
  // one profile per thread (worker_threads) and process: add them all up. The run took as long as
  // the longest one, the main thread's.
  const key = (n) => `${n.callFrame.url}:${n.callFrame.lineNumber}:${n.callFrame.columnNumber}:${n.callFrame.functionName}`;
  const funcs = new Map();
  const outside = new Map();
  let total = 0;
  let activeUs = 0;
  for (const name of names) {
    const prof = JSON.parse(readFileSync(path.join(job.dir, name), "utf8"));
    total = Math.max(total, (prof.endTime - prof.startTime) / 1e6);
    const byId = new Map(prof.nodes.map((n) => [n.id, n]));
    const parent = new Map();
    for (const n of prof.nodes) for (const c of n.children || []) parent.set(c, n.id);
    const selfUs = new Map();
    prof.samples.forEach((id, i) => selfUs.set(id, (selfUs.get(id) || 0) + (prof.timeDeltas[i] || 0)));
    for (const n of prof.nodes) if (!funcs.has(key(n))) funcs.set(key(n), { frame: n.callFrame, self: 0, cum: 0 });
    for (const [id, us] of selfUs) {
      let node = byId.get(id);
      if (!node || us <= 0) continue;
      const fnName = node.callFrame.functionName;
      if (fnName !== "(idle)" && fnName !== "(root)") activeUs += us;
      funcs.get(key(node)).self += us;
      const { url, functionName } = node.callFrame;
      const inProject =
        functionName &&
        url &&
        url.startsWith("file:") &&
        projectRelative(job.root, fileURLToPath(url)) !== null;
      if (!inProject) {
        const label = bucketLabel(node.callFrame, job.root);
        if (label) outside.set(label, (outside.get(label) || 0) + us);
      }
      const seen = new Set();
      while (node) {
        const k = key(node);
        if (!seen.has(k)) {
          seen.add(k);
          funcs.get(k).cum += us;
        }
        node = byId.get(parent.get(node.id));
      }
    }
  }

  const rows = [];
  for (const f of funcs.values()) {
    const { url, functionName, lineNumber } = f.frame;
    if (!functionName || !url.startsWith("file:")) continue;
    const file = fileURLToPath(url);
    const rel = projectRelative(job.root, file);
    if (rel) rows.push({ file, rel, line0: lineNumber, name: functionName, self: f.self / 1e6, cum: f.cum / 1e6 });
  }
  rows.sort((a, b) => b.cum - a.cum);
  const functions = [];
  for (const r of rows.slice(0, 40)) {
    const info = describe(r.file, r.line0, r.name, job.entry, job.root);
    if (info)
      functions.push({ ...info, file: r.rel.split(path.sep).join("/"), line: r.line0 + 1, name: r.name, ncalls: 0, tottime: r.self, cumtime: r.cum });
  }
  const notes = [];
  const attributedSelf = functions.reduce((a, f) => a + f.tottime, 0);
  const denom = Math.max(total, activeUs / 1e6);
  const share = denom > 0 ? Math.min(1, attributedSelf / denom) : 0;
  if (denom > 0 && share < 0.5) {
    const top = [...outside.entries()]
      .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
      .slice(0, 3)
      .map(([l, us]) => `${l} ${Math.round((us / 1e6 / denom) * 100)}%`);
    // the scan's own notes already say there are no call counts and that most of the run is
    // outside the app's functions; this one says where it went instead (#164)
    if (top.length)
      notes.push(`only ${(share * 100).toFixed(1)}% of the run is in your own named functions; the rest went to ${top.join(", ")}`);
  }
  writeFileSync(job.out, JSON.stringify({ wall: total, base: ".", functions, notes }));
}

// ------------------------------------------------------------------ verify

function dec(v) {
  if (Array.isArray(v)) return v.map(dec);
  if (v === null || typeof v !== "object") return v;
  const [[tag, x]] = Object.entries(v);
  switch (tag) {
    case "$float": return Number(x);
    case "$undefined": return undefined;
    case "$bigint": return BigInt(x);
    case "$typed": return new globalThis[x[0]](x[1].map((e) => (x[0].startsWith("Big") ? BigInt(e) : e)));
    case "$map": return new Map(x.map(([k, e]) => [dec(k), dec(e)]));
    case "$set": return new Set(x.map(dec));
    case "$object": return Object.fromEntries(x.map(([k, e]) => [k, dec(e)]));
  }
  throw new Error("unknown tag " + tag);
}

// Numbers match within 1e-9 of their size, or of `scale` (the largest number in the expected array
// holding them) unless both are whole. -0 isn't 0: it prints differently.
function same(a, b, scale = 0) {
  if (a == null && b == null) return true;
  if (typeof a === "number" && typeof b === "number") {
    if (a === b || Number.isNaN(a) || Number.isNaN(b)) return Object.is(a, b);
    if (!Number.isFinite(a) || !Number.isFinite(b)) return false;
    if (Number.isInteger(a) && Number.isInteger(b)) scale = 0;
    return Math.abs(a - b) <= 1e-9 * Math.max(Math.abs(a), Math.abs(b), scale);
  }
  if (typeof a === "bigint" || typeof b === "bigint") {
    try {
      return BigInt(a) === BigInt(b);
    } catch {
      return false;
    }
  }
  const seq = (x) => Array.isArray(x) || (ArrayBuffer.isView(x) && !(x instanceof DataView));
  if (seq(a) && seq(b)) {
    let big = 0;
    for (const x of a) if (typeof x === "number" && Number.isFinite(x)) big = Math.max(big, Math.abs(x));
    return a.length === b.length && Array.from(a).every((x, i) => same(x, b[i], big));
  }
  if (a instanceof Set && b instanceof Set) return a.size === b.size && [...a].every((x) => b.has(x));
  if (a instanceof Map && b instanceof Map) return a.size === b.size && [...a].every(([k, x]) => b.has(k) && same(x, b.get(k)));
  if (a && b && typeof a === "object" && typeof b === "object") {
    const ka = Object.keys(a);
    return ka.length === Object.keys(b).length && ka.every((k) => Object.hasOwn(b, k) && same(a[k], b[k]));
  }
  return a === b;
}

function show(v, limit = 160) {
  let s;
  try {
    s = v === undefined ? "undefined" : JSON.stringify(v, (_, x) => (typeof x === "bigint" ? x + "n" : x)) ?? String(v);
  } catch {
    s = String(v);
  }
  return s.length <= limit ? s : s.slice(0, limit - 1) + "…";
}

const SLICE_LENGTHS = [0, 1, 2, 3, 5, 8, 13, 21, 34, 55];

/** Smaller versions of an array/string/typed-array argument: empty, tiny, and random slices. */
function variants(value, rand) {
  const sliceable = typeof value === "string" || Array.isArray(value) || (ArrayBuffer.isView(value) && !(value instanceof DataView));
  if (!sliceable || value.length < 2) return [];
  return SLICE_LENGTHS.filter((len) => len < value.length).flatMap((len) =>
    Array.from({ length: len < 2 ? 1 : 3 }, () => {
      const start = Math.floor(rand() * (value.length - len + 1));
      return value.slice(start, start + len);
    }),
  );
}

const NON_ASCII = ["\u{e9}", "\u{65e5}\u{672c}", "\u{1f44d}\u{1f3fd}", "e\u{301}"];
const BIG = 4096; // the longest "huge" input, in items or characters
const EDGE_SAMPLE = 20;
const EDGE_LIMIT = 100;
const GROW = [4, 16, 64, 256, 512]; // how much bigger, step by step, than the short version of an input

/** Inputs where ports and originals tend to part ways, of the same type as the recorded value. */
function edges(v) {
  if (typeof v === "number") {
    return Number.isInteger(v) ? [0, 1, -1].filter((x) => x !== v) : [NaN, Infinity, -Infinity, -0, 0, 1e308, 5e-324];
  }
  if (typeof v === "bigint") return [0n, 1n, -1n].filter((x) => x !== v);
  if (typeof v === "string") {
    const cs = [...v];
    const mid = cs.length >> 1;
    return [cs.slice(0, mid).join("") + NON_ASCII[0] + cs.slice(mid).join(""), v + NON_ASCII[1], NON_ASCII[2] + v, v + NON_ASCII[3], ""];
  }
  if (Array.isArray(v) && v.length) return [[], ...edges(v[0]).slice(0, 4).map((e) => [e, ...v.slice(1)])];
  if (ArrayBuffer.isView(v) && !(v instanceof DataView) && v.length) {
    const empty = new v.constructor(0);
    const variants = edges(v[0]).slice(0, 4).map((e) => {
      const copy = new v.constructor(v);
      copy[0] = e;
      return copy;
    });
    return [empty, ...variants];
  }
  return [];
}

/** A short version of a recorded value, so edge cases stay cheap when the real inputs are big. */
function small(v) {
  if (Array.isArray(v) || (ArrayBuffer.isView(v) && !(v instanceof DataView))) return v.slice(0, 8);
  if (typeof v === "string") return [...v].slice(0, 64).join("");
  return v;
}

/** The value repeated factor times (null past BIG items or characters). */
function bigger(v, factor) {
  if (ArrayBuffer.isView(v) && !(v instanceof DataView)) {
    if (!v.length || v.length * factor > BIG) return null;
    const res = new v.constructor(v.length * factor);
    for (let i = 0; i < factor; i++) res.set(v, i * v.length);
    return res;
  }
  if (!(typeof v === "string" || Array.isArray(v)) || !v.length || v.length * factor > BIG) return null;
  return typeof v === "string" ? v.repeat(factor) : Array.from({ length: factor }, () => v).flat(1);
}

/** Edge cases from a sample of recorded calls: special numbers, non-ASCII text, empty and much bigger
 *  inputs. The original answers them; inputs it rejects are skipped. */
function deriveEdges(original, cases, budget, limit) {
  const derived = [];
  const seen = new Set();
  const start = performance.now();
  const left = () => budget - (performance.now() - start) / 1000;
  const attempt = (args) => {
    let encoded;
    let took;
    try {
      encoded = { args: args.map(enc) };
      const key = JSON.stringify(encoded);
      if (seen.has(key)) return null;
      seen.add(key);
      const t = performance.now();
      const r = original(...structuredClone(args));
      took = (performance.now() - t) / 1000;
      encoded.result = enc(r);
    } catch {
      return null; // outside the function's domain: not a fair test
    }
    encoded.derived = "edge";
    derived.push(encoded);
    return took;
  };
  const grown = new Set();
  for (const c of cases.filter((c) => !("raises" in c)).slice(0, EDGE_SAMPLE)) {
    const args = dec(c.args);
    const base = args.map(small);
    for (let i = 0; i < args.length; i++) {
      for (const e of edges(base[i])) {
        if (derived.length >= limit || left() <= 0) return derived;
        attempt(base.map((a, j) => (j === i ? e : a)));
      }
      if (grown.has(i) || bigger(base[i], 4) === null) continue;
      // much bigger inputs, only while their time (assumed quadratic) fits the budget
      grown.add(i);
      let took;
      try {
        const t = performance.now();
        original(...structuredClone(base));
        took = (performance.now() - t) / 1000;
      } catch {
        continue;
      }
      for (const factor of GROW) {
        const big = bigger(base[i], factor);
        if (big === null || derived.length >= limit || took * 16 > left() / 2) break;
        took = attempt(base.map((a, j) => (j === i ? big : a)));
        if (took === null) break;
      }
    }
  }
  return derived;
}

/** Extra test cases: edge cases, then recorded calls with smaller inputs, answered by the original. */
function derive(original, cases, budget, limit) {
  let seed = 0xfa57;
  const rand = () => ((seed = (Math.imul(seed, 1103515245) + 12345) >>> 0) / 4294967296);
  const derived = deriveEdges(original, cases, budget / 2, EDGE_LIMIT);
  limit += derived.length;
  const seen = new Set();
  const start = performance.now();
  for (const c of cases) {
    if ("raises" in c) continue;
    const args = dec(c.args);
    for (let i = 0; i < args.length; i++) {
      for (const smaller of variants(args[i], rand)) {
        if (derived.length >= limit || (performance.now() - start) / 1000 > budget) return derived;
        const next = args.slice();
        next[i] = smaller;
        let encoded;
        try {
          encoded = { args: next.map(enc) };
          const key = JSON.stringify(encoded);
          if (seen.has(key)) continue;
          seen.add(key);
          encoded.result = enc(original(...structuredClone(next)));
        } catch {
          continue; // outside the function's domain: not a fair test
        }
        encoded.derived = true;
        derived.push(encoded);
      }
    }
  }
  return derived;
}

function enc(v) {
  if (v === null || typeof v === "string" || typeof v === "boolean") return v;
  if (typeof v === "number") return Number.isFinite(v) ? (Object.is(v, -0) ? { $float: "-0" } : v) : { $float: String(v) };
  if (v === undefined) return { $undefined: 0 };
  if (typeof v === "bigint") return { $bigint: v.toString() };
  if (Array.isArray(v)) return v.map(enc);
  if (ArrayBuffer.isView(v) && !(v instanceof DataView))
    return { $typed: [v.constructor.name, Array.from(v, (x) => (typeof x === "bigint" ? x.toString() : x))] };
  if (v instanceof Map) return { $map: [...v].map(([k, x]) => [enc(k), enc(x)]) };
  if (v instanceof Set) return { $set: [...v].map(enc) };
  if (typeof v === "object") {
    const proto = Object.getPrototypeOf(v);
    if (proto === Object.prototype || proto === null) return { $object: Object.entries(v).map(([k, x]) => [k, enc(x)]) };
  }
  throw new Error("unsupported");
}

function bestPass(fn, calls, budget) {
  let best = Infinity;
  let spent = 0;
  let passes = 0;
  while (passes < 3 || (spent < budget && passes < 2000)) {
    const t = performance.now();
    for (const args of calls) {
      try {
        fn(...args);
      } catch {}
    }
    const took = (performance.now() - t) / 1000;
    best = Math.min(best, took);
    spent += took;
    passes++;
  }
  return best;
}

async function verify(job) {
  process.env.FAST_ANYTHING_DISABLE = "1";
  const fast = createRequire(import.meta.url)(job.addon)[job.js_name];
  if (typeof fast !== "function") throw new Error(job.addon + " doesn't export " + job.js_name);
  let original = null;
  let handed = null; // from a block fast-anything appended to the module, so unexported functions work too
  globalThis.__fastAnythingOriginal = (fn) => {
    handed = fn;
  };
  try {
    const mod = await import(pathToFileURL(job.module_file).href);
    original = handed ?? mod[job.func] ?? mod.default?.[job.func] ?? null;
  } catch {}

  const cases = readFileSync(job.cases, "utf8").split("\n").filter((l) => l.trim()).map((l) => JSON.parse(l));
  const recorded = cases.length;
  // held-out calls (at the end) only get replayed: shrinking them would show their values in failures
  const visible = cases.slice(0, recorded - (job.held_out ?? 0));
  const derived = typeof original === "function" ? derive(original, visible, job.derive_budget ?? 3, job.derive_limit ?? 200) : [];
  cases.push(...derived);
  const report = { total: cases.length, derived: derived.length, edges: derived.filter((c) => c.derived === "edge").length, passed: 0, failed: 0, failures: [] };
  const calls = [];
  cases.forEach((c, i) => {
    const args = dec(c.args);
    calls.push(args);
    let got;
    let err = null;
    try {
      got = fast(...args);
    } catch (e) {
      err = e;
    }
    let ok;
    let expected;
    if ("raises" in c) [ok, expected] = [err !== null, "throws " + c.raises];
    else {
      const want = dec(c.result);
      [ok, expected] = [err === null && same(want, got), show(want)];
    }
    if (ok) {
      report.passed++;
      return;
    }
    report.failed++;
    if (report.failures.length < 8)
      report.failures.push({
        case: i,
        call: (c.derived === "edge" ? "(edge) " : c.derived ? "(derived) " : "") + job.func + "(" + args.map((a) => show(a, 80)).join(", ") + ")",
        expected,
        got: err === null ? show(got) : "threw " + (err?.name ?? "Error") + ": " + show(String(err?.message ?? err), 200),
      });
  });

  if (cases.length && report.failed === 0 && typeof original === "function") {
    calls.length = recorded;
    const js = bestPass(original, calls, job.budget ?? 0.5);
    const rs = bestPass(fast, calls, job.budget ?? 0.5);
    report.micro = { original_ns: (js / calls.length) * 1e9, rust_ns: (rs / calls.length) * 1e9, speedup: rs > 0 ? js / rs : 0 };
  }
  writeFileSync(job.out, JSON.stringify(report));
}

const [mode, jobPath] = process.argv.slice(2);
const job = JSON.parse(readFileSync(jobPath, "utf8"));
if (mode === "profile") profile(job);
else if (mode === "verify") await verify(job);
else throw new Error("unknown mode " + mode);
