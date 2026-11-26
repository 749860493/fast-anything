//! C and C++ backend: apps built with gcc or clang, e.g.  clang -O2 main.c fuzzy.c  (app arguments
//! after a second --). fast-anything builds the binary itself into .fast/bin. Profiled with a
//! one-line timer per function for one run, read with a small declaration parser, and bridged with
//! a block appended to the source file that loads the port with LoadLibrary or dlopen.

use crate::backend::{self, Lang, Profile, Recording};
use crate::hotspot::{Hotspot, Param};
use crate::native::{self, Flavor, Patches};
use crate::python::DISABLE_ENV;
use crate::state::{Ctx, Port, Verify};
use crate::ui;
use crate::util::{self, combined, output, tail};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const PROF_H: &str = include_str!("cfamily/prof.h");
const PROF_C: &str = include_str!("cfamily/prof.c");
const CLOCK_H: &str = include_str!("cfamily/clock.h");
const REC_H: &str = include_str!("cfamily/rec.h");
const REC_C: &str = include_str!("cfamily/rec.c");
const REC_HPP: &str = include_str!("cfamily/rec.hpp");
const JSON_HPP: &str = include_str!("cfamily/json.hpp");
const TYPED_RS: &str = include_str!("cfamily/typed.rs");

/// Intel oneAPI: icx (C, clang-based), icpx and dpcpp (C++). icx-cl takes MSVC-style flags, so it isn't listed.
const C_COMPILERS: &[&str] = &["gcc", "clang", "cc", "icx"];
const CPP_COMPILERS: &[&str] = &["g++", "clang++", "c++", "icpx", "dpcpp"];
const C_EXT: &[&str] = &["c"];
const CPP_EXT: &[&str] = &["cc", "cpp", "cxx", "c++", "C"];
/// Compiler flags whose value is the next argument.
const VALUE_FLAGS: &[&str] =
    &["-o", "-I", "-D", "-U", "-include", "-isystem", "-L", "-l", "-x", "-MF", "-MT", "-target"];

fn base_name(program: &str) -> String {
    // gcc-14, clang-19, x86_64-linux-gnu-g++ ...
    let stem = util::program_stem(program);
    stem.rsplit('-').find(|p| !p.chars().all(|c| c.is_ascii_digit() || c == '.')).unwrap_or(&stem).to_string()
}

pub fn detect(program: &str) -> Option<Lang> {
    let b = base_name(program);
    if CPP_COMPILERS.contains(&b.as_str()) {
        Some(Lang::Cpp)
    } else if C_COMPILERS.contains(&b.as_str()) {
        Some(Lang::C)
    } else {
        None
    }
}

/// A C compiler given C++ sources still builds C++.
pub fn refine(lang: Lang, args: &[String]) -> Lang {
    if lang == Lang::C && Build::parse(args).is_ok_and(|b| b.sources.iter().any(|s| is_cpp_file(s))) {
        Lang::Cpp
    } else {
        lang
    }
}

fn is_cpp_file(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|e| CPP_EXT.contains(&e.to_string_lossy().as_ref()))
}

pub fn resolve(program: &str, root: &Path) -> Result<String> {
    let prog = if program.contains(['/', '\\']) { root.join(program) } else { PathBuf::from(program) };
    let out = output(Command::new(&prog).arg("--version").current_dir(root), program)?;
    ensure!(out.status.success(), "{program} failed:\n{}", tail(&combined(&out), 5));
    Ok(prog.to_string_lossy().into_owned())
}

/// compiler [flags] sources... [-o out] [-- app args]
struct Build {
    flags: Vec<String>,
    sources: Vec<String>,
    args: Vec<String>,
}

impl Build {
    fn parse(args: &[String]) -> Result<Build> {
        let (compile, app) = match args.iter().position(|a| a == "--") {
            Some(i) => (&args[..i], args[i + 1..].to_vec()),
            None => (args, Vec::new()),
        };
        let (mut flags, mut sources) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < compile.len() {
            let a = &compile[i];
            if a == "-o" {
                i += 2;
                continue;
            }
            let ext = Path::new(a).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
            if !a.starts_with('-') && (C_EXT.contains(&ext.as_str()) || CPP_EXT.contains(&ext.as_str())) {
                sources.push(a.clone());
            } else {
                flags.push(a.clone());
                if VALUE_FLAGS.contains(&a.as_str()) && i + 1 < compile.len() {
                    flags.push(compile[i + 1].clone());
                    i += 1;
                }
            }
            i += 1;
        }
        ensure!(
            !sources.is_empty(),
            "give the sources to compile:  fast-anything scan -- clang -O2 main.c fuzzy.c  (app arguments after a second --)"
        );
        Ok(Build { flags, sources, args: app })
    }
}

fn binary(ctx: &Ctx) -> PathBuf {
    ctx.fast.join("bin").join(if cfg!(windows) { "app.exe" } else { "app" })
}

/// Copies the runtime files into .fast/cfa and returns that folder.
fn runtime_dir(ctx: &Ctx) -> Result<PathBuf> {
    let dir = ctx.fast.join("cfa");
    fs::create_dir_all(&dir)?;
    for (name, text) in [
        ("fa_prof.h", PROF_H.to_string()),
        ("fa_clock.h", CLOCK_H.to_string()),
        ("fa_rec.h", REC_H.to_string()),
        ("fa_rec.hpp", REC_HPP.replace("/*JSON*/", JSON_HPP)),
    ] {
        fs::write(dir.join(name), text)?;
    }
    Ok(dir)
}

fn compile(ctx: &Ctx, cc: &str, b: &Build, extra: &[PathBuf], include: Option<&Path>) -> Result<PathBuf> {
    let bin = binary(ctx);
    fs::create_dir_all(bin.parent().context("bin")?)?;
    let mut cmd = Command::new(cc);
    cmd.args(&b.flags).args(&b.sources).args(extra).current_dir(&ctx.root).arg("-o").arg(&bin);
    if let Some(h) = include {
        cmd.arg("-include").arg(h);
    }
    let out = output(&mut cmd, cc)?;
    ensure!(out.status.success(), "building the app failed:\n{}", errors_first(&combined(&out)));
    Ok(bin)
}

/// Compiler output with the error lines (and a few after each) first: warnings can bury them.
fn errors_first(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let mut keep = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.contains("error") && !l.contains("warning") {
            keep.extend(lines[i..(i + 4).min(lines.len())].iter().copied());
        }
    }
    if keep.is_empty() { tail(log, 30) } else { keep.into_iter().take(40).collect::<Vec<_>>().join("\n") }
}

fn run(ctx: &Ctx, bin: &Path, b: &Build, env: &[(&str, &Path)], derive: bool) -> Result<std::process::Output> {
    Ok(run_for(ctx, bin, b, env, derive, None)?.0)
}

/// run, ending the app after `duration` seconds (and stopping it from outside 15s later).
fn run_for(
    ctx: &Ctx,
    bin: &Path,
    b: &Build,
    env: &[(&str, &Path)],
    derive: bool,
    duration: Option<f64>,
) -> Result<(std::process::Output, bool)> {
    let mut cmd = Command::new(bin);
    if let Some(d) = duration {
        cmd.env("FAST_ANYTHING_STOP_AFTER_MS", ((d * 1000.0) as u64).to_string());
    }
    cmd.args(&b.args).current_dir(&ctx.root).env(DISABLE_ENV, "1").stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    if !derive {
        cmd.env("FAST_ANYTHING_RECORD_DERIVE", "0");
    }
    util::output_for(
        &mut cmd,
        "the app",
        duration.map(|d| Duration::from_secs_f64(d.ceil() + 15.0)),
        Duration::from_secs(10),
    )
}

pub fn bench_command(ctx: &Ctx, cc: &str) -> Result<(String, Vec<String>)> {
    let b = Build::parse(&ctx.state.command[1..])?;
    let bin = compile(ctx, cc, &b, &[], None)?;
    Ok((bin.to_string_lossy().into_owned(), b.args))
}

// ------------------------------------------------------------------ reading the source

/// The source with comments, string and character literals and preprocessor lines blanked out
/// (same length, newlines kept), so braces and parentheses can be counted.
fn blank(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let mut line_start = true;
    let wipe = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    while i < b.len() {
        let c = b[i];
        if line_start && c == b'#' {
            let start = i;
            while i < b.len() && !(b[i] == b'\n' && b[i - 1] != b'\\') {
                i += 1;
            }
            wipe(&mut out, start, i);
            continue;
        }
        if c == b'\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if !c.is_ascii_whitespace() {
            line_start = false;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            wipe(&mut out, start, i);
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            wipe(&mut out, start, i);
        } else if c == b'"' || c == b'\'' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != c && b[i] != b'\n' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            wipe(&mut out, start + 1, i.saturating_sub(1).max(start + 1));
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty())
}

const SPECIFIERS: &[&str] = &[
    "static",
    "inline",
    "extern",
    "constexpr",
    "consteval",
    "virtual",
    "explicit",
    "friend",
    "__inline",
    "__forceinline",
    "register",
];
const KEYWORDS: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "return",
    "sizeof",
    "else",
    "do",
    "case",
    "new",
    "delete",
    "throw",
    "alignof",
    "decltype",
    "static_assert",
    "defined",
];
const IO: &[&str] = &[
    "printf", "fprintf", "puts", "fputs", "putchar", "fputc", "fwrite", "fread", "fopen", "fclose", "fgets", "fgetc",
    "getchar", "scanf", "fscanf", "perror", "system", "exit", "abort", "getline", "cout", "cerr", "cin", "clog",
    "ofstream", "ifstream", "fstream", "sleep", "usleep", "Sleep", "remove", "rename", "getenv", "open", "read",
    "write", "close", "send", "recv", "socket", "thread", "async",
];
const NONDET: &[&str] = &[
    "rand",
    "srand",
    "random",
    "time",
    "clock",
    "random_device",
    "system_clock",
    "steady_clock",
    "high_resolution_clock",
    "mt19937",
    "default_random_engine",
];

#[derive(Debug, Clone)]
struct CFunc {
    name: String,
    namespaces: Vec<String>,
    class: Option<String>,
    is_static: bool,
    template: bool,
    body_open: usize,
    body_close: usize,
    params: Vec<Param>,
    ret: String,
}

#[derive(Debug)]
enum Scope {
    Namespace(String),
    Transparent,
    Class(String),
    Body(usize),
    Other,
}

fn match_back(s: &[u8], close: usize, open: u8, shut: u8) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = close as isize;
    while i >= 0 {
        let c = s[i as usize];
        if c == shut {
            depth += 1;
        } else if c == open {
            depth -= 1;
            if depth == 0 {
                return Some(i as usize);
            }
        }
        i -= 1;
    }
    None
}

fn split_top(s: &str, sep: u8) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut depth, mut start, mut out) = (0i32, 0, Vec::new());
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' | b'<' | b'[' | b'{' => depth += 1,
            b')' | b'>' | b']' | b'}' => depth -= 1,
            _ if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn squash(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" *", "*")
        .replace("* ", "*")
        .replace(" &", "&")
        .replace("& ", "&")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(", ", ",")
}

fn parse_params(list: &str) -> Vec<Param> {
    let list = list.trim();
    if list.is_empty() || list == "void" {
        return vec![];
    }
    split_top(list, b',')
        .into_iter()
        .map(|p| {
            let mut parts = split_top(p, b'=').into_iter();
            let decl = parts.next().unwrap_or_default().trim();
            let default = parts.next().map(|d| d.trim().to_string());
            let (decl, array) = match decl.strip_suffix(']').and_then(|d| d.rfind('[').map(|i| &d[..i])) {
                Some(d) => (d.trim_end(), true),
                None => (decl, false),
            };
            let name_start = decl.rfind(|c: char| !(c.is_alphanumeric() || c == '_')).map(|i| i + 1).unwrap_or(0);
            let (ty, name) = (decl[..name_start].trim(), &decl[name_start..]);
            let (ty, name) = if ty.is_empty() || ty.ends_with("::") { (decl, "") } else { (ty, name) };
            let ty = if array { format!("{}*", ty.trim()) } else { ty.to_string() };
            Param { name: name.to_string(), annotation: Some(squash(&ty)), default, kwonly: false, splat: false }
        })
        .collect()
}

fn parse(src: &str) -> Vec<CFunc> {
    let text = blank(src);
    let b = text.as_bytes();
    let mut stack: Vec<Scope> = Vec::new();
    let mut stmt = 0usize;
    let mut found: Vec<CFunc> = Vec::new();
    let decl_scope = |stack: &Vec<Scope>| {
        stack.iter().all(|s| matches!(s, Scope::Namespace(_) | Scope::Transparent | Scope::Class(_)))
    };
    for i in 0..b.len() {
        match b[i] {
            b'{' => {
                if !decl_scope(&stack) {
                    stack.push(Scope::Other);
                    continue;
                }
                let head = text[stmt..i].trim();
                let w: Vec<&str> = words(head).collect();
                let scope = if w.first() == Some(&"namespace") {
                    Scope::Namespace(w.get(1).map(|s| s.to_string()).unwrap_or_default())
                } else if w == ["extern"] {
                    Scope::Transparent
                } else if let Some(k) = w
                    .iter()
                    .position(|x| ["class", "struct", "union"].contains(x))
                    .filter(|_| !head.contains('(') && !head.contains('='))
                {
                    if w.get(k).is_some_and(|x| *x == "union") {
                        Scope::Other
                    } else {
                        Scope::Class(w.get(k + 1).map(|s| s.to_string()).unwrap_or_default())
                    }
                } else if let Some(f) = function_head(head, i, &stack) {
                    found.push(f);
                    Scope::Body(found.len() - 1)
                } else {
                    Scope::Other
                };
                stack.push(scope);
                stmt = i + 1;
            }
            b'}' => {
                if let Some(Scope::Body(k)) = stack.pop() {
                    found[k].body_close = i;
                }
                if decl_scope(&stack) {
                    stmt = i + 1;
                }
            }
            b';' if decl_scope(&stack) => stmt = i + 1,
            b':' if decl_scope(&stack) && b.get(i + 1) != Some(&b':') && i > 0 && b[i - 1] != b':' => {
                let head = text[stmt..i].trim();
                if ["public", "private", "protected"].contains(&head) {
                    stmt = i + 1;
                }
            }
            _ => {}
        }
    }
    found.retain(|f| f.body_close > f.body_open);
    found
}

fn function_head(head: &str, brace: usize, stack: &[Scope]) -> Option<CFunc> {
    let hb = head.as_bytes();
    let close = head.rfind(')')?;
    let after = &head[close + 1..];
    if !words(after).all(|w| ["const", "noexcept", "override", "final", "mutable"].contains(&w))
        || after.contains("->")
        || after.contains(':')
    {
        return None;
    }
    let open = match_back(hb, close, b'(', b')')?;
    let before = head[..open].trim_end();
    let name_start =
        before.rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '~')).map(|i| i + 1).unwrap_or(0);
    let name = &before[name_start..];
    let prefix = before[..name_start].trim();
    if name.is_empty()
        || prefix.is_empty()
        || KEYWORDS.contains(&name)
        || name.contains("operator")
        || name.contains('~')
    {
        return None;
    }
    let template = prefix.starts_with("template");
    let is_static = words(prefix).any(|w| w == "static");
    let mut ret = prefix.to_string();
    if template && let Some(gt) = ret.find('>') {
        ret = ret[gt + 1..].to_string();
    }
    let ret = squash(&words_except(&ret, SPECIFIERS));
    let namespaces =
        stack.iter().filter_map(|s| if let Scope::Namespace(n) = s { Some(n.clone()) } else { None }).collect();
    let class = stack.iter().rev().find_map(|s| if let Scope::Class(n) = s { Some(n.clone()) } else { None });
    Some(CFunc {
        name: name.to_string(),
        namespaces,
        class,
        is_static,
        template,
        body_open: brace,
        body_close: 0,
        params: parse_params(&head[open + 1..close]),
        ret,
    })
}

/// Removes whole words (specifiers, attributes) from a declaration, keeping everything else.
fn words_except(s: &str, drop: &[&str]) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !drop.contains(&word.as_str()) {
            out.push_str(word);
        }
        word.clear();
    };
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    let out = out.trim().to_string();
    // [[nodiscard]] and __attribute__((...))
    let mut cleaned = out.clone();
    while let Some(a) = cleaned.find("[[") {
        let Some(z) = cleaned[a..].find("]]") else { break };
        cleaned.replace_range(a..a + z + 2, "");
    }
    while let Some(a) = cleaned.find("__attribute__") {
        let rest = &cleaned[a..];
        let Some(open) = rest.find('(') else { break };
        let Some(close) = match_forward(rest.as_bytes(), open) else { break };
        cleaned.replace_range(a..a + close + 1, "");
    }
    cleaned.trim().to_string()
}

fn match_forward(s: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, &c) in s.iter().enumerate().skip(open) {
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

// ------------------------------------------------------------------ types

#[derive(Debug, Clone, Copy, PartialEq)]
enum Scalar {
    Int { signed: bool, rust: &'static str },
    Float(&'static str),
    Bool,
}

fn scalar(t: &str) -> Option<Scalar> {
    let t = squash(t).replace("const ", "").replace("std::", "");
    let t = t.trim();
    let int = |signed, rust| Some(Scalar::Int { signed, rust });
    match t {
        "char" | "signed char" | "int8_t" => int(true, "i8"),
        "unsigned char" | "uint8_t" => int(false, "u8"),
        "short" | "short int" | "signed short" | "int16_t" => int(true, "i16"),
        "unsigned short" | "unsigned short int" | "uint16_t" => int(false, "u16"),
        "int" | "signed" | "signed int" | "int32_t" => int(true, "i32"),
        "unsigned" | "unsigned int" | "uint32_t" => int(false, "u32"),
        "long" | "long int" | "long long" | "long long int" | "int64_t" | "ssize_t" | "ptrdiff_t" | "intptr_t" => {
            int(true, "i64")
        }
        "unsigned long"
        | "unsigned long int"
        | "unsigned long long"
        | "unsigned long long int"
        | "uint64_t"
        | "size_t"
        | "uintptr_t" => int(false, "u64"),
        "float" => Some(Scalar::Float("f32")),
        "double" | "long double" => Some(Scalar::Float("f64")),
        "bool" | "_Bool" => Some(Scalar::Bool),
        _ => None,
    }
}

/// What a C parameter is to the recorder and the bridge.
#[derive(Debug, Clone, PartialEq)]
enum CKind {
    Scalar(Scalar),
    Str,
    /// const T* data whose length is the parameter at this index
    Array(Scalar, usize),
    Len,
}

fn c_kinds(params: &[Param]) -> std::result::Result<Vec<CKind>, String> {
    let mut kinds: Vec<Option<CKind>> = vec![None; params.len()];
    for (i, p) in params.iter().enumerate() {
        let t = squash(p.annotation.as_deref().unwrap_or(""));
        if kinds[i].is_some() {
            continue;
        }
        if let Some(s) = scalar(&t) {
            kinds[i] = Some(CKind::Scalar(s));
        } else if t == "const char*" || t == "char const*" {
            kinds[i] = Some(CKind::Str);
        } else if let Some(inner) = t.strip_suffix('*').filter(|_| t.starts_with("const ") || t.contains(" const*")) {
            let inner = inner.replace("const", "");
            let s = scalar(&inner).ok_or_else(|| format!("takes a {t}"))?;
            let len = params.get(i + 1).and_then(|n| scalar(n.annotation.as_deref().unwrap_or("")));
            ensure_len(len, &t)?;
            kinds[i] = Some(CKind::Array(s, i + 1));
            kinds[i + 1] = Some(CKind::Len);
        } else if t.ends_with('*') {
            return Err(format!("takes a pointer it may write through ({t})"));
        } else {
            return Err(format!("takes a {t}"));
        }
    }
    Ok(kinds.into_iter().map(|k| k.unwrap_or(CKind::Len)).collect())
}

fn ensure_len(len: Option<Scalar>, t: &str) -> std::result::Result<(), String> {
    match len {
        Some(Scalar::Int { .. }) => Ok(()),
        _ => Err(format!("takes a {t} without a length right after it")),
    }
}

/// C++ types fast-anything can carry across: scalars, std::string, vector, array, map/unordered_map
/// with string keys and pair, by value or const reference.
fn cpp_rust_type(t: &str) -> Option<String> {
    let t = squash(t);
    let t = t.trim().trim_start_matches("const ").trim_end_matches('&').trim();
    let t = t.strip_suffix(" const").unwrap_or(t).trim();
    if let Some(s) = scalar(t) {
        return Some(scalar_rust(s).into());
    }
    let t = t.strip_prefix("std::").unwrap_or(t);
    if t == "string" || t == "const char*" {
        return Some("String".into());
    }
    let (raw, inner) = t.strip_suffix('>')?.split_once('<')?;
    let args = split_top(inner, b',');
    Some(match (raw, args.as_slice()) {
        ("vector", [x, ..]) => format!("Vec<{}>", cpp_rust_type(x)?),
        ("array", [x, n]) => format!("[{}; {}]", cpp_rust_type(x)?, n.trim().parse::<usize>().ok()?),
        ("map", [k, v, ..]) if cpp_rust_type(k)? == "String" => {
            format!("std::collections::BTreeMap<String, {}>", cpp_rust_type(v)?)
        }
        ("unordered_map", [k, v, ..]) if cpp_rust_type(k)? == "String" => {
            format!("std::collections::HashMap<String, {}>", cpp_rust_type(v)?)
        }
        ("pair", [a, b]) => format!("({}, {})", cpp_rust_type(a)?, cpp_rust_type(b)?),
        _ => return None,
    })
}

fn scalar_rust(s: Scalar) -> &'static str {
    match s {
        Scalar::Int { rust, .. } | Scalar::Float(rust) => rust,
        Scalar::Bool => "bool",
    }
}

pub fn rust_type_c(t: &str) -> Option<String> {
    let t = squash(t);
    if let Some(s) = scalar(&t) {
        return Some(scalar_rust(s).into());
    }
    if t == "const char*" {
        return Some("String".into());
    }
    let inner = t.strip_suffix('*')?.replace("const", "");
    Some(format!("Vec<{}>", scalar_rust(scalar(&inner)?)))
}

fn blockers(f: &CFunc, body: &str, cpp: bool, funcs: &[CFunc]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    if f.class.is_some() && !f.is_static {
        blockers.push("instance method (static methods and free functions only for now)".into());
    }
    if f.name.contains("::") {
        blockers.push("method defined outside its class".into());
    }
    if f.template {
        blockers.push("template".into());
    }
    if f.name == "main" {
        blockers.push("the entry point".into());
    }
    if f.params.iter().any(|p| p.name.is_empty()) {
        blockers.push("has unnamed parameters".into());
    }
    if cpp {
        for p in &f.params {
            let t = p.annotation.clone().unwrap_or_default();
            if (t.ends_with('&') && !t.starts_with("const")) || t.ends_with("&&") {
                blockers.push(format!("takes a non-const reference ({t})"));
            } else if cpp_rust_type(&t).is_none() {
                blockers.push(format!("takes a {t}"));
            }
        }
        if f.ret == "void" {
            blockers.push("returns nothing (void)".into());
        } else if f.ret.ends_with('&') || f.ret.ends_with('*') || cpp_rust_type(&f.ret).is_none() {
            blockers.push(format!("returns a {}", f.ret));
        }
    } else {
        if let Err(e) = c_kinds(&f.params) {
            blockers.push(e);
        }
        if f.ret == "void" {
            blockers.push("returns nothing (void)".into());
        } else if scalar(&f.ret).is_none() {
            blockers.push(format!("returns a {} (C ports return numbers or bools for now)", f.ret));
        }
    }
    let b = body.as_bytes();
    let (mut io, mut nondet, mut helpers) = (Vec::new(), Vec::new(), Vec::new());
    let mut statics = false;
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let w = &body[start..i];
            let member = start >= 1 && (b[start - 1] == b'.' || (start >= 2 && &body[start - 2..start] == "->"));
            let mut j = i;
            while j < b.len() && b[j] == b' ' {
                j += 1;
            }
            let call = b.get(j) == Some(&b'(');
            if w == "static" {
                statics = true;
            }
            if !member
                && IO.contains(&w)
                && (call || ["cout", "cerr", "cin", "clog", "ofstream", "ifstream", "fstream", "thread"].contains(&w))
            {
                io.push(w.to_string());
            }
            if !member && NONDET.contains(&w) {
                nondet.push(w.to_string());
            }
            if !member && call && w != f.name && funcs.iter().any(|g| g.name == w && g.class == f.class) {
                helpers.push(w.to_string());
            }
        } else {
            i += 1;
        }
    }
    io.dedup();
    nondet.dedup();
    helpers.sort();
    helpers.dedup();
    if !io.is_empty() {
        blockers.push(format!("does I/O: {}", io.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
    }
    if !nondet.is_empty() {
        blockers.push(format!("not deterministic: {}", nondet.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
    }
    if statics {
        blockers.push("keeps static variables".into());
    }
    if !helpers.is_empty() {
        warnings.push(format!(
            "calls {}, they get ported together with it",
            helpers.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    (blockers, warnings, helpers)
}

struct Described {
    path: PathBuf,
    f: CFunc,
    hs: Hotspot,
}

fn line_of(src: &str, offset: usize) -> u32 {
    src[..offset.min(src.len())].bytes().filter(|&c| c == b'\n').count() as u32 + 1
}

fn describe(ctx: &Ctx, b: &Build, lang: Lang) -> Result<Vec<Described>> {
    let mut out = Vec::new();
    for s in &b.sources {
        let path = util::absolute(&ctx.root.join(s));
        let src = fs::read_to_string(&path).with_context(|| format!("reading {s}"))?;
        let cpp = lang == Lang::Cpp || is_cpp_file(s);
        let funcs = parse(&src);
        let text = blank(&src);
        let file = util::forward_slashes(path.strip_prefix(&ctx.root).unwrap_or(&path));
        let module = file.rsplit_once('.').map(|(m, _)| m).unwrap_or(&file).replace('/', ".");
        for f in &funcs {
            let body = &text[f.body_open + 1..f.body_close];
            let (blockers, warnings, calls) = blockers(f, body, cpp, &funcs);
            // the declaration starts at the line of its return type
            let decl_start = src[..f.body_open]
                .rfind(&f.ret.split_whitespace().next().unwrap_or(&f.name).to_string())
                .unwrap_or(f.body_open);
            let line = line_of(&src, decl_start.min(f.body_open));
            let mut qual: Vec<String> = f.namespaces.iter().filter(|n| !n.is_empty()).cloned().collect();
            qual.extend(f.class.clone());
            qual.push(f.name.clone());
            let hs = Hotspot {
                file: file.clone(),
                line,
                end_line: line_of(&src, f.body_close),
                name: f.name.clone(),
                qualname: qual.join("."),
                module: Some(module.clone()),
                kind: "function".into(),
                portability: if blockers.is_empty() { 0.9 } else { 0.0 },
                blockers,
                warnings,
                params: f.params.clone(),
                returns: Some(f.ret.clone()),
                source: src[src[..f.body_open].rfind(['\n', ';', '}']).map(|i| i + 1).unwrap_or(0)..=f.body_close]
                    .trim()
                    .replace('\r', ""),
                calls,
                owner: f.class.clone(),
                ..Hotspot::default()
            };
            out.push(Described { path: path.clone(), f: f.clone(), hs });
        }
    }
    Ok(out)
}

fn insert_at(text: &str, mut at: Vec<(usize, String)>) -> String {
    at.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut out = text.to_string();
    for (offset, piece) in at {
        out.insert_str(offset, &piece);
    }
    out
}

/// The build with one runtime source added, written in the language the compiler builds.
fn runtime_source(dir: &Path, name: &str, text: &str, lang: Lang) -> Result<PathBuf> {
    let path = dir.join(format!("{name}.{}", if lang == Lang::Cpp { "cpp" } else { "c" }));
    fs::write(&path, text)?;
    Ok(path)
}

/// `duration`: end the app after that many seconds (servers that run until closed).
pub fn profile(ctx: &Ctx, cc: &str, args: &[String], lang: Lang, duration: Option<f64>) -> Result<Profile> {
    let b = Build::parse(args)?;
    let found = describe(ctx, &b, lang)?;
    let dir = runtime_dir(ctx)?;
    let prof_c = runtime_source(&dir, "fa_prof", PROF_C, lang)?;
    let mut by_file: BTreeMap<&Path, Vec<(usize, String)>> = BTreeMap::new();
    for (i, d) in found.iter().enumerate() {
        by_file.entry(&d.path).or_default().push((d.f.body_open + 1, format!(" FA_PROF({i});")));
    }
    let mut patches = Patches::new(ctx)?;
    for (path, at) in by_file {
        let text = fs::read_to_string(path)?;
        patches.write(path, &insert_at(&text, at))?;
    }
    let bin = compile(ctx, cc, &b, &[prof_c], Some(&dir.join("fa_prof.h")));
    patches.restore()?;
    let bin = bin?;
    let out = ctx.fast.join("profile.json");
    let _ = fs::remove_file(&out);
    let (ran, killed) = run_for(ctx, &bin, &b, &[("FAST_ANYTHING_PROF_OUT", &out)], true, duration)?;
    ensure!(!killed, "the app was still running 15s after --duration");
    ensure!(ran.status.success(), "the app failed ({}):\n{}", ran.status, tail(&combined(&ran), 30));
    let result: Value = serde_json::from_str(
        &fs::read_to_string(&out).context("the profiler wrote nothing: does main return normally?")?,
    )?;
    let wall = result["wall"].as_f64().unwrap_or(0.0);
    let stats = result["stats"].as_object().cloned().unwrap_or_default();
    let stat = |i: usize| stats.get(&i.to_string()).and_then(Value::as_array).cloned();
    let mut functions = Vec::new();
    for (i, d) in found.iter().enumerate() {
        let helper = found
            .iter()
            .enumerate()
            .any(|(j, g)| stat(j).is_some() && g.path == d.path && g.hs.calls.contains(&d.f.name));
        let mut hs = d.hs.clone();
        match stat(i) {
            Some(s) => {
                hs.ncalls = s[0].as_u64().unwrap_or(0);
                hs.tottime = s[1].as_f64().unwrap_or(0.0);
                hs.cumtime = s[2].as_f64().unwrap_or(0.0);
                functions.push(hs);
            }
            None if helper => functions.push(hs),
            None => {}
        }
    }
    Ok(Profile { wall, base: ".".into(), functions, notes: Vec::new() })
}

// ------------------------------------------------------------------ recording

fn arg_names(hs: &Hotspot) -> Vec<String> {
    hs.params.iter().map(|p| p.name.clone()).collect()
}

fn c_put(kind: &CKind, name: &str, params: &[Param], sub: Option<(usize, &str)>, idx: usize) -> String {
    let value = |i: usize, n: &str| match sub {
        Some((k, v)) if k == i => v.to_string(),
        _ => n.to_string(),
    };
    let len = match kind {
        CKind::Array(_, len) => value(*len, &params[*len].name),
        _ => String::new(),
    };
    c_put_value(kind, &value(idx, name), &len)
}

/// Code that records one argument: the value v (and, for an array, its length n).
fn c_put_value(kind: &CKind, v: &str, n: &str) -> String {
    match kind {
        CKind::Scalar(Scalar::Int { signed: true, .. }) | CKind::Len => format!("fa_rec_i64((long long) ({v}));"),
        CKind::Scalar(Scalar::Int { signed: false, .. }) => format!("fa_rec_u64((unsigned long long) ({v}));"),
        CKind::Scalar(Scalar::Float(_)) => format!("fa_rec_f64((double) ({v}));"),
        CKind::Scalar(Scalar::Bool) => format!("fa_rec_bool((int) ({v}));"),
        CKind::Str => format!("fa_rec_str({v});"),
        CKind::Array(s, _) => match s {
            Scalar::Float(_) => format!("fa_rec_arr_f64({v}, (size_t) ({n}), (int) sizeof(*({v})));"),
            Scalar::Int { signed, .. } => {
                format!("fa_rec_arr_i64({v}, (size_t) ({n}), (int) sizeof(*({v})), {});", i32::from(*signed))
            }
            Scalar::Bool => format!("fa_rec_arr_i64({v}, (size_t) ({n}), (int) sizeof(*({v})), 0);"),
        },
    }
}

fn c_suffix(ret: &str) -> &'static str {
    match scalar(ret) {
        Some(Scalar::Int { signed: false, .. }) => "u64",
        Some(Scalar::Int { .. }) => "i64",
        Some(Scalar::Float(_)) => "f64",
        _ => "bool",
    }
}

fn c_cast(ret: &str) -> &'static str {
    match c_suffix(ret) {
        "u64" => "(unsigned long long)",
        "i64" => "(long long)",
        "f64" => "(double)",
        _ => "(int)",
    }
}

fn c_record_prologue(hs: &Hotspot) -> Result<String> {
    let kinds = c_kinds(&hs.params).map_err(anyhow::Error::msg)?;
    let ret = hs.returns.clone().unwrap_or_default();
    let names = arg_names(hs);
    let puts = |sub: Option<(usize, &str)>| -> String {
        kinds.iter().enumerate().map(|(i, k)| c_put(k, &names[i], &hs.params, sub, i)).collect::<Vec<_>>().join(" ")
    };
    let call = |sub: &[(usize, String)]| -> String {
        let args: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, n)| sub.iter().find(|(k, _)| *k == i).map(|(_, v)| v.clone()).unwrap_or_else(|| n.clone()))
            .collect();
        format!("{}({})", hs.name, args.join(", "))
    };
    let (sfx, cast) = (c_suffix(&ret), c_cast(&ret));
    let mut derive = String::new();
    for (i, k) in kinds.iter().enumerate() {
        let (len_expr, sub_val, len_sub, free) = match k {
            CKind::Str => (format!("fa_rec_chars({})", names[i]), "fa_s".to_string(), None, "fa_rec_free(fa_s);"),
            CKind::Array(_, len) => {
                // an empty slice is (NULL, 0) where the case runs in a child process (see rec.c)
                let ty = hs.params[i].annotation.clone().unwrap_or_default();
                let n = &names[i];
                (
                    format!("(size_t) ({})", names[*len]),
                    format!("(fa_n ? {n} + fa_off : ({ty}) fa_rec_empty({n}))"),
                    Some(*len),
                    "",
                )
            }
            _ => continue,
        };
        let mut subs = vec![(i, sub_val.clone())];
        if let Some(l) = len_sub {
            let lt = hs.params[l].annotation.clone().unwrap_or_else(|| "int".into());
            subs.push((l, format!("({lt}) fa_n")));
        }
        let prep = if matches!(k, CKind::Str) {
            format!("char *fa_s = fa_rec_substr({}, fa_off, fa_n); ", names[i])
        } else {
            String::new()
        };
        // fa_rec_begin + puts with the substitutions (puts takes one substitution: arrays put data + len themselves)
        let put_sub = if matches!(k, CKind::Str) { "fa_s".to_string() } else { sub_val.clone() };
        let mut put_line = String::new();
        for (j, kk) in kinds.iter().enumerate() {
            if *kk == CKind::Len {
                let n = if len_sub == Some(j) { "fa_n" } else { names[j].as_str() };
                put_line.push_str(&format!("fa_rec_i64((long long) ({n})); "));
                continue;
            }
            let p = match kk {
                CKind::Array(s, len) if j == i => {
                    let n = "fa_n";
                    match s {
                        Scalar::Float(_) => {
                            format!("fa_rec_arr_f64({put_sub}, (size_t) {n}, (int) sizeof(*({put_sub})));")
                        }
                        Scalar::Int { signed, .. } => format!(
                            "fa_rec_arr_i64({put_sub}, (size_t) {n}, (int) sizeof(*({put_sub})), {});",
                            i32::from(*signed)
                        ),
                        Scalar::Bool => {
                            format!("fa_rec_arr_i64({put_sub}, (size_t) {n}, (int) sizeof(*({put_sub})), 0);")
                        }
                    }
                    .replace("{len}", &len.to_string())
                }
                _ if j == i => c_put(kk, &names[j], &hs.params, Some((j, &put_sub)), j),
                _ => c_put(kk, &names[j], &hs.params, None, j),
            };
            put_line.push_str(&p);
            put_line.push(' ');
        }
        derive.push_str(&format!(
            "    for (int fa_k = 0; fa_k < 10 && fa_rec_derive_ok(); fa_k++) {{\n        size_t fa_len = {len_expr}, fa_n = (size_t) fa_rec_slices[fa_k];\n        if (fa_len < 2 || fa_n >= fa_len) break;\n        for (int fa_rep = 0; fa_rep < (fa_n < 2 ? 1 : 3); fa_rep++) {{\n            size_t fa_off = fa_rec_rand(fa_len - fa_n + 1);\n            {prep}fa_rec_begin(); {put_line}\n            if (fa_rec_fork(2)) fa_rec_answer_{sfx}({cast} {call});\n            {free}\n        }}\n    }}\n",
            call = call(&subs),
        ));
    }
    let edges = c_edges(hs, &kinds, sfx, cast);
    Ok(format!(
        "if (!fa_rec_inside) {{\n    fa_rec_inside = 1;\n    fa_rec_begin(); {puts}\n    long long fa_t0 = fa_rec_now();\n    {ret} fa_r = {call};\n    fa_rec_done_{sfx}({cast} fa_r, fa_rec_now() - fa_t0);\n{edges}{derive}    fa_rec_inside = 0;\n    return fa_r;\n}}",
        puts = puts(None),
        call = call(&[]),
    ))
}

/// Edge cases for a C function, from the first recorded calls: special numbers, non-ASCII text,
/// empty and much bigger inputs, each varied on its own from short versions of the inputs (text
/// cut to 64 characters, arrays to 8 items). The original answers each one in a child process
/// (see rec.c). An empty array is (NULL, 0), so a function that reads it anyway crashes there and
/// the case is skipped, instead of reading memory that isn't part of the input.
fn c_edges(hs: &Hotspot, kinds: &[CKind], sfx: &str, cast: &str) -> String {
    let names = arg_names(hs);
    let annot = |i: usize| hs.params[i].annotation.clone().unwrap_or_else(|| "int".into());
    // NaN and infinity from builtins, so the app's file needn't include math.h
    let numbers = |s: Scalar| -> Option<&'static str> {
        match s {
            Scalar::Int { signed: true, .. } => Some("0, 1, -1"),
            Scalar::Int { signed: false, .. } => Some("0, 1"),
            Scalar::Float("f32") => {
                Some("0, 1, -1, 1e30, __builtin_nan(\"\"), __builtin_inf(), -__builtin_inf(), -0.0")
            }
            Scalar::Float(_) => Some("0, 1, -1, 1e300, __builtin_nan(\"\"), __builtin_inf(), -__builtin_inf(), -0.0"),
            Scalar::Bool => None,
        }
    };
    let same = |s: Scalar, a: &str, b: &str| match s {
        Scalar::Float(_) => format!("fa_rec_same_f64((double) {a}, (double) {b})"),
        _ => format!("{a} == {b}"),
    };
    let mut base = names.clone();
    let (mut prep, mut free) = (String::new(), String::new());
    for (i, k) in kinds.iter().enumerate() {
        match k {
            CKind::Str => {
                prep.push_str(&format!("char *fa_b{i} = {n} ? fa_rec_substr({n}, 0, 64) : 0;\n        ", n = names[i]));
                free.push_str(&format!("fa_rec_free(fa_b{i}); "));
                base[i] = format!("fa_b{i}");
            }
            CKind::Array(_, l) => {
                let n = &names[*l];
                prep.push_str(&format!("size_t fa_l{l} = (size_t) ({n}) < 8 ? (size_t) ({n}) : 8;\n        "));
                base[*l] = format!("({}) fa_l{l}", annot(*l));
            }
            _ => {}
        }
    }
    // one case: record the arguments, then have the original answer in a child process
    let case = |subs: &[(usize, String)], keep: bool| -> String {
        let mut vals = base.clone();
        for (i, v) in subs {
            vals[*i] = v.clone();
        }
        let puts: Vec<String> = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| match k {
                CKind::Array(_, l) => c_put_value(k, &vals[i], &vals[*l]),
                _ => c_put_value(k, &vals[i], ""),
            })
            .collect();
        format!(
            "fa_rec_begin(); {} if (fa_rec_fork({})) fa_rec_answer_{sfx}({cast} {}({}));",
            puts.join(" "),
            i32::from(keep),
            hs.name,
            vals.join(", ")
        )
    };
    // 4x bigger each step, up to 4096 items or characters, while its time (assumed quadratic) fits
    let grow = "for (int fa_f = 4; fa_f <= 512 && fa_rec_edge_took >= 0 && fa_rec_edge_ok() && fa_rec_edge_took * 16 <= fa_rec_edge_left() / 2; fa_f = fa_f == 256 ? 512 : fa_f * 4)";
    let mut body = String::new();
    for (i, k) in kinds.iter().enumerate() {
        let name = &names[i];
        match k {
            CKind::Scalar(s) => {
                let Some(list) = numbers(*s) else { continue };
                let ty = annot(i);
                body.push_str(&format!(
                    "        {{\n            static const double fa_e[] = {{{list}}};\n            for (int fa_k = 0; fa_k < (int) (sizeof fa_e / sizeof *fa_e) && fa_rec_edge_ok(); fa_k++) {{\n                {ty} fa_v = ({ty}) fa_e[fa_k];\n                if ({}) continue;\n                {}\n            }}\n        }}\n",
                    same(*s, "fa_v", name),
                    case(&[(i, "fa_v".into())], true)
                ));
            }
            CKind::Str => body.push_str(&format!(
                "        if (fa_b{i}) {{\n            for (int fa_k = 0; fa_k < 5 && fa_rec_edge_ok(); fa_k++) {{\n                char *fa_v = fa_rec_str_edge(fa_b{i}, fa_k);\n                if (!fa_v) continue;\n                {}\n                fa_rec_free(fa_v);\n            }}\n            if (fa_rec_grow({i})) {{\n                {}\n                {grow} {{\n                    char *fa_g = fa_rec_str_repeat(fa_b{i}, fa_f);\n                    if (!fa_g) break;\n                    {}\n                    fa_rec_free(fa_g);\n                }}\n            }}\n        }}\n",
                case(&[(i, "fa_v".into())], true),
                case(&[], false),
                case(&[(i, "fa_g".into())], true),
            )),
            CKind::Array(s, l) => {
                let (ty, lt) = (annot(i), annot(*l));
                let elem = squash(&ty).replace("const", "").trim_end_matches('*').trim().to_string();
                body.push_str(&format!(
                    "        if (fa_rec_edge_ok()) {{\n            {}\n        }}\n",
                    case(&[(i, format!("({ty}) 0")), (*l, format!("({lt}) 0"))], true)
                ));
                if let Some(list) = numbers(*s) {
                    body.push_str(&format!(
                        "        if (fa_l{l} > 0) {{\n            static const double fa_e[] = {{{list}}};\n            {elem} *fa_c = ({elem} *) fa_rec_dup({name}, fa_l{l}, sizeof *({name}));\n            for (int fa_k = 0; fa_k < (int) (sizeof fa_e / sizeof *fa_e) && fa_rec_edge_ok(); fa_k++) {{\n                fa_c[0] = ({elem}) fa_e[fa_k];\n                if ({}) continue;\n                {}\n            }}\n            fa_rec_free(fa_c);\n        }}\n",
                        same(*s, "fa_c[0]", &format!("({name})[0]")),
                        case(&[(i, "fa_c".into())], true)
                    ));
                }
                body.push_str(&format!(
                    "        if (fa_l{l} > 0 && fa_rec_grow({i})) {{\n            {}\n            {grow} {{\n                {elem} *fa_g = ({elem} *) fa_rec_repeat({name}, fa_l{l}, sizeof *({name}), fa_f);\n                if (!fa_g) break;\n                {}\n                fa_rec_free(fa_g);\n            }}\n        }}\n",
                    case(&[], false),
                    case(&[(i, "fa_g".into()), (*l, format!("({lt}) (fa_l{l} * (size_t) fa_f)"))], true),
                ));
            }
            CKind::Len => {}
        }
    }
    if body.is_empty() {
        return String::new();
    }
    format!("    if (fa_rec_edge_call()) {{\n        {prep}\n{body}        {free}\n    }}\n")
}

fn cpp_record_prologue(hs: &Hotspot) -> String {
    let names = arg_names(hs);
    format!(
        "if (!fa_rec::st().inside) return fa_rec::call([](const auto &...fa_a) {{ return {}(fa_a...); }}{}{});",
        hs.name,
        if names.is_empty() { "" } else { ", " },
        names.join(", ")
    )
}

pub fn record(ctx: &Ctx, cc: &str, args: &[String], hs: &Hotspot, out: &Path, lang: Lang) -> Result<Recording> {
    let b = Build::parse(args)?;
    let found = describe(ctx, &b, lang)?;
    let d = found
        .iter()
        .filter(|d| d.hs.file == hs.file && d.hs.qualname == hs.qualname)
        .min_by_key(|d| (d.hs.line as i64 - hs.line as i64).abs())
        .with_context(|| format!("{} isn't in {} any more: scan again", hs.qualname, hs.file))?;
    let cpp = lang == Lang::Cpp || is_cpp_file(&hs.file);
    let dir = runtime_dir(ctx)?;
    let (prologue, header, extra) = if cpp {
        (cpp_record_prologue(hs), dir.join("fa_rec.hpp"), vec![])
    } else {
        (c_record_prologue(hs)?, dir.join("fa_rec.h"), vec![runtime_source(&dir, "fa_rec", REC_C, lang)?])
    };
    let text = fs::read_to_string(&d.path)?;
    let meta = out.with_extension("meta.json");
    let mut patches = Patches::new(ctx)?;
    patches
        .write(&d.path, &insert_at(&text, vec![(d.f.body_open + 1, format!(" {}", prologue.replace('\n', " ")))]))?;
    let bin = compile(ctx, cc, &b, &extra, Some(&header));
    patches.restore()?;
    let bin = bin?;
    let env = [("FAST_ANYTHING_RECORD_OUT", out), ("FAST_ANYTHING_RECORD_META", meta.as_path())];
    let _ = fs::remove_file(&meta);
    let mut ran = run(ctx, &bin, &b, &env, true)?;
    if !ran.status.success() {
        // A shrunk input crashed the original (C has no exceptions): record without them.
        crate::ui::warn("the app crashed on a shrunk input; recording real calls only");
        ran = run(ctx, &bin, &b, &env, false)?;
    }
    ensure!(ran.status.success(), "the app failed while recording ({}):\n{}", ran.status, tail(&combined(&ran), 30));
    serde_json::from_str(&fs::read_to_string(&meta).context("nothing was recorded: does main return normally?")?)
        .context("reading the recording summary")
}

// ------------------------------------------------------------------ the crate

pub const FLAVOR_C: Flavor = Flavor {
    lang: Lang::C,
    fence: "c",
    rust_type: rust_type_c,
    default_json: |_| None,
    rules: &[
        "C `int` is `i32`, `long` is `i64` here; unsigned types wrap. Use `wrapping_*` where the original can overflow.",
        "A `const T *` followed by a length arrives as a Vec, and the length parameter still arrives on its own.",
        "`const char *` arrives as a UTF-8 String: `strlen` and `s[i]` are bytes, use `as_bytes()` for the same behavior.",
        "Floats: compilers may fuse `a * b + c` into one FMA instruction (clang does on ARM), which rounds differently. If recorded results differ in the last digits, the original was built that way: use `f64::mul_add` there, or build the app with `-ffp-contract=off`.",
        "`long double` arrives and returns as `f64`: the bridge and the recordings narrow it to `double`.",
        "Returns are numbers or bools. Undefined behavior in the original (overflow of signed ints, out-of-bounds reads) isn't a spec: match the defined cases.",
    ],
    floats: Some(native::FLOATS),
};

pub const FLAVOR_CPP: Flavor = Flavor {
    lang: Lang::Cpp,
    fence: "cpp",
    rust_type: cpp_rust_type,
    default_json: |_| None,
    rules: &[
        "`int` is `i32`, `long long`/`int64_t` is `i64`, `size_t` is `u64`; unsigned types wrap. Use `wrapping_*` where the original can overflow.",
        "`std::string` arrives as a UTF-8 String: `size()` and `s[i]` are bytes, use `as_bytes()` for the same behavior.",
        "`std::vector` is Vec, `std::array<T, N>` is `[T; N]`, `std::map<std::string, T>` is BTreeMap (same key order), `std::pair` is a tuple.",
        "Return `Err(message)` where the original throws.",
    ],
    floats: Some(native::FLOATS),
};

fn flavor(port: &Port) -> &'static Flavor {
    if port.lang == Lang::Cpp { &FLAVOR_CPP } else { &FLAVOR_C }
}

pub fn scaffold(ctx: &Ctx, hs: &Hotspot, helpers: &[Hotspot], port: &Port) -> Result<String> {
    let task = native::scaffold(ctx, hs, helpers, port, flavor(port))?;
    if port.lang == Lang::C {
        fs::write(ctx.crate_dir(port).join("src").join("lib.rs"), glue(hs, port))?;
    }
    Ok(task)
}

/// src/lib.rs: the JSON entry point, plus for C the typed one the bridge calls.
fn glue(hs: &Hotspot, port: &Port) -> String {
    let f = flavor(port);
    let json = native::glue_rs(hs, port, f.default_json, f.floats);
    if port.lang == Lang::C { json + &typed_glue(hs).unwrap_or_default() } else { json }
}

/// A crate whose glue predates what the bridge sends (C's fa_typed, tagged NaN and infinity) or
/// differs otherwise gets it regenerated and rebuilt, so this replay checks what the bridge will
/// call. Its sources changed, so enable asks for one more verify.
pub fn verify(ctx: &Ctx, port: &Port) -> Result<Verify> {
    if let Some(hs) = ctx.state.hotspots.iter().find(|h| h.id == port.id) {
        let lib_rs = ctx.crate_dir(port).join("src").join("lib.rs");
        let glue = glue(hs, port);
        if fs::read_to_string(&lib_rs).is_ok_and(|t| t != glue) {
            fs::write(&lib_rs, &glue)?;
            ui::warn(format!(
                "regenerated {}'s src/lib.rs (generated glue) to match the bridge; verify again before enable",
                port.id
            ));
            if let Err(log) = backend::build(ctx, port)? {
                return Ok(Verify { built: false, build_log: Some(log), ..Verify::default() });
            }
        }
    }
    native::verify(ctx, port, false)
}

// ------------------------------------------------------------------ the typed entry point

/// One parameter of fa_typed, the entry point the C bridge calls (see cfamily/typed.rs).
struct TypedParam {
    /// Its type in fa_typed, and in the bridge's pointer to fa_typed.
    rust: String,
    c: String,
    /// The port's argument, from a{i}.
    port: String,
    /// For verify: how a recorded argument is read, the C value it's held as, how that's passed.
    read: String,
    held: String,
    pass: String,
    /// In the bridge: the value passed, and what runs the C version instead (a NULL string).
    arg: String,
    check: String,
}

/// The Rust type of a `const T *` array's items; None for long double, which keeps JSON.
fn ffi_item(t: &str) -> Option<&'static str> {
    let t = squash(t).replace("std::", "");
    let words: Vec<&str> = t.strip_suffix('*')?.split_whitespace().filter(|w| *w != "const").collect();
    Some(match words.join(" ").as_str() {
        "char" => "std::ffi::c_char",
        "signed char" => "std::ffi::c_schar",
        "unsigned char" => "std::ffi::c_uchar",
        "short" | "short int" | "signed short" => "std::ffi::c_short",
        "unsigned short" | "unsigned short int" => "std::ffi::c_ushort",
        "int" | "signed" | "signed int" => "std::ffi::c_int",
        "unsigned" | "unsigned int" => "std::ffi::c_uint",
        "long" | "long int" => "std::ffi::c_long",
        "unsigned long" | "unsigned long int" => "std::ffi::c_ulong",
        "long long" | "long long int" => "std::ffi::c_longlong",
        "unsigned long long" | "unsigned long long int" => "std::ffi::c_ulonglong",
        "int8_t" => "i8",
        "uint8_t" => "u8",
        "int16_t" => "i16",
        "uint16_t" => "u16",
        "int32_t" => "i32",
        "uint32_t" => "u32",
        "int64_t" => "i64",
        "uint64_t" => "u64",
        "size_t" | "uintptr_t" => "usize",
        "ssize_t" | "ptrdiff_t" | "intptr_t" => "isize",
        "float" => "f32",
        "double" => "f64",
        "bool" | "_Bool" => "bool",
        _ => return None,
    })
}

/// What fa_typed returns through, in Rust and in C.
fn typed_wide(ret: &str) -> (&'static str, &'static str) {
    match c_suffix(ret) {
        "u64" => ("u64", "unsigned long long"),
        "i64" => ("i64", "long long"),
        "f64" => ("f64", "double"),
        _ => ("i32", "int"),
    }
}

/// fa_typed's parameters; None where one keeps JSON (an array of long double).
fn typed_params(hs: &Hotspot) -> Option<Vec<TypedParam>> {
    let kinds = c_kinds(&hs.params).ok()?;
    let mut out = Vec::new();
    for (i, k) in kinds.iter().enumerate() {
        let t = hs.params[i].annotation.clone().unwrap_or_default();
        let n = &hs.params[i].name;
        let int = |signed: bool| TypedParam {
            rust: (if signed { "i64" } else { "u64" }).into(),
            c: (if signed { "long long" } else { "unsigned long long" }).into(),
            port: format!("typed_arg({i}, a{i}.into_deserializer())?"),
            read: (if signed { "Value::as_i64" } else { "Value::as_u64" }).into(),
            held: (if signed { "i64" } else { "u64" }).into(),
            pass: format!("x.{i}"),
            arg: format!("({}) ({n})", if signed { "long long" } else { "unsigned long long" }),
            check: String::new(),
        };
        out.push(match k {
            CKind::Scalar(Scalar::Int { signed, .. }) => int(*signed),
            CKind::Len => int(!matches!(scalar(&t), Some(Scalar::Int { signed: false, .. }))),
            CKind::Scalar(Scalar::Float(_)) => TypedParam {
                rust: "f64".into(),
                c: "double".into(),
                port: format!("typed_arg({i}, a{i}.into_deserializer())?"),
                read: "typed_f64".into(),
                held: "f64".into(),
                pass: format!("x.{i}"),
                arg: format!("(double) ({n})"),
                check: String::new(),
            },
            CKind::Scalar(Scalar::Bool) => TypedParam {
                rust: "i32".into(),
                c: "int".into(),
                port: format!("typed_arg({i}, (a{i} != 0).into_deserializer())?"),
                read: "|v| typed_bool(v).map(i32::from)".into(),
                held: "i32".into(),
                pass: format!("x.{i}"),
                arg: format!("({n}) ? 1 : 0"),
                check: String::new(),
            },
            CKind::Str => TypedParam {
                rust: "*const std::ffi::c_char".into(),
                c: "const char *".into(),
                port: format!("typed_arg({i}, unsafe {{ typed_str({i}, a{i}) }}?.into_deserializer())?"),
                read: "typed_cstring".into(),
                held: "Option<std::ffi::CString>".into(),
                pass: format!("typed_ptr(&x.{i})"),
                arg: n.clone(),
                check: format!("    if (!{n}) return 0;\n"),
            },
            CKind::Array(s, len) => {
                let item = ffi_item(&t)?;
                let (wide, read) = match s {
                    Scalar::Int { signed: true, .. } => (".map(|&x| x as i64)", format!("typed_int::<{item}>")),
                    Scalar::Int { signed: false, .. } => (".map(|&x| x as u64)", format!("typed_uint::<{item}>")),
                    Scalar::Float(_) if item == "f32" => (".map(|&x| f64::from(x))", "typed_f32".into()),
                    Scalar::Float(_) => (".copied()", "typed_f64".into()),
                    Scalar::Bool => (".copied()", "typed_bool".into()),
                };
                TypedParam {
                    rust: format!("*const {item}"),
                    c: t.clone(),
                    port: format!("typed_items({i}, unsafe {{ typed_slice({i}, a{i}, a{len}) }}?.iter(){wide})?"),
                    read: format!("|v| typed_vec(v, {read})"),
                    held: format!("Vec<{item}>"),
                    pass: format!("x.{i}.as_ptr()"),
                    arg: n.clone(),
                    check: String::new(),
                }
            }
        });
    }
    Some(out)
}

/// fa_typed and what verify needs to replay and time it, for src/lib.rs.
fn typed_glue(hs: &Hotspot) -> Option<String> {
    let ps = typed_params(hs)?;
    let (wide, _) = typed_wide(hs.returns.as_deref().unwrap_or(""));
    let list = |f: &dyn Fn(usize, &TypedParam) -> String| -> String {
        ps.iter().enumerate().map(|(i, p)| f(i, p)).collect::<Vec<_>>().join(", ")
    };
    let fits: String = c_kinds(&hs.params)
        .ok()?
        .iter()
        .enumerate()
        .filter_map(|(i, k)| match k {
            CKind::Array(_, len) => Some(format!("    typed_fits({len}, x.{len}, x.{i}.len())?;\n")),
            _ => None,
        })
        .collect();
    Some(format!(
        r#"
// ------------------------------------------------------------------ the C bridge's entry point

use serde::de::IntoDeserializer;

#[allow(dead_code)]
mod typed {{
{template}}}
use typed::*;

type TypedFn = unsafe extern "C" fn(*mut {wide}, {types}) -> i32;

/// The port called with fa_typed's arguments: the C return value, or why there is none.
///
/// # Safety
/// As for fa_typed.
unsafe fn typed({params}) -> Result<{wide}, String> {{
    let result = port::{func}({port_args})?;
    typed_ret(&result)
}}

/// 1 with the result in *out, 0 when the port doesn't answer (the C version runs then).
///
/// # Safety
/// out must be writable, strings NUL-terminated and arrays as long as their length says.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fa_typed(out: *mut {wide}, {params}) -> i32 {{
    match std::panic::catch_unwind(|| unsafe {{ typed({names}) }}) {{
        Ok(Ok(r)) => {{
            unsafe {{ *out = r }};
            1
        }}
        _ => 0,
    }}
}}

/// A recorded call's arguments, held as the C bridge passes them.
struct TypedArgs({held});

fn typed_args(a: &[Value]) -> Result<TypedArgs, String> {{
    let x = TypedArgs({reads});
{fits}    Ok(x)
}}

/// fa_call, through the C values: what verify replays.
///
/// # Safety
/// input must point to len readable bytes and out_len to a writable usize.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fa_call_typed(input: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {{
    let call = |a: &[Value]| {{
        let x = typed_args(a)?;
        unsafe {{ typed({passes}) }}.map(TypedWide::json)
    }};
    unsafe {{ typed_call(input, len, out_len, call) }}
}}

/// Nanoseconds per call of fa_typed, called as the C bridge calls it, over the recorded inputs.
///
/// # Safety
/// input must point to len readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fa_time_typed(input: *const u8, len: usize, budget: f64) -> f64 {{
    let f = std::hint::black_box(fa_typed as TypedFn);
    let run = |x: &TypedArgs| {{
        let mut out = {wide}::default();
        let answered = unsafe {{ f(&mut out, {passes}) }};
        (answered, out)
    }};
    unsafe {{ typed_time(input, len, budget, typed_args, run) }}
}}
"#,
        template = TYPED_RS,
        types = list(&|_, p| p.rust.clone()),
        params = list(&|i, p| format!("a{i}: {}", p.rust)),
        func = native::snake(&hs.name),
        port_args = list(&|_, p| p.port.clone()),
        names = list(&|i, _| format!("a{i}")),
        held = list(&|_, p| p.held.clone()),
        reads = list(&|i, p| format!("typed_json(a, {i}, {})?", p.read)),
        passes = list(&|_, p| p.pass.clone()),
    ))
}

// ------------------------------------------------------------------ the bridge

fn suffix(port: &Port) -> String {
    util::slug(&port.id)
}

fn c_string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn param_list(hs: &Hotspot) -> String {
    hs.params
        .iter()
        .map(|p| format!("{} {}", p.annotation.as_deref().unwrap_or("int"), p.name))
        .collect::<Vec<_>>()
        .join(", ")
}

const LOADER: &str = "#ifdef _WIN32\n#ifndef WIN32_LEAN_AND_MEAN\n#define WIN32_LEAN_AND_MEAN\n#endif\n#ifndef NOMINMAX\n#define NOMINMAX\n#endif\n#include <windows.h>\n#define FA_OPEN(p) ((void *) LoadLibraryA(p))\n#define FA_SYM(l, n) ((void *) GetProcAddress((HMODULE) (l), n))\n#else\n#include <dlfcn.h>\n#define FA_OPEN(p) dlopen(p, RTLD_NOW | RTLD_LOCAL)\n#define FA_SYM(l, n) dlsym(l, n)\n#endif";

/// The C bridge for a function fa_typed takes: its arguments go to the port as they are.
fn c_typed_bridge(hs: &Hotspot, ps: &[TypedParam], s: &str, lib: &str) -> String {
    let ret = hs.returns.clone().unwrap_or_default();
    let (_, wide) = typed_wide(&ret);
    format!(
        r#"/* Calls the verified Rust port of {id}. FAST_ANYTHING_DISABLE=1 runs the C version instead. */
#include <stdlib.h>
{loader}
static int (*fa_typed_{s})({wide} *{types});
int fa_on_{s}(void) {{
    static int state = -1;
    if (state < 0) {{
        void *lib = getenv("FAST_ANYTHING_DISABLE") ? NULL : FA_OPEN({lib});
        state = 0;
        if (lib) {{
            *(void **) &fa_typed_{s} = FA_SYM(lib, "fa_typed");
            state = fa_typed_{s} != NULL;
        }}
    }}
    return state;
}}
/* Sets *fa_ok to 0 (and the C version runs) if the port can't answer. */
{ret} fa_fast_{s}(int *fa_ok, {params}) {{
    {wide} fa_r = 0;
    *fa_ok = 0;
{checks}    *fa_ok = fa_typed_{s}(&fa_r{args});
    return ({ret}) fa_r;
}}"#,
        id = hs.id,
        loader = LOADER,
        types = ps.iter().map(|p| format!(", {}", p.c)).collect::<String>(),
        params = param_list(hs),
        checks = ps.iter().map(|p| p.check.as_str()).collect::<String>(),
        args = ps.iter().map(|p| format!(", {}", p.arg)).collect::<String>(),
    )
}

/// The block appended to the C file: typed (see typed_params), or JSON in and out for an array of
/// long double.
fn c_bridge(hs: &Hotspot, s: &str, lib: &str) -> Result<String> {
    if let Some(ps) = typed_params(hs) {
        return Ok(c_typed_bridge(hs, &ps, s, lib));
    }
    let kinds = c_kinds(&hs.params).map_err(anyhow::Error::msg)?;
    let ret = hs.returns.clone().unwrap_or_default();
    let mut puts = String::new();
    let mut first = true;
    for (i, k) in kinds.iter().enumerate() {
        if *k == CKind::Len && i > 0 && matches!(kinds[i - 1], CKind::Array(..)) && false {
            continue;
        }
        let n = &hs.params[i].name;
        if !first {
            puts.push_str("    fa_put_S(&fa_b, \",\");\n");
        }
        first = false;
        puts.push_str(&match k {
            CKind::Scalar(Scalar::Int { signed: true, .. }) | CKind::Len => format!("    fa_int_S(&fa_b, (long long) ({n}));\n"),
            CKind::Scalar(Scalar::Int { signed: false, .. }) => format!("    fa_uint_S(&fa_b, (unsigned long long) ({n}));\n"),
            CKind::Scalar(Scalar::Float(_)) => format!("    fa_dbl_S(&fa_b, (double) ({n}));\n"),
            CKind::Scalar(Scalar::Bool) => format!("    fa_put_S(&fa_b, ({n}) ? \"true\" : \"false\");\n"),
            CKind::Str => format!("    fa_str_S(&fa_b, {n});\n"),
            CKind::Array(sc, len) => {
                let l = &hs.params[*len].name;
                let item = match sc {
                    Scalar::Float(_) => format!("fa_dbl_S(&fa_b, (double) {n}[fa_i]);"),
                    Scalar::Int { signed: true, .. } => format!("fa_int_S(&fa_b, (long long) {n}[fa_i]);"),
                    Scalar::Int { signed: false, .. } => format!("fa_uint_S(&fa_b, (unsigned long long) {n}[fa_i]);"),
                    Scalar::Bool => format!("fa_put_S(&fa_b, {n}[fa_i] ? \"true\" : \"false\");"),
                };
                format!("    fa_put_S(&fa_b, \"[\");\n    for (size_t fa_i = 0; fa_i < (size_t) ({l}); fa_i++) {{ if (fa_i) fa_put_S(&fa_b, \",\"); {item} }}\n    fa_put_S(&fa_b, \"]\");\n")
            }
        });
    }
    let parse = match scalar(&ret) {
        Some(Scalar::Bool) => "strncmp(fa_reply, \"true\", 4) == 0".to_string(),
        // NaN and infinity come back as {"$float": name}
        Some(Scalar::Float(_)) => format!(
            r#"({ret}) (strncmp(fa_reply, "{{\"$float\":\"", 11) ? strtod(fa_reply, NULL) : fa_reply[11] == 'N' ? NAN : fa_reply[11] == 'I' ? INFINITY : -INFINITY)"#
        ),
        Some(Scalar::Int { signed: false, .. }) => format!("({ret}) strtoull(fa_reply, NULL, 10)"),
        _ => format!("({ret}) strtoll(fa_reply, NULL, 10)"),
    };
    let body = format!(
        r#"/* Calls the verified Rust port of {id}. FAST_ANYTHING_DISABLE=1 runs the C version instead. */
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
{loader}
typedef struct {{ char *p; size_t len, cap; }} fa_buf_S;
static unsigned char *(*fa_call_S)(const unsigned char *, size_t, size_t *);
static void (*fa_free_S)(unsigned char *, size_t);
int fa_on_S(void) {{
    static int state = -1;
    if (state < 0) {{
        void *lib = getenv("FAST_ANYTHING_DISABLE") ? NULL : FA_OPEN({lib});
        state = 0;
        if (lib) {{
            *(void **) &fa_call_S = FA_SYM(lib, "fa_call");
            *(void **) &fa_free_S = FA_SYM(lib, "fa_free");
            state = fa_call_S && fa_free_S;
        }}
    }}
    return state;
}}
static void fa_put_S(fa_buf_S *b, const char *s) {{
    size_t n = strlen(s);
    if (b->len + n + 1 > b->cap) {{ b->cap = (b->len + n + 1) * 2; b->p = (char *) realloc(b->p, b->cap); }}
    memcpy(b->p + b->len, s, n + 1);
    b->len += n;
}}
static void fa_int_S(fa_buf_S *b, long long v) {{ char t[32]; snprintf(t, sizeof t, "%lld", v); fa_put_S(b, t); }}
static void fa_uint_S(fa_buf_S *b, unsigned long long v) {{ char t[32]; snprintf(t, sizeof t, "%llu", v); fa_put_S(b, t); }}
/* JSON has no NaN or infinity: the port's glue reads them as {{"$float": name}} */
static void fa_dbl_S(fa_buf_S *b, double v) {{
    char t[40];
    if (isnan(v)) {{ fa_put_S(b, "{{\"$float\":\"NaN\"}}"); return; }}
    if (isinf(v)) {{ fa_put_S(b, v > 0 ? "{{\"$float\":\"Infinity\"}}" : "{{\"$float\":\"-Infinity\"}}"); return; }}
    snprintf(t, sizeof t, "%.17g", v);
    if (!strpbrk(t, ".eE")) strcat(t, ".0");
    fa_put_S(b, t);
}}
static void fa_str_S(fa_buf_S *b, const char *s) {{
    char t[8] = {{0}};
    if (!s) {{ fa_put_S(b, "null"); return; }}
    fa_put_S(b, "\"");
    for (; *s; s++) {{
        unsigned char c = (unsigned char) *s;
        if (c == '"' || c == '\\') {{ t[0] = '\\'; t[1] = (char) c; t[2] = 0; }}
        else if (c < 0x20) snprintf(t, sizeof t, "\\u%04x", c);
        else {{ t[0] = (char) c; t[1] = 0; }}
        fa_put_S(b, t);
    }}
    fa_put_S(b, "\"");
}}
/* Sets *fa_ok to 0 (and the C version runs) if the port can't answer. */
{ret} fa_fast_S(int *fa_ok, {params}) {{
    fa_buf_S fa_b = {{0}};
    size_t fa_n = 0;
    unsigned char *fa_out;
    char *fa_text;
    const char *fa_reply;
    {ret} fa_r = 0;
    *fa_ok = 0;
    fa_put_S(&fa_b, "[");
{puts}    fa_put_S(&fa_b, "]");
    fa_out = fa_call_S((const unsigned char *) fa_b.p, fa_b.len, &fa_n);
    fa_text = (char *) malloc(fa_n + 1);
    memcpy(fa_text, fa_out, fa_n);
    fa_text[fa_n] = 0;
    fa_free_S(fa_out, fa_n);
    fa_reply = strstr(fa_text, "\"ok\":");
    if (fa_reply) {{
        fa_reply += 5;
        fa_r = {parse};
        *fa_ok = 1;
    }}
    free(fa_text);
    free(fa_b.p);
    return fa_r;
}}"#,
        id = hs.id,
        loader = LOADER,
        params = param_list(hs),
    );
    Ok(body.replace("_S", &format!("_{s}")))
}

fn cpp_bridge(hs: &Hotspot, s: &str, lib: &str, namespaces: &[String]) -> String {
    let ret = hs.returns.clone().unwrap_or_default();
    let open: String = namespaces
        .iter()
        .map(|n| if n.is_empty() { "namespace {\n".to_string() } else { format!("namespace {n} {{\n") })
        .collect();
    let close: String = namespaces.iter().map(|_| "}\n").collect();
    let body = format!(
        r#"// Calls the verified Rust port of {id}. FAST_ANYTHING_DISABLE=1 runs the C++ version instead.
#include <algorithm>
#include <array>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <map>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <unordered_map>
#include <utility>
#include <vector>
{loader}
namespace fa_bridge_S {{
{json}
struct Lib {{
    unsigned char *(*call)(const unsigned char *, size_t, size_t *) = nullptr;
    void (*free)(unsigned char *, size_t) = nullptr;
    bool on = false;
    Lib() {{
        if (std::getenv("FAST_ANYTHING_DISABLE")) return;
        void *lib = FA_OPEN({lib});
        if (!lib) return;
        *(void **) &call = FA_SYM(lib, "fa_call");
        *(void **) &free = FA_SYM(lib, "fa_free");
        on = call && free;
    }}
}};
inline Lib &lib() {{ static Lib l; return l; }}
}} // namespace fa_bridge_S
{open}bool fa_on_S() {{ return fa_bridge_S::lib().on; }}
bool fa_fast_S({ret} &fa_out, {params}) {{
    std::string fa_in = "[";
    if (!fa_bridge_S::enc_all(fa_in, {args})) return false;
    fa_in += "]";
    size_t fa_n = 0;
    unsigned char *fa_raw = fa_bridge_S::lib().call((const unsigned char *) fa_in.data(), fa_in.size(), &fa_n);
    std::string fa_text((const char *) fa_raw, fa_n);
    fa_bridge_S::lib().free(fa_raw, fa_n);
    const char *fa_p = fa_text.c_str();
    fa_bridge_S::V fa_reply = fa_bridge_S::parse(fa_p);
    if (const fa_bridge_S::V *fa_err = fa_reply.get("error")) throw std::runtime_error("fast-anything: " + fa_err->s);
    const fa_bridge_S::V *fa_value = fa_reply.get("ok");
    if (!fa_value) return false;
    fa_bridge_S::from(*fa_value, fa_out);
    return true;
}}
{close}"#,
        id = hs.id,
        loader = LOADER,
        json = JSON_HPP,
        params = param_list(hs),
        args = arg_names(hs).join(", "),
    );
    body.replace("_S", &format!("_{s}"))
}

fn find_body(name: &str, hint: u32) -> impl Fn(&str) -> Option<usize> {
    let call = format!("{name}(");
    let spaced = format!("{name} (");
    move |text: &str| {
        native::body_line(text, hint, |l| (l.contains(&call) || l.contains(&spaced)) && !l.trim_end().ends_with(';'))
    }
}

pub fn enable(ctx: &Ctx, port: &Port) -> Result<()> {
    let hs = ctx
        .state
        .hotspots
        .iter()
        .find(|h| h.id == port.id)
        .with_context(|| format!("{} isn't in the last scan", port.id))?;
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    let cpp = port.lang == Lang::Cpp || is_cpp_file(&port.file);
    let s = suffix(port);
    let (_, lib) = port.lang.artifact(&port.ext);
    let lib = c_string_literal(&util::forward_slashes(&ctx.lib_dir().join(lib)));
    let ret = hs.returns.clone().unwrap_or_default();
    let args = arg_names(hs).join(", ");
    let (prologue, bridge) = if cpp {
        let namespaces = parse(&text)
            .into_iter()
            .find(|f| f.name == hs.name && f.class == hs.owner)
            .map(|f| f.namespaces)
            .unwrap_or_default();
        (
            format!(
                "bool fa_on_{s}();\nbool fa_fast_{s}({ret} &, {params});\nif (fa_on_{s}()) {{\n    {ret} fa_r{{}};\n    if (fa_fast_{s}(fa_r, {args})) return fa_r;\n}}",
                params = param_list(hs)
            ),
            cpp_bridge(hs, &s, &lib, &namespaces),
        )
    } else {
        (
            format!(
                "int fa_on_{s}(void);\n{ret} fa_fast_{s}(int *, {params});\nif (fa_on_{s}()) {{\n    int fa_ok;\n    {ret} fa_r = fa_fast_{s}(&fa_ok, {args});\n    if (fa_ok) return fa_r;\n}}",
                params = param_list(hs)
            ),
            c_bridge(hs, &s, &lib)?,
        )
    };
    let patched = native::insert_in_body(&text, "//", &port.id, &prologue, find_body(&port.func, port.line.max(1)))?;
    let patched = backend::append_marked(&patched, "//", &format!("{} bridge", port.id), &bridge);
    fs::write(&file, patched).with_context(|| format!("writing {}", port.file))
}

pub fn disable(ctx: &Ctx, port: &Port) -> Result<()> {
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    let text = native::strip_in_body(&text, "//", &port.id);
    let text = backend::strip_marked(&text, "//", &format!("{} bridge", port.id));
    fs::write(&file, text).with_context(|| format!("writing {}", port.file))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compiles C++ `source` with the recording code of its function `name` and runs it; returns
    /// the recordings, or None without a C++ compiler.
    #[cfg(unix)]
    fn record_cpp(source: &str, name: &str) -> Option<Vec<Value>> {
        let cc = ["c++", "g++", "clang++"].into_iter().find(|c| Command::new(c).arg("--version").output().is_ok())?;
        let f = parse(source).into_iter().find(|f| f.name == name).unwrap();
        let hs = Hotspot {
            name: f.name.clone(),
            params: f.params.clone(),
            returns: Some(f.ret.clone()),
            ..Hotspot::default()
        };
        let dir = std::env::temp_dir().join(format!("fa-cpp-rec-test-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("fa_rec.hpp"), REC_HPP.replace("/*JSON*/", JSON_HPP)).unwrap();
        let prologue = cpp_record_prologue(&hs).replace('\n', " ");
        fs::write(dir.join("app.cpp"), insert_at(source, vec![(f.body_open + 1, format!(" {prologue}"))])).unwrap();
        let built = Command::new(cc)
            .args(["-std=c++17", "-O1", "-include", "fa_rec.hpp", "app.cpp", "-o", "app"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        let ran = Command::new(dir.join("app"))
            .current_dir(&dir)
            .env("FAST_ANYTHING_RECORD_OUT", dir.join("rec.jsonl"))
            .env("FAST_ANYTHING_RECORD_META", dir.join("meta.json"))
            .output()
            .unwrap();
        let text = fs::read_to_string(dir.join("rec.jsonl")).unwrap_or_default();
        let _ = fs::remove_dir_all(&dir);
        assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
        Some(text.lines().map(|l| serde_json::from_str(l).unwrap()).collect())
    }

    #[cfg(unix)]
    #[test]
    fn cpp_edge_cases_that_hang_or_crash_are_skipped() {
        use serde_json::json;
        // never returns for 0 or -1
        let spin = "long spin(long n) {\n    long steps = 0;\n    while (n != 1) {\n        n = n % 2 ? 3 * n + 1 : n / 2;\n        steps++;\n    }\n    return steps;\n}\nint main() {\n    long t = 0;\n    for (long k = 2; k < 30; k++) t += spin(k);\n    return t > 0 ? 0 : 1;\n}\n";
        let started = std::time::Instant::now();
        let Some(cases) = record_cpp(spin, "spin") else {
            eprintln!("no C++ compiler on PATH, skipping");
            return;
        };
        assert!(started.elapsed().as_secs() < 30, "the recording finished");
        let edges: Vec<&Value> = cases.iter().filter(|c| c["derived"] == "edge").collect();
        assert!(edges.iter().any(|c| c["args"] == json!([1]) && c["result"] == json!(0)), "{edges:?}");
        assert!(edges.iter().all(|c| c["args"] != json!([0]) && c["args"] != json!([-1])), "{edges:?}");
        // reads xs[0] whatever the size: crashes on an empty vector
        let first = "#include <vector>\nint first(const std::vector<int> &xs) {\n    return ((volatile const int *) xs.data())[0] * 0 + (int) xs.size();\n}\nint main() {\n    std::vector<int> xs(20, 1);\n    int t = 0;\n    for (int k = 0; k < 3; k++) t += first(xs);\n    return t > 0 ? 0 : 1;\n}\n";
        let cases = record_cpp(first, "first").unwrap();
        let edges: Vec<&Value> = cases.iter().filter(|c| c["derived"] == "edge").collect();
        assert!(!edges.is_empty() && edges.iter().all(|c| c["args"] != json!([[]])), "{edges:?}");
    }

    /// Compiles `source` with the recording prologue of its function `name` and runs it; returns
    /// the recordings, or None without a C compiler.
    fn record_c(source: &str, name: &str) -> Option<Vec<Value>> {
        let cc = ["cc", "gcc", "clang"].into_iter().find(|c| Command::new(c).arg("--version").output().is_ok())?;
        let f = parse(source).into_iter().find(|f| f.name == name).unwrap();
        let hs = Hotspot {
            name: f.name.clone(),
            params: f.params.clone(),
            returns: Some(f.ret.clone()),
            ..Hotspot::default()
        };
        let dir = std::env::temp_dir().join(format!("fa-c-rec-test-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for (file, text) in [("fa_clock.h", CLOCK_H), ("fa_rec.h", REC_H), ("fa_rec.c", REC_C)] {
            fs::write(dir.join(file), text).unwrap();
        }
        let prologue = c_record_prologue(&hs).unwrap().replace('\n', " ");
        fs::write(dir.join("app.c"), insert_at(source, vec![(f.body_open + 1, format!(" {prologue}"))])).unwrap();
        let built = Command::new(cc)
            .args(["-O1", "-include", "fa_rec.h", "app.c", "fa_rec.c", "-o", "app"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        let ran = Command::new(dir.join("app"))
            .current_dir(&dir)
            .env("FAST_ANYTHING_RECORD_OUT", dir.join("rec.jsonl"))
            .env("FAST_ANYTHING_RECORD_META", dir.join("meta.json"))
            .output()
            .unwrap();
        let text = fs::read_to_string(dir.join("rec.jsonl")).unwrap_or_default();
        let _ = fs::remove_dir_all(&dir);
        assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
        Some(text.lines().map(|l| serde_json::from_str(l).unwrap()).collect())
    }

    #[cfg(unix)]
    #[test]
    fn c_edge_cases_run_where_a_crash_or_hang_is_contained() {
        use serde_json::json;
        // score crashes on an emoji, never returns for k == -1 and reads xs[0] even when n is 0
        let source = "#include <stddef.h>\n#include <string.h>\nlong score(const char *s, const int *xs, size_t n, int k)\n{\n    if (strstr(s, \"\\xF0\\x9F\\x91\\x8D\")) { volatile int *p = 0; return *p; }\n    if (k == -1) for (;;) {}\n    long t = (long) strlen(s) + k + ((volatile const int *) xs)[0] * 0;\n    for (size_t i = 0; i < n; i++) t += xs[i];\n    return t;\n}\nint main(void) {\n    char s[201] = {0};\n    for (int i = 0; i < 40; i++) strcat(s, \"word \");\n    int xs[24];\n    for (int i = 0; i < 24; i++) xs[i] = i;\n    long t = 0;\n    for (int k = 0; k < 3; k++) t += score(s, xs, 20 + k, k);\n    return t > 0 ? 0 : 1;\n}\n";
        let Some(cases) = record_c(source, "score") else {
            eprintln!("no C compiler on PATH, skipping");
            return;
        };
        let edges: Vec<&Value> = cases.iter().filter(|c| c["derived"] == "edge").collect();
        let has = |i: usize, want: Value| edges.iter().any(|c| c["args"][i] == want);
        let short: String = "word ".repeat(40).chars().take(64).collect();
        assert!(has(0, json!("")) && has(0, json!(format!("{short}\u{65e5}\u{672c}"))), "text: {edges:?}");
        assert!(has(3, json!(1)), "special numbers: {edges:?}");
        assert!(edges.iter().any(|c| c["args"][1].as_array().is_some_and(|a| a.len() == 8 * 16)), "bigger arrays");
        assert!(edges.iter().all(|c| c["args"][3] != json!(-1)), "a case that never returns");
        assert!(!cases.iter().any(|c| c["args"][0].as_str().is_some_and(|s| s.contains('\u{1f44d}'))), "a crash");
        assert!(!cases.iter().any(|c| c["args"][1] == json!([])), "an empty array the original reads anyway");
        for c in &cases {
            let a = c["args"].as_array().unwrap();
            let xs = a[1].as_array().unwrap();
            assert_eq!(xs.len() as u64, a[2].as_u64().unwrap(), "{c}");
            let want = a[0].as_str().unwrap().len() as i64
                + a[3].as_i64().unwrap()
                + xs.iter().map(|x| x.as_i64().unwrap()).sum::<i64>();
            assert_eq!(c["result"].as_i64(), Some(want), "{c}");
        }
    }

    #[test]
    fn c_recordings_carry_every_parameter() {
        // the port's glue and the bridge take the array's length as an argument of its own
        let source = "#include <stddef.h>\nlong total(const int *xs, size_t n, int scale)\n{\n    long s = 0;\n    for (size_t i = 0; i < n; i++) s += xs[i] * scale;\n    return s;\n}\nint main(void) {\n    int xs[40];\n    for (int i = 0; i < 40; i++) xs[i] = i;\n    long t = 0;\n    for (int k = 0; k < 5; k++) t += total(xs, 30 + k, k);\n    return t > 0 ? 0 : 1;\n}\n";
        let Some(cases) = record_c(source, "total") else {
            eprintln!("no C compiler on PATH, skipping");
            return;
        };
        assert!(cases.iter().any(|c| c.get("derived").is_some()), "no derived cases: {cases:?}");
        for c in &cases {
            let args = c["args"].as_array().unwrap();
            assert_eq!(args.len(), 3, "{c}");
            let (xs, n, scale) = (args[0].as_array().unwrap(), args[1].as_u64().unwrap(), args[2].as_i64().unwrap());
            assert_eq!(xs.len() as u64, n, "{c}");
            let sum: i64 = xs.iter().map(|x| x.as_i64().unwrap() * scale).sum();
            assert_eq!(c["result"].as_i64(), Some(sum), "{c}");
        }
    }

    #[test]
    fn bridges_compile_whatever_the_parameters_are_called() {
        // every name the bridges use for their own variables, as a parameter; the typed C bridge
        // without a warning, and the JSON one, for an array of long double
        let c = "long pick(const int *b, size_t n, const char *text, double out, int ok, long r, int fallback, const float *state, unsigned lib) {\n    return 0;\n}\n";
        let c_json =
            "long double pick(const long double *b, int n, const char *text, float reply) {\n    return 0;\n}\n";
        let cpp = "long pick(const std::vector<int> &in, int n, const std::string &text, double p, int reply, int err, int ok, long out) {\n    return 0;\n}\n";
        let (c_compilers, cpp_compilers) = (["cc", "gcc", "clang"], ["c++", "g++", "clang++"]);
        for (source, compilers, ext) in [(c, c_compilers, "c"), (c_json, c_compilers, "c"), (cpp, cpp_compilers, "cpp")]
        {
            let Some(cc) = compilers.into_iter().find(|c| Command::new(c).arg("--version").output().is_ok()) else {
                eprintln!("no {ext} compiler on PATH, skipping");
                continue;
            };
            let f = parse(source).remove(0);
            let hs = Hotspot {
                id: "lib.pick".into(),
                name: f.name.clone(),
                params: f.params.clone(),
                returns: Some(f.ret.clone()),
                ..Hotspot::default()
            };
            let bridge = if ext == "c" {
                c_bridge(&hs, "t", "\"lib.so\"").unwrap()
            } else {
                cpp_bridge(&hs, "t", "\"lib.so\"", &[])
            };
            assert!(ext == "cpp" || bridge.contains("\"fa_call\"") == (source == c_json), "{bridge}");
            let dir = std::env::temp_dir().join(format!("fa-bridge-names-{ext}-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("bridge.{ext}")), bridge).unwrap();
            let mut cmd = Command::new(cc);
            if ext == "cpp" {
                cmd.arg("-std=c++17");
            }
            if source == c {
                cmd.args(["-Wall", "-Wextra", "-Werror"]);
            }
            let out = cmd.args(["-c", &format!("bridge.{ext}"), "-o", "bridge.o"]).current_dir(&dir).output().unwrap();
            let _ = fs::remove_dir_all(&dir);
            assert!(out.status.success(), "{ext}: {}", String::from_utf8_lossy(&out.stderr));
        }
    }

    #[test]
    fn duration_ends_an_app_that_runs_until_closed() {
        let Some(cc) = ["cc", "gcc", "clang"].into_iter().find(|c| Command::new(c).arg("--version").output().is_ok())
        else {
            eprintln!("no C compiler on PATH, skipping");
            return;
        };
        let tmp = std::env::temp_dir();
        // macOS's temp folder is behind a symlink; the profile has the real paths
        let tmp = if cfg!(windows) { tmp } else { tmp.canonicalize().unwrap() };
        let dir = tmp.join(format!("fa-c-duration-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // a server's shape: work, forever
        let main = "long work(long n) {\n    long s = 0;\n    for (long i = 0; i < n; i++) s += i * i % 7;\n    return s;\n}\n\nint main(void) {\n    volatile long sink = 0;\n    for (;;) sink += work(20000000);\n}\n";
        fs::write(dir.join("main.c"), main).unwrap();
        let ctx = Ctx::open(&dir).unwrap();
        let p = profile(&ctx, cc, &["-O1".into(), "main.c".into()], Lang::C, Some(1.0));
        let after = fs::read_to_string(dir.join("main.c")).unwrap();
        let _ = fs::remove_dir_all(&dir);
        let p = p.unwrap();
        assert!(p.functions.iter().any(|f| f.name == "work" && f.ncalls > 0), "{:?}", p.functions);
        assert!((0.9..10.0).contains(&p.wall), "{}", p.wall);
        assert_eq!(after, main);
    }

    #[test]
    fn finds_c_and_cpp_functions() {
        let c = "#include <stdio.h>\n/* a { comment */\nstatic int lev(const char *a, const char *b) {\n  return 0;\n}\n\nlong count(const int *xs, size_t n, int max)\n{\n  if (n) { return 1; }\n  printf(\"{\");\n  return 0;\n}\nint main(void) { return lev(\"a\", \"b\"); }\n";
        assert_eq!(detect("icpx"), Some(Lang::Cpp));
        assert_eq!(detect("C:\\oneAPI\\bin\\dpcpp.exe"), Some(Lang::Cpp));
        assert_eq!(detect("icx"), Some(Lang::C));
        assert_eq!(detect("icx-cl"), None);
        let f = parse(c);
        assert_eq!(f.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["lev", "count", "main"]);
        assert_eq!(f[0].ret, "int");
        assert!(f[0].is_static);
        assert_eq!(
            c_kinds(&f[1].params).unwrap(),
            vec![CKind::Array(scalar("int").unwrap(), 1), CKind::Len, CKind::Scalar(scalar("int").unwrap())]
        );
        let cpp = "namespace text {\nclass Fuzzy {\npublic:\n  static std::vector<std::pair<int, int>> find(const std::vector<std::string> &names, int max = 2) {\n    return {};\n  }\n  int size() const { return 0; }\n};\n}\n";
        let f = parse(cpp);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].class.as_deref(), Some("Fuzzy"));
        assert_eq!(f[0].namespaces, ["text"]);
        assert_eq!(f[0].params[1].default.as_deref(), Some("2"));
        assert_eq!(cpp_rust_type(&f[0].ret).as_deref(), Some("Vec<(i32, i32)>"));
        assert_eq!(cpp_rust_type("const std::vector<std::string>&").as_deref(), Some("Vec<String>"));
        assert_eq!(detect("clang++"), Some(Lang::Cpp));
        assert_eq!(detect("/usr/bin/gcc-14"), Some(Lang::C));
        assert_eq!(detect("clang-cl"), None.or(detect("clang-cl")));
        let b =
            Build::parse(&["-O2".into(), "main.c".into(), "-o".into(), "app".into(), "--".into(), "x".into()]).unwrap();
        assert_eq!(
            (b.flags, b.sources, b.args),
            (vec!["-O2".to_string()], vec!["main.c".to_string()], vec!["x".to_string()])
        );
    }

    /// How the tests build C and C++: without fusing `a * b + c` into one FMA instruction, which clang does
    /// by default on ARM (macOS) and which rounds differently from the ports written here.
    #[cfg(unix)]
    const C_FLAGS: [&str; 2] = ["-O2", "-ffp-contract=off"];

    /// A C or C++ project in a temp folder: scanned, each (id, port.rs) ported and built, then all
    /// enabled. None without a compiler or cargo.
    #[cfg(unix)]
    fn enabled_c_project(tag: &str, files: &[(&str, &str)], ports: &[(&str, &str)]) -> Option<(PathBuf, &'static str)> {
        let cpp = files.iter().any(|f| is_cpp_file(f.0));
        let compilers = if cpp { ["c++", "g++", "clang++"] } else { ["cc", "gcc", "clang"] };
        let cc = compilers.into_iter().find(|c| Command::new(c).arg("--version").output().is_ok())?;
        Command::new("cargo").arg("--version").output().ok()?;
        let dir = std::env::temp_dir().canonicalize().unwrap().join(format!("fa-c-typed-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            fs::write(dir.join(name), text).unwrap();
        }
        let flags: Vec<&str> = C_FLAGS.into_iter().chain(cpp.then_some("-std=c++17")).collect();
        let command = [cc].iter().chain(&flags).chain(files.iter().map(|f| &f.0)).map(|s| s.to_string()).collect();
        crate::pipeline::scan(&dir, command, None).unwrap();
        for (id, port_rs) in ports {
            let out = crate::pipeline::port(&dir, Some(id), 200).unwrap();
            fs::write(&out.lib_rs, port_rs).unwrap();
            let ctx = Ctx::open(&dir).unwrap();
            let built = backend::build(&ctx, &ctx.state.ports[*id]).unwrap();
            assert!(built.is_ok(), "{id}: {built:?}");
        }
        for (id, _) in ports {
            crate::pipeline::enable(&dir, Some(id), true).unwrap();
        }
        Some((dir, cc))
    }

    /// Builds the sources in dir and runs them; FAST_ANYTHING_DISABLE=1 with `disable`.
    #[cfg(unix)]
    fn run_c(dir: &Path, cc: &str, sources: &[&str], disable: bool) -> String {
        let std: &[&str] = if sources.iter().any(|s| is_cpp_file(s)) { &["-std=c++17"] } else { &[] };
        let built = Command::new(cc)
            .args(C_FLAGS)
            .args(std)
            .args(sources)
            .args(["-o", "app"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        let mut cmd = Command::new(dir.join("app"));
        if disable {
            cmd.env(DISABLE_ENV, "1");
        }
        let ran = cmd.current_dir(dir).output().unwrap();
        assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
        String::from_utf8_lossy(&ran.stdout).into_owned()
    }

    /// verify's replay of a built port, in this process.
    #[cfg(unix)]
    fn replay(dir: &Path, id: &str) -> Verify {
        replay_lib(dir, id, None)
    }

    /// The replay, loading the port from `lib` if given (a library loaded once stays mapped on macOS,
    /// so a rebuilt port at the same path would be the old one).
    #[cfg(unix)]
    fn replay_lib(dir: &Path, id: &str, lib_path: Option<PathBuf>) -> Verify {
        let ctx = Ctx::open(dir).unwrap();
        let port = &ctx.state.ports[id];
        let crate_dir = ctx.crate_dir(port);
        let (_, lib) = port.lang.artifact(&port.ext);
        let job = serde_json::json!({
            "lib": lib_path.unwrap_or_else(|| ctx.lib_dir().join(lib)), "cases": crate_dir.join("recordings.jsonl"),
            "meta": crate_dir.join("recordings.meta.json"), "func": port.func, "out": crate_dir.join("verify.json"),
        });
        fs::write(crate_dir.join("job-verify.json"), job.to_string()).unwrap();
        native::replay(&crate_dir.join("job-verify.json")).unwrap();
        serde_json::from_str(&fs::read_to_string(crate_dir.join("verify.json")).unwrap()).unwrap()
    }

    /// The calls recorded for a port, derived ones included.
    #[cfg(unix)]
    fn recorded(dir: &Path, id: &str) -> Vec<Value> {
        let ctx = Ctx::open(dir).unwrap();
        let text = fs::read_to_string(ctx.crate_dir(&ctx.state.ports[id]).join("recordings.jsonl")).unwrap();
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// verify's replay of the port rebuilt from another port.rs.
    #[cfg(unix)]
    fn replay_other(dir: &Path, id: &str, port_rs: &str) -> Verify {
        let ctx = Ctx::open(dir).unwrap();
        let port = &ctx.state.ports[id];
        fs::write(ctx.crate_dir(port).join("src").join("port.rs"), port_rs).unwrap();
        let built = backend::build(&ctx, port).unwrap();
        assert!(built.is_ok(), "{id}: {built:?}");
        let (_, lib) = port.lang.artifact(&port.ext);
        let fresh = ctx.lib_dir().join(format!("other-{}-{lib}", std::process::id()));
        fs::copy(ctx.lib_dir().join(lib), &fresh).unwrap();
        replay_lib(dir, id, Some(fresh))
    }

    /// {"$float": name}, how NaN and infinity are recorded.
    #[cfg(unix)]
    fn tagged(name: &str) -> Value {
        serde_json::json!({ "$float": name })
    }

    #[cfg(unix)]
    #[test]
    fn nan_and_infinity_reach_the_c_app_as_they_are() {
        let ranges = "#include <stddef.h>\n/* Mean of [lo, hi) over w: NaN for an empty range (0/0), infinite for w == 0. */\ndouble weighted_mean(long long lo, long long hi, double w) {\n    double s = 0.0;\n    for (long long i = lo; i < hi; i++) s += (double) i;\n    return s / (double) (hi - lo) / w;\n}\n/* Sum of xs: NaN or infinite when one of them is. */\ndouble total(const double *xs, size_t n) {\n    double s = 0.0;\n    for (size_t i = 0; i < n; i++) s += xs[i];\n    return s;\n}\n";
        let main = "#include <math.h>\n#include <stddef.h>\n#include <stdio.h>\ndouble weighted_mean(long long lo, long long hi, double w);\ndouble total(const double *xs, size_t n);\nint main(void) {\n    double sum = 0.0, ms[8];\n    int empty = 0, infinite = 0, odd = 0;\n    for (int k = 0; k < 4000; k++) {\n        long long lo = k * 37 % 1000, hi = lo + (k % 50) * 20;\n        double m = weighted_mean(lo, hi, (double) (k % 23));\n        if (isnan(m)) empty++;\n        else if (isinf(m)) infinite++;\n        else sum += m;\n        ms[k % 8] = m;\n        if (k % 8 == 7) {\n            double t = total(ms, 8);\n            if (isfinite(t)) sum += t;\n            else odd++;\n        }\n    }\n    printf(\"total %.3f, empty %d, infinite %d, sums not finite %d\\n\", sum, empty, infinite, odd);\n    return 0;\n}\n";
        let mean = "pub fn weighted_mean(lo: i64, hi: i64, w: f64) -> Result<f64, String> {\n    let n = hi - lo;\n    let s = if n > 0 { ((lo + hi - 1) * n / 2) as f64 } else { 0.0 };\n    Ok(s / n as f64 / w)\n}\n";
        let total = "pub fn total(xs: Vec<f64>, _n: u64) -> Result<f64, String> {\n    Ok(xs.iter().fold(0.0, |s, x| s + x))\n}\n";
        let ids = ["ranges.weighted_mean", "ranges.total"];
        let files = [("ranges.c", ranges), ("main.c", main)];
        let Some((dir, cc)) = enabled_c_project("nan", &files, &[(ids[0], mean), (ids[1], total)]) else {
            eprintln!("no C compiler or cargo on PATH, skipping");
            return;
        };
        // recorded as they are, results and arguments (in arrays too), and NaN and infinity are edge cases
        let (means, totals) = (recorded(&dir, ids[0]), recorded(&dir, ids[1]));
        let real = |c: &&Value| c.get("derived").is_none();
        let edge = |c: &&Value| c["derived"] == "edge";
        let verified = (replay(&dir, ids[0]), replay(&dir, ids[1]));
        // the port answers NaN and infinity itself, not the C version, and takes them too
        let check = "#include <math.h>\n#include <stddef.h>\n#include <stdio.h>\nint fa_on_W(void);\nint fa_on_T(void);\ndouble fa_fast_W(int *, long long lo, long long hi, double w);\ndouble fa_fast_T(int *, const double *xs, size_t n);\nint main(void) {\n    int on = fa_on_W() + fa_on_T(), ok[5];\n    double xs[3] = {1.5, NAN, 2.0}, ys[2] = {-INFINITY, 1.0};\n    double a = fa_fast_W(&ok[0], 5, 5, 1.0), b = fa_fast_W(&ok[1], 1, 4, 0.0), c = fa_fast_W(&ok[2], 1, 4, NAN);\n    double d = fa_fast_T(&ok[3], xs, 3), e = fa_fast_T(&ok[4], ys, 2);\n    printf(\"%d %d %d %d %d %d\", on, isnan(a) != 0, isinf(b) != 0, isnan(c) != 0, isnan(d) != 0, isinf(e) && e < 0);\n    for (int i = 0; i < 5; i++) printf(\" %d\", ok[i]);\n    printf(\"\\n\");\n    return 0;\n}\n";
        let check = check
            .replace("_W(", &format!("_{}(", util::slug(ids[0])))
            .replace("_T(", &format!("_{}(", util::slug(ids[1])));
        fs::write(dir.join("check.c"), check).unwrap();
        let original = run_c(&dir, cc, &["main.c", "ranges.c"], true);
        let enabled = run_c(&dir, cc, &["main.c", "ranges.c"], false);
        let checked = run_c(&dir, cc, &["check.c", "ranges.c"], false);
        // 0.0 for an empty range, where the original returns NaN: verify passed it while NaN wasn't recorded
        let wrong = mean.replace("Ok(s / n as f64 / w)", "Ok(if n == 0 { 0.0 } else { s / n as f64 / w })");
        let caught = replay_other(&dir, ids[0], &wrong);
        let _ = fs::remove_dir_all(&dir);
        assert!(means.iter().filter(real).any(|c| c["result"] == tagged("NaN")), "{means:?}");
        assert!(means.iter().filter(real).any(|c| c["result"] == tagged("Infinity")), "{means:?}");
        for name in ["NaN", "Infinity", "-Infinity"] {
            assert!(means.iter().filter(edge).any(|c| c["args"][2] == tagged(name)), "{name}: {means:?}");
            assert!(totals.iter().filter(edge).any(|c| c["args"][0][0] == tagged(name)), "{name}: {totals:?}");
        }
        let negative_zero = |v: &Value| v.as_f64().is_some_and(|x| x == 0.0 && x.is_sign_negative());
        assert!(means.iter().filter(edge).any(|c| negative_zero(&c["args"][2])), "{means:?}");
        let holds = |c: &Value, name: &str| c["args"][0].as_array().unwrap().contains(&tagged(name));
        assert!(totals.iter().filter(real).any(|c| holds(c, "NaN") && c["result"] == tagged("NaN")), "{totals:?}");
        assert!(totals.iter().filter(real).any(|c| holds(c, "Infinity") && c["result"] == tagged("Infinity")));
        for v in [&verified.0, &verified.1] {
            assert!(v.total > 0 && v.passed == v.total, "{v:?}");
        }
        assert!(caught.failed > 0 && caught.failures.iter().any(|f| f.expected == r#"{"$float":"NaN"}"#), "{caught:?}");
        assert!(original.contains("empty 80, infinite 170"), "{original}");
        assert_eq!(enabled, original);
        assert_eq!(checked, "2 1 1 1 1 1 1 1 1 1 1\n");
    }

    #[cfg(unix)]
    #[test]
    fn nan_and_infinity_reach_the_cpp_app_as_they_are() {
        let ranges = "#include <limits>\n#include <utility>\n#include <vector>\n// Mean of xs over w (NaN when xs is empty, infinite for w == 0) and the smallest of xs.\nstd::pair<double, double> stats(const std::vector<double> &xs, double w) {\n    double s = 0.0, low = std::numeric_limits<double>::infinity();\n    for (double x : xs) {\n        s += x;\n        if (x < low) low = x;\n    }\n    return {s / (double) xs.size() / w, low};\n}\n";
        let main = "#include <cmath>\n#include <cstdio>\n#include <utility>\n#include <vector>\nstd::pair<double, double> stats(const std::vector<double> &xs, double w);\nint main() {\n    double total = 0.0;\n    int empty = 0, infinite = 0;\n    for (int k = 0; k < 4000; k++) {\n        std::vector<double> xs;\n        for (int i = 0; i < (k % 50) * 4; i++) xs.push_back((k * 37 + i) % 1000 * 0.5);\n        std::pair<double, double> m = stats(xs, (double) (k % 23));\n        if (std::isnan(m.first)) empty++;\n        else if (std::isinf(m.first)) infinite++;\n        else total += m.first;\n        if (!std::isinf(m.second)) total += m.second;\n    }\n    std::printf(\"total %.3f, empty %d, infinite %d\\n\", total, empty, infinite);\n    return 0;\n}\n";
        let port = "pub fn stats(xs: Vec<f64>, w: f64) -> Result<(f64, f64), String> {\n    let s = xs.iter().fold(0.0, |s, x| s + x);\n    let low = xs.iter().fold(f64::INFINITY, |m, &x| if x < m { x } else { m });\n    Ok((s / xs.len() as f64 / w, low))\n}\n";
        let id = "ranges.stats";
        let Some((dir, cc)) =
            enabled_c_project("nan-cpp", &[("ranges.cpp", ranges), ("main.cpp", main)], &[(id, port)])
        else {
            eprintln!("no C++ compiler or cargo on PATH, skipping");
            return;
        };
        let cases = recorded(&dir, id);
        let verified = replay(&dir, id);
        // the port answers NaN and infinity itself, also inside the vector and the pair
        let check = "#include <cmath>\n#include <cstdio>\n#include <utility>\n#include <vector>\nbool fa_on_S();\nbool fa_fast_S(std::pair<double, double> &, const std::vector<double> &xs, double w);\nint main() {\n    std::pair<double, double> a, b, c;\n    bool x = fa_fast_S(a, {}, 1.0), y = fa_fast_S(b, {1.0, 2.0}, 0.0), z = fa_fast_S(c, {1.0, NAN, -INFINITY}, NAN);\n    std::printf(\"%d %d %d %d %d %d %d\\n\", fa_on_S(), x, y, z, std::isnan(a.first) && std::isinf(a.second) && a.second > 0,\n                std::isinf(b.first) && b.second == 1.0, std::isnan(c.first) && std::isinf(c.second) && c.second < 0);\n    return 0;\n}\n";
        fs::write(dir.join("check.cpp"), check.replace("_S(", &format!("_{}(", util::slug(id)))).unwrap();
        let original = run_c(&dir, cc, &["main.cpp", "ranges.cpp"], true);
        let enabled = run_c(&dir, cc, &["main.cpp", "ranges.cpp"], false);
        let checked = run_c(&dir, cc, &["check.cpp", "ranges.cpp"], false);
        let empty_is_zero = "Ok((if xs.is_empty() { 0.0 } else { s / xs.len() as f64 / w }, low))";
        let wrong = port.replace("Ok((s / xs.len() as f64 / w, low))", empty_is_zero);
        let caught = replay_other(&dir, id, &wrong);
        let _ = fs::remove_dir_all(&dir);
        let real = |c: &&Value| c.get("derived").is_none();
        let edge = |c: &&Value| c["derived"] == "edge";
        let empty = serde_json::json!([tagged("NaN"), tagged("Infinity")]);
        assert!(cases.iter().filter(real).any(|c| c["args"][0] == serde_json::json!([]) && c["result"] == empty));
        assert!(cases.iter().filter(real).any(|c| c["result"][0] == tagged("Infinity")), "{cases:?}");
        for name in ["NaN", "Infinity", "-Infinity"] {
            assert!(cases.iter().filter(edge).any(|c| c["args"][1] == tagged(name)), "{name}: {cases:?}");
            assert!(cases.iter().filter(edge).any(|c| c["args"][0][0] == tagged(name)), "{name}: {cases:?}");
        }
        assert!(
            cases.iter().filter(edge).any(|c| c["args"][1].as_f64().is_some_and(|x| x == 0.0 && x.is_sign_negative()))
        );
        assert!(verified.total > 0 && verified.passed == verified.total, "{verified:?}");
        assert!(caught.failed > 0 && caught.failures.iter().any(|f| f.expected.contains(r#"{"$float":"NaN"}"#)));
        assert!(original.contains("empty 80, infinite 170"), "{original}");
        assert_eq!(enabled, original);
        assert_eq!(checked, "1 1 1 1 1 1 1\n");
    }

    #[test]
    fn cpp_json_carries_nan_and_infinity_tagged() {
        let Some(cc) =
            ["c++", "g++", "clang++"].into_iter().find(|c| Command::new(c).arg("--version").output().is_ok())
        else {
            eprintln!("no C++ compiler on PATH, skipping");
            return;
        };
        let main = r#"int main() {
    const double inf = INFINITY;
    std::string o = "[";
    t::enc_all(o, std::vector<double>{NAN, -0.5, -inf}, std::map<std::string, float>{{"k", (float) inf}},
               std::pair<double, long>{-0.0, 1}, std::array<double, 2>{{inf, 2.5}});
    std::printf("%s]\n", o.c_str());
    const char *p = "[[1.5,{\"$float\":\"NaN\"},{\"$float\":\"Infinity\"},{\"$float\":\"-Infinity\"},-0.0],"
                    "{\"k\":{\"$float\":\"-Infinity\"}},[{\"$float\":\"NaN\"},2],{\"$float\":\"nan\"}]";
    t::V v = t::parse(p);
    std::vector<double> xs;
    std::map<std::string, float> m;
    std::pair<double, double> pair;
    double lookalike = 1;
    t::from(v.arr[0], xs);
    t::from(v.arr[1], m);
    t::from(v.arr[2], pair);
    t::from(v.arr[3], lookalike);
    std::printf("%g %d %g %g %d | %g | %d %g | %g\n", xs[0], (int) std::isnan(xs[1]), xs[2], xs[3], (int) std::signbit(xs[4]),
                (double) m["k"], (int) std::isnan(pair.first), pair.second, lookalike);
    return 0;
}
"#;
        let includes = ["algorithm", "array", "cmath", "cstdio", "cstdlib", "cstring", "limits", "map", "string"]
            .iter()
            .chain(&["type_traits", "unordered_map", "utility", "vector"])
            .map(|h| format!("#include <{h}>\n"))
            .collect::<String>();
        let dir = std::env::temp_dir().join(format!("fa-cpp-json-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("t.cpp"), format!("{includes}namespace t {{\n{JSON_HPP}}}\n{main}")).unwrap();
        let built = Command::new(cc)
            .args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "t.cpp", "-o", "t"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let ran = built.status.success().then(|| Command::new(dir.join("t")).output().unwrap());
        let _ = fs::remove_dir_all(&dir);
        assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        assert_eq!(
            String::from_utf8_lossy(&ran.unwrap().stdout).replace("\r\n", "\n"),
            "[[{\"$float\":\"NaN\"},-0.5,{\"$float\":\"-Infinity\"}],{\"k\":{\"$float\":\"Infinity\"}},[-0.0,1],[{\"$float\":\"Infinity\"},2.5]]\n\
             1.5 1 inf -inf 1 | -inf | 1 2 | 0\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn c_values_reach_the_port_as_typed_and_come_back_exact() {
        let lib = "#include <stdbool.h>\n#include <stddef.h>\n#include <string.h>\n\nlong long mix(signed char a, unsigned short b, int c, unsigned int d, long e, const char *s, const long *xs, size_t n) {\n    long long t = a + b + c + (long long) d + e + (long long) strlen(s);\n    for (size_t i = 0; i < n; i++) t += xs[i] * (long long) (i + 1);\n    return t;\n}\n\nunsigned long long hash(const unsigned char *bs, int n, unsigned long long seed) {\n    unsigned long long h = seed;\n    for (int i = 0; i < n; i++) h = h * 1099511628211ULL ^ bs[i];\n    return h;\n}\n\ndouble scale(float g, const int *ys, unsigned n, double k) {\n    double t = g;\n    for (unsigned i = 0; i < n; i++) t += ys[i] * k;\n    return t;\n}\n\nbool longer(const char *s, short k, bool flip) {\n    return ((long) strlen(s) > k) != flip;\n}\n";
        let main = "#include <stdbool.h>\n#include <stddef.h>\n#include <stdio.h>\nlong long mix(signed char a, unsigned short b, int c, unsigned int d, long e, const char *s, const long *xs, size_t n);\nunsigned long long hash(const unsigned char *bs, int n, unsigned long long seed);\ndouble scale(float g, const int *ys, unsigned n, double k);\nbool longer(const char *s, short k, bool flip);\nint main(void) {\n    const char *words[4] = {\"plain\", \"h\\xc3\\xa9llo\", \"\\xe6\\x97\\xa5\\xe6\\x9c\\xac\", \"\"};\n    long xs[16];\n    int ys[16];\n    unsigned char bs[16];\n    for (int i = 0; i < 16; i++) { xs[i] = i * 1000 - 7000; ys[i] = 3 - i; bs[i] = (unsigned char) (i * 37); }\n    long long a = 0;\n    unsigned long long h = 0;\n    double s = 0;\n    int l = 0;\n    for (int k = 0; k < 400; k++) {\n        a += mix((signed char) (k % 200 - 100), (unsigned short) (k * 160), k * -9000, 4000000000u - k, -k * 1000L, words[k % 4], xs, (size_t) (k % 16));\n        h ^= hash(bs, k % 16, 18446744073709551557ULL - k);\n        s += scale(k * 0.37f, ys, (unsigned) (k % 16), k * 0.001);\n        l += longer(words[k % 4], (short) (k % 8), k % 3 == 0);\n    }\n    printf(\"%lld %llu %.17g %d\\n\", a, h, s, l);\n    return 0;\n}\n";
        let ports = [
            (
                "lib.mix",
                "pub fn mix(a: i8, b: u16, c: i32, d: u32, e: i64, s: String, xs: Vec<i64>, _n: u64) -> Result<i64, String> {\n    let t = [a as i64, b as i64, c as i64, d as i64, e, s.len() as i64].iter().fold(0i64, |t, v| t.wrapping_add(*v));\n    Ok(xs.iter().enumerate().fold(t, |t, (i, x)| t.wrapping_add(x.wrapping_mul(i as i64 + 1))))\n}\n",
            ),
            (
                "lib.hash",
                "pub fn hash(bs: Vec<u8>, _n: i32, seed: u64) -> Result<u64, String> {\n    Ok(bs.iter().fold(seed, |h, &b| h.wrapping_mul(1099511628211) ^ b as u64))\n}\n",
            ),
            (
                "lib.scale",
                "pub fn scale(g: f32, ys: Vec<i32>, _n: u32, k: f64) -> Result<f64, String> {\n    Ok(ys.iter().fold(g as f64, |t, &y| t + y as f64 * k))\n}\n",
            ),
            (
                "lib.longer",
                "pub fn longer(s: String, k: i16, flip: bool) -> Result<bool, String> {\n    Ok((s.len() as i64 > k as i64) != flip)\n}\n",
            ),
        ];
        let Some((dir, cc)) = enabled_c_project("typed", &[("lib.c", lib), ("main.c", main)], &ports) else {
            eprintln!("no C compiler or cargo on PATH, skipping");
            return;
        };
        // verify replays the recorded calls through the same C values
        let verified: Vec<(&str, usize, usize)> = ports
            .iter()
            .map(|(id, _)| {
                let v = replay(&dir, id);
                (*id, v.passed, v.total)
            })
            .collect();
        let check = "#include <math.h>\n#include <stdbool.h>\n#include <stddef.h>\n#include <stdio.h>\nint fa_on_M(void);\nint fa_on_H(void);\nint fa_on_C(void);\nint fa_on_L(void);\nlong long fa_fast_M(int *, signed char a, unsigned short b, int c, unsigned int d, long e, const char *s, const long *xs, size_t n);\nunsigned long long fa_fast_H(int *, const unsigned char *bs, int n, unsigned long long seed);\ndouble fa_fast_C(int *, float g, const int *ys, unsigned n, double k);\nbool fa_fast_L(int *, const char *s, short k, bool flip);\nint main(void) {\n    long xs[3] = {5, -6, 7};\n    int ys[2] = {3, -4};\n    unsigned char bs[4] = {1, 200, 3, 255};\n    int ok[7];\n    int on = fa_on_M() + fa_on_H() + fa_on_C() + fa_on_L();\n    long long r1 = fa_fast_M(&ok[0], -5, 65000, -70000, 4000000000u, -900000L, \"h\\xc3\\xa9llo \\xe6\\x97\\xa5\", xs, 3);\n    unsigned long long r2 = fa_fast_H(&ok[1], bs, 4, 18446744073709551557ULL);\n    double r3 = fa_fast_C(&ok[2], 1.5f, ys, 2, 0.25);\n    bool r4 = fa_fast_L(&ok[3], \"\\xc3\\xa9\\xc3\\xa9\", 3, true);\n    bool r5 = fa_fast_L(&ok[4], \"ab\", 1, false);\n    /* NaN reaches the port; NULL never does, the C version answers that */\n    double r6 = fa_fast_C(&ok[5], NAN, ys, 2, 0.25);\n    fa_fast_L(&ok[6], NULL, 1, false);\n    printf(\"%d %lld %llu %.17g %d %d\", on, r1, r2, r3, (int) r4, (int) r5);\n    for (int i = 0; i < 7; i++) printf(\" %d\", ok[i]);\n    printf(\" %d\\n\", isnan(r6) != 0);\n    return 0;\n}\n";
        let check = [("_M(", "lib.mix"), ("_H(", "lib.hash"), ("_C(", "lib.scale"), ("_L(", "lib.longer")]
            .iter()
            .fold(check.to_string(), |c, (from, id)| c.replace(from, &format!("_{}(", util::slug(id))));
        fs::write(dir.join("check.c"), check).unwrap();
        let original = run_c(&dir, cc, &["main.c", "lib.c"], true);
        let enabled = run_c(&dir, cc, &["main.c", "lib.c"], false);
        let checked = run_c(&dir, cc, &["check.c", "lib.c"], false);
        let _ = fs::remove_dir_all(&dir);
        for (id, passed, total) in verified {
            assert!(total > 0 && passed == total, "{id}: {passed} of {total} recorded calls match");
        }
        assert_eq!(enabled, original);
        let r1 = -5i64 + 65000 - 70000 + 4000000000 - 900000 + "h\u{e9}llo \u{65e5}".len() as i64 + 5 - 6 * 2 + 7 * 3;
        let r2 =
            [1u8, 200, 3, 255].iter().fold(18446744073709551557u64, |h, &b| h.wrapping_mul(1099511628211) ^ b as u64);
        assert_eq!(checked, format!("4 {r1} {r2} 1.25 0 1 1 1 1 1 1 1 0 1\n"));
    }

    #[cfg(unix)]
    #[test]
    fn verify_times_the_call_the_c_app_makes() {
        let geo = "/* Squared distance between two points. */\ndouble dist2(double x1, double y1, double x2, double y2) {\n    double dx = x1 - x2, dy = y1 - y2;\n    return dx * dx + dy * dy;\n}\n";
        let main = "#include <stdio.h>\ndouble dist2(double x1, double y1, double x2, double y2);\nint main(void) {\n    double acc = 0.0;\n    for (int i = 0; i < 200000; i++) {\n        double t = i * 1e-5;\n        acc += dist2(t, 1.0 - t, 0.5, 0.25);\n    }\n    printf(\"%.6f\\n\", acc);\n    return 0;\n}\n";
        let port = "pub fn dist2(x1: f64, y1: f64, x2: f64, y2: f64) -> Result<f64, String> {\n    let (dx, dy) = (x1 - x2, y1 - y2);\n    Ok(dx * dx + dy * dy)\n}\n";
        let Some((dir, _)) = enabled_c_project("time", &[("geo.c", geo), ("main.c", main)], &[("geo.dist2", port)])
        else {
            eprintln!("no C compiler or cargo on PATH, skipping");
            return;
        };
        let v = replay(&dir, "geo.dist2");
        let _ = fs::remove_dir_all(&dir);
        assert!(v.total > 0 && v.passed == v.total, "{v:?}");
        // both sides timed per call: within 3x of the original (through JSON it was 22x slower)
        let m = v.micro.expect("timed");
        assert!(m.rust_ns > 0.0 && m.rust_ns < 3.0 * m.original_ns, "{m:?}");
        assert!(m.clock_ns > 0.0 && m.clock_ns < m.original_ns, "the clock is part of the original's time: {m:?}");
    }

    #[cfg(unix)]
    #[test]
    fn too_cheap_to_time_stays_off_and_microseconds_get_switched_on() {
        let geo = "double dist2(double x1, double y1, double x2, double y2) {\n    double dx = x1 - x2, dy = y1 - y2;\n    return dx * dx + dy * dy;\n}\n";
        let ranges = "/* Mean of [lo, hi), the slow way: NaN for an empty range (0/0). */\ndouble mean_of_range(long long lo, long long hi) {\n    double s = 0.0;\n    for (long long i = lo; i < hi; i++) s += (double) i;\n    return s / (double) (hi - lo);\n}\n";
        let main = "#include <math.h>\n#include <stdio.h>\ndouble dist2(double x1, double y1, double x2, double y2);\ndouble mean_of_range(long long lo, long long hi);\nint main(void) {\n    double acc = 0.0, total = 0.0;\n    int empty = 0;\n    for (int i = 0; i < 200000; i++) acc += dist2(i * 1e-5, 1.0 - i * 1e-5, 0.5, 0.25);\n    for (int k = 0; k < 20000; k++) {\n        long long lo = k * 37 % 1000, hi = lo + (k % 50) * 100;\n        double m = mean_of_range(lo, hi);\n        if (isnan(m)) empty++;\n        else total += m;\n    }\n    printf(\"%.6f, total %.3f, empty ranges %d\\n\", acc, total, empty);\n    return 0;\n}\n";
        let ports = [
            (
                "geo.dist2",
                "pub fn dist2(x1: f64, y1: f64, x2: f64, y2: f64) -> Result<f64, String> {\n    let (dx, dy) = (x1 - x2, y1 - y2);\n    Ok(dx * dx + dy * dy)\n}\n",
            ),
            (
                "ranges.mean_of_range",
                "pub fn mean_of_range(lo: i64, hi: i64) -> Result<f64, String> {\n    if hi <= lo {\n        return Ok(0.0 / (hi - lo) as f64);\n    }\n    Ok((lo + hi - 1) as f64 / 2.0)\n}\n",
            ),
        ];
        let files = [("geo.c", geo), ("ranges.c", ranges), ("main.c", main)];
        let Some((dir, cc)) = enabled_c_project("cheap", &files, &ports) else {
            eprintln!("no C compiler or cargo on PATH, skipping");
            return;
        };
        for (id, _) in ports {
            crate::pipeline::disable(&dir, Some(id)).unwrap();
        }
        // other tests building meanwhile can slow the recorded calls down: record those again
        for _ in 0..5 {
            if replay(&dir, "geo.dist2").too_cheap() {
                break;
            }
            let out = crate::pipeline::port(&dir, Some("geo.dist2"), 200).unwrap();
            fs::write(&out.lib_rs, ports[0].1).unwrap();
            let ctx = Ctx::open(&dir).unwrap();
            assert!(backend::build(&ctx, &ctx.state.ports["geo.dist2"]).unwrap().is_ok());
        }
        // what verify keeps for enable
        let mut ctx = Ctx::open(&dir).unwrap();
        let [cheap, costly] = ports.map(|(id, _)| {
            let v = Verify { built: true, lang: Lang::C, ..replay(&dir, id) };
            ctx.state.ports.get_mut(id).unwrap().verify = Some(v.clone());
            v
        });
        ctx.save().unwrap();
        let refused = crate::pipeline::enable(&dir, Some("geo.dist2"), false);
        let switched = crate::pipeline::enable(&dir, Some("ranges.mean_of_range"), false);
        let original = run_c(&dir, cc, &["main.c", "geo.c", "ranges.c"], true);
        let enabled = run_c(&dir, cc, &["main.c", "geo.c", "ranges.c"], false);
        let on = Ctx::open(&dir).unwrap().state.ports.values().map(|p| (p.id.clone(), p.enabled)).collect::<Vec<_>>();
        let forced = crate::pipeline::enable(&dir, Some("geo.dist2"), true);
        let _ = fs::remove_dir_all(&dir);
        assert!(cheap.ok() && cheap.too_cheap(), "{:?}", cheap.micro);
        let refused = refused.unwrap_err().to_string();
        assert!(
            refused.starts_with("geo.dist2 takes about as long as reading the clock (")
                && refused.contains(" in C, the clock "),
            "{refused}"
        );
        assert!(costly.ok() && !costly.too_cheap() && !costly.slower(), "{:?}", costly.micro);
        switched.unwrap();
        assert_eq!(on, [("geo.dist2".to_string(), false), ("ranges.mean_of_range".to_string(), true)]);
        assert!(original.ends_with("empty ranges 400\n"), "{original}");
        assert_eq!(enabled, original);
        forced.unwrap();
    }

    /// The typed glue generated into every C port, compiled here as it is there: next to the
    /// JSON entry point's wire code.
    #[allow(dead_code)]
    mod typed_glue {
        const FLOATS: [&str; 4] = crate::native::FLOATS;
        include!("native/wire.rs");
        mod typed {
            include!("cfamily/typed.rs");
        }
        use serde::de::IntoDeserializer;
        use serde_json::json;
        use typed::*;

        #[test]
        fn c_values_convert_like_json_and_results_take_one_number_or_bool() {
            assert_eq!(typed_arg::<i8, _>(0, 100i64.into_deserializer()), Ok(100));
            assert!(typed_arg::<i8, _>(1, 300i64.into_deserializer()).unwrap_err().starts_with("argument 2: "));
            assert_eq!(typed_arg::<f32, _>(0, 0.1f64.into_deserializer()), Ok(0.1f32));
            assert_eq!(typed_arg::<String, _>(0, "h\u{e9}".into_deserializer()), Ok("h\u{e9}".to_string()));
            assert_eq!(typed_items::<Vec<u16>, _>(0, [1u64, 2].into_iter()), Ok(vec![1, 2]));
            assert!(typed_items::<[i32; 3], _>(0, [1i64, 2].into_iter()).is_err());
            let bad = c"\xff";
            assert!(
                unsafe { typed_str(0, bad.as_ptr()) }.is_err() && unsafe { typed_str(0, std::ptr::null()) }.is_err()
            );
            assert!(unsafe { typed_slice::<i32>(0, std::ptr::null(), 2u64) }.is_err());
            assert_eq!(unsafe { typed_slice::<i32>(0, std::ptr::null(), 0i64) }, Ok(&[][..]));
            assert!(unsafe { typed_slice::<i32>(0, [1].as_ptr(), -1i64) }.is_err());
            assert_eq!(typed_ret::<i64, _>(&-7i32), Ok(-7));
            assert_eq!(typed_ret::<i64, _>(&u64::MAX), Ok(i64::MAX));
            assert_eq!(typed_ret::<i64, _>(&-2.9f64), Ok(-2));
            assert_eq!(typed_ret::<u64, _>(&u64::MAX), Ok(u64::MAX));
            assert!(typed_ret::<f64, _>(&f64::NAN).unwrap().is_nan());
            assert_eq!(typed_ret::<f64, _>(&f32::NEG_INFINITY), Ok(f64::NEG_INFINITY));
            assert_eq!(typed_ret::<f64, _>(&Some(2.5f64)), Ok(2.5));
            assert_eq!(typed_ret::<i32, _>(&true), Ok(1));
            assert!(typed_ret::<f64, _>(&"text").is_err() && typed_ret::<f64, _>(&None::<f64>).is_err());
            assert!(typed_ret::<i64, _>(&vec![1]).is_err());
            assert_eq!(1i32.json(), json!(true));
            assert_eq!(f64::NAN.json(), json!({"$float": "NaN"}));
        }

        #[test]
        fn verify_reads_and_writes_nan_and_infinity_tagged() {
            let tag = |name: &str| json!({ "$float": name });
            assert!(typed_f64(&tag("NaN")).unwrap().is_nan());
            assert_eq!(typed_f64(&tag("-Infinity")), Some(f64::NEG_INFINITY));
            assert_eq!(typed_f32(&tag("Infinity")), Some(f32::INFINITY));
            assert_eq!(typed_f64(&json!(-0.0)).map(f64::is_sign_negative), Some(true));
            assert_eq!(typed_f64(&tag("nan")), None);
            assert_eq!(typed_f64(&json!({"$float": "NaN", "x": 1})), None);
            let xs = typed_vec(&json!([1.5, tag("Infinity"), 2]), typed_f64).unwrap();
            assert_eq!(xs, [1.5, f64::INFINITY, 2.0]);
            let args = [json!(1), tag("NaN")];
            assert!(typed_json(&args, 1, typed_f64).unwrap().is_nan());
            assert_eq!(f64::INFINITY.json(), tag("Infinity"));
            assert_eq!(f64::NEG_INFINITY.json(), tag("-Infinity"));
            assert!((-0.0f64).json().as_f64().is_some_and(f64::is_sign_negative));
        }
    }
}
