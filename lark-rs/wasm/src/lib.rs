//! wasm-bindgen bindings for `lark-rs` — the Lark parsing toolkit compiled to
//! WebAssembly for browser and Node.js (#47).
//!
//! The module exposes one class mirroring the PyO3 binding's API surface:
//!
//! * `Lark` — `new Lark(grammar, { parser, lexer, start, ... })` + `.parse()`
//!
//! `.parse()` returns the tree as a plain JS object in the **oracle JSON
//! shape** the rest of the repo standardizes on (`tools/generate_oracles.py`):
//!
//! ```json
//! {"type": "tree",  "data": "rule_name", "children": [...]}
//! {"type": "token", "token_type": "NAME", "value": "...", "line": 1, ...}
//! {"type": "unknown", "repr": "None"}            // maybe_placeholders hole
//! ```
//!
//! so a JS test can compare a parse against a committed Python-Lark oracle
//! fixture directly (see `tests/wasm/`). Token nodes additionally carry
//! position fields the oracle omits.
//!
//! All parsing logic lives in the shared `lark-rs` crate; this file is a thin
//! adapter. Two WASM-specific constraints shaped it:
//!
//! * No `std::thread`, and small native stacks (~1 MB): the engine is
//!   recursion-free in input depth (#33), and `Tree`'s `Drop`/`Clone` are
//!   iterative (#151) — and the serializer below is also an explicit-stack
//!   walk, so no path here recurses to tree depth either.
//! * No filesystem: `%import` of the bundled libraries (`common`, `python`,
//!   `lark`, `unicode`) works — they are compiled from in-memory sources — and
//!   relative file imports (`%import .module (...)`) resolve against the
//!   `importSources` option (the #47 follow-up): a plain object mapping virtual paths
//!   (`"tokens.lark"`, `"dir/lib.lark"`) to grammar text. Without it, a file
//!   import fails with the same `ImportNotFound` a grammar loaded from a bare
//!   string gets everywhere else.

use lark_core::{
    Ambiguity, Child, Lark, LarkOptions, LexerType, ParseTree, ParserAlgorithm, Token, Tree,
};
use wasm_bindgen::prelude::*;

// ─── Errors ─────────────────────────────────────────────────────────────────
// Mirror the exception split of the PyO3 binding: a JS `Error` whose `name` is
// "GrammarError" (grammar failed to compile) or "ParseError" (input rejected),
// so callers can dispatch on `e.name`.

fn js_error(name: &str, message: &str) -> JsValue {
    let err = js_sys::Error::new(message);
    err.set_name(name);
    err.into()
}

fn map_lark_error(e: lark_core::LarkError) -> JsValue {
    match e {
        lark_core::LarkError::Grammar(g) => js_error("GrammarError", &g.to_string()),
        lark_core::LarkError::Parse(p) => js_error("ParseError", &p.to_string()),
    }
}

// ─── Lark ───────────────────────────────────────────────────────────────────

/// The compiled parser, mirroring `lark.Lark`.
#[wasm_bindgen(js_name = Lark)]
pub struct WasmLark {
    inner: Lark,
}

#[wasm_bindgen(js_class = Lark)]
impl WasmLark {
    /// Build a parser from grammar text.
    ///
    /// `options` is an optional plain object with the same keys (and defaults)
    /// as the Python binding's kwargs: `parser` ("earley" | "lalr" | "cyk"),
    /// `lexer` ("auto" | "basic" | "contextual" | "dynamic" |
    /// "dynamic_complete"), `start` (string or array of strings), `ambiguity`
    /// ("resolve" | "explicit" | "forest"), `propagatePositions`,
    /// `keepAllTokens`, `maybePlaceholders`, `strict` (booleans),
    /// `gRegexFlags` (a string of Python-style flag letters, e.g. `"is"`), and
    /// `importSources` (an object mapping virtual paths to grammar text, for
    /// relative `%import` without a filesystem). Snake_case key spellings are
    /// accepted too.
    #[wasm_bindgen(constructor)]
    pub fn new(grammar: &str, options: &JsValue) -> Result<WasmLark, JsValue> {
        let options = parse_options(options)?;
        let inner = Lark::new(grammar, options).map_err(map_lark_error)?;
        Ok(WasmLark { inner })
    }

    /// Parse `text` and return the tree as a plain JS object (or, for a
    /// collapsing `?start` rule, a bare token object — exactly as Python Lark
    /// returns a bare `Token`). `start` selects among multiple start rules.
    pub fn parse(&self, text: &str, start: Option<String>) -> Result<JsValue, JsValue> {
        let json = self.parse_to_json(text, start)?;
        js_sys::JSON::parse(&json).map_err(|_| {
            js_error(
                "LarkError",
                "internal: tree serialization was not valid JSON",
            )
        })
    }

    /// Parse `text` and return the tree serialized as a JSON string (the same
    /// shape `parse` returns as an object).
    #[wasm_bindgen(js_name = parseToJson)]
    pub fn parse_to_json(&self, text: &str, start: Option<String>) -> Result<String, JsValue> {
        let result = match start.as_deref() {
            Some(s) => self.inner.parse_with_start(text, s),
            None => self.inner.parse(text),
        }
        .map_err(|e| js_error("ParseError", &e.to_string()))?;
        Ok(parse_tree_to_json(&result))
    }
}

/// The lark-rs crate version baked into the package.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ─── Options parsing ────────────────────────────────────────────────────────

/// Read `key` (or its snake_case spelling) off the options object.
fn get(opts: &JsValue, key: &str, snake: &str) -> Option<JsValue> {
    for k in [key, snake] {
        if let Ok(v) = js_sys::Reflect::get(opts, &JsValue::from_str(k)) {
            if !v.is_undefined() && !v.is_null() {
                return Some(v);
            }
        }
    }
    None
}

fn get_string(opts: &JsValue, key: &str, snake: &str) -> Result<Option<String>, JsValue> {
    match get(opts, key, snake) {
        None => Ok(None),
        Some(v) => v
            .as_string()
            .map(Some)
            .ok_or_else(|| js_error("GrammarError", &format!("option '{key}' must be a string"))),
    }
}

fn get_bool(opts: &JsValue, key: &str, snake: &str, default: bool) -> Result<bool, JsValue> {
    match get(opts, key, snake) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| js_error("GrammarError", &format!("option '{key}' must be a boolean"))),
    }
}

fn parse_options(opts: &JsValue) -> Result<LarkOptions, JsValue> {
    if !opts.is_undefined() && !opts.is_null() && !opts.is_object() {
        return Err(js_error("GrammarError", "options must be an object"));
    }

    let parser = match get_string(opts, "parser", "parser")?.as_deref() {
        None | Some("earley") => ParserAlgorithm::Earley,
        Some("lalr") => ParserAlgorithm::Lalr,
        Some("cyk") => ParserAlgorithm::Cyk,
        Some(other) => {
            return Err(js_error(
                "GrammarError",
                &format!("unknown parser {other:?} (expected 'earley', 'lalr', or 'cyk')"),
            ))
        }
    };

    let lexer = match get_string(opts, "lexer", "lexer")?.as_deref() {
        None | Some("auto") => LexerType::Auto,
        // Python Lark renamed 'standard' to 'basic'; accept both.
        Some("basic") | Some("standard") => LexerType::Basic,
        Some("contextual") => LexerType::Contextual,
        Some("dynamic") => LexerType::Dynamic,
        Some("dynamic_complete") => LexerType::DynamicComplete,
        Some(other) => {
            return Err(js_error(
                "GrammarError",
                &format!(
                    "unknown lexer {other:?} (expected 'auto', 'basic', 'contextual', \
                     'dynamic', or 'dynamic_complete')"
                ),
            ))
        }
    };

    let ambiguity = match get_string(opts, "ambiguity", "ambiguity")?.as_deref() {
        None | Some("resolve") => Ambiguity::Resolve,
        Some("explicit") => Ambiguity::Explicit,
        Some("forest") => Ambiguity::Forest,
        Some(other) => {
            return Err(js_error(
                "GrammarError",
                &format!(
                    "unknown ambiguity {other:?} (expected 'resolve', 'explicit', or 'forest')"
                ),
            ))
        }
    };

    let start = match get(opts, "start", "start") {
        None => vec!["start".to_string()],
        Some(v) => {
            if let Some(s) = v.as_string() {
                vec![s]
            } else if js_sys::Array::is_array(&v) {
                let arr = js_sys::Array::from(&v);
                let mut out = Vec::with_capacity(arr.length() as usize);
                for item in arr.iter() {
                    out.push(item.as_string().ok_or_else(|| {
                        js_error("GrammarError", "start must be a string or array of strings")
                    })?);
                }
                if out.is_empty() {
                    return Err(js_error("GrammarError", "start must not be empty"));
                }
                out
            } else {
                return Err(js_error(
                    "GrammarError",
                    "start must be a string or array of strings",
                ));
            }
        }
    };

    // Python's `g_regex_flags` is an `re` flag bitset; JS has no such
    // constants, so the binding takes the flag letters as a string.
    let mut g_regex_flags = 0u32;
    if let Some(letters) = get_string(opts, "gRegexFlags", "g_regex_flags")? {
        use lark_core::grammar::terminal::flags;
        for ch in letters.chars() {
            g_regex_flags |= match ch {
                'i' => flags::IGNORECASE,
                'm' => flags::MULTILINE,
                's' => flags::DOTALL,
                'x' => flags::VERBOSE,
                other => {
                    return Err(js_error(
                        "GrammarError",
                        &format!("unknown regex flag {other:?} (expected letters from 'imsx')"),
                    ))
                }
            };
        }
    }

    // In-memory grammar sources for relative `%import .module (...)` (the #47 follow-up):
    // a plain object mapping virtual `/`-separated paths (e.g. "tokens.lark",
    // "dir/lib.lark") to grammar text — WASM has no filesystem, so this is the
    // only way to supply sibling grammars.
    let import_sources = match get(opts, "importSources", "import_sources") {
        None => None,
        Some(v) => {
            if !v.is_object() {
                return Err(js_error(
                    "GrammarError",
                    "importSources must be an object mapping paths to grammar text",
                ));
            }
            let mut map = std::collections::HashMap::new();
            for entry in js_sys::Object::entries(&js_sys::Object::from(v)).iter() {
                let pair = js_sys::Array::from(&entry);
                let (key, value) = (pair.get(0).as_string(), pair.get(1).as_string());
                match (key, value) {
                    (Some(k), Some(text)) => {
                        map.insert(k, text);
                    }
                    _ => {
                        return Err(js_error(
                            "GrammarError",
                            "importSources values must be grammar-text strings",
                        ))
                    }
                }
            }
            Some(std::sync::Arc::new(map))
        }
    };

    Ok(LarkOptions {
        start,
        parser,
        lexer,
        ambiguity,
        propagate_positions: get_bool(opts, "propagatePositions", "propagate_positions", false)?,
        keep_all_tokens: get_bool(opts, "keepAllTokens", "keep_all_tokens", false)?,
        maybe_placeholders: get_bool(opts, "maybePlaceholders", "maybe_placeholders", true)?,
        strict: get_bool(opts, "strict", "strict", false)?,
        g_regex_flags,
        base_path: None,
        import_sources,
        postlex: None,
        // No JS option — the binding always uses the default scanner backend;
        // the backend choice is an internal lark-rs knob (LEXER_DFA_PLAN).
        lexer_backend: Default::default(),
    })
}

// ─── Tree serialization ─────────────────────────────────────────────────────

/// One pending emission step of the iterative serializer.
enum Emit<'a> {
    Tree(&'a Tree),
    Token(&'a Token),
    Hole,
    Raw(&'static str),
}

fn emit_for(child: &Child) -> Emit<'_> {
    match child {
        Child::Tree(t) => Emit::Tree(t),
        Child::Token(t) => Emit::Token(t),
        Child::None => Emit::Hole,
    }
}

/// Serialize a parse result to the oracle JSON shape with an explicit work
/// stack — a parse tree is as deep as the input is nested, and WASM stacks are
/// small, so this must not recurse to tree depth (the same property #33/#151
/// give the engine and `Tree`'s own glue).
fn parse_tree_to_json(pt: &ParseTree) -> String {
    let mut out = String::new();
    let mut stack: Vec<Emit> = vec![match pt {
        ParseTree::Tree(t) => Emit::Tree(t),
        ParseTree::Token(t) => Emit::Token(t),
        // A bare `None` root (`?start: [A]` on `""`, #289). Emit the same
        // `unknown`/`None` node Python's `tree_to_dict(None)` produces.
        ParseTree::None => Emit::Hole,
    }];
    while let Some(item) = stack.pop() {
        match item {
            Emit::Raw(s) => out.push_str(s),
            Emit::Hole => out.push_str(r#"{"type":"unknown","repr":"None"}"#),
            Emit::Token(tok) => {
                out.push_str(r#"{"type":"token","token_type":"#);
                push_json_str(&mut out, &tok.type_);
                out.push_str(r#","value":"#);
                push_json_str(&mut out, &tok.value);
                out.push_str(&format!(
                    r#","line":{},"column":{},"endLine":{},"endColumn":{},"startPos":{},"endPos":{}}}"#,
                    tok.line(), tok.column(), tok.end_line(), tok.end_column(), tok.start_pos(), tok.end_pos()
                ));
            }
            Emit::Tree(t) => {
                out.push_str(r#"{"type":"tree","data":"#);
                push_json_str(&mut out, &t.data);
                out.push_str(r#","children":["#);
                // Pops come in reverse push order: push the closer first, then
                // the children back-to-front with their separators.
                stack.push(Emit::Raw("]}"));
                for (i, child) in t.children.iter().enumerate().rev() {
                    stack.push(emit_for(child));
                    if i > 0 {
                        stack.push(Emit::Raw(","));
                    }
                }
            }
        }
    }
    out
}

/// Append `s` as a JSON string literal (quoted + escaped).
fn push_json_str(out: &mut String, s: &str) {
    out.push_str(&serde_json::to_string(s).expect("strings always serialize"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lalr(grammar: &str) -> Lark {
        Lark::new(
            grammar,
            LarkOptions {
                parser: ParserAlgorithm::Lalr,
                start: vec!["start".to_string()],
                ..Default::default()
            },
        )
        .expect("grammar builds")
    }

    #[test]
    fn serializer_matches_oracle_shape() {
        let lark = lalr("start: WORD \"!\"\nWORD: /[a-z]+/\n");
        let tree = lark.parse("hi!").expect("parses");
        let json: serde_json::Value =
            serde_json::from_str(&parse_tree_to_json(&tree)).expect("valid JSON");
        assert_eq!(json["type"], "tree");
        assert_eq!(json["data"], "start");
        assert_eq!(json["children"][0]["type"], "token");
        assert_eq!(json["children"][0]["token_type"], "WORD");
        assert_eq!(json["children"][0]["value"], "hi");
        assert_eq!(json["children"][0]["line"], 1);
    }

    #[test]
    fn serializer_escapes_strings() {
        let lark = lalr("start: STRING\nSTRING: /\"[^\"]*\"/\n");
        let tree = lark.parse("\"a\\b\"").expect("parses");
        let json: serde_json::Value =
            serde_json::from_str(&parse_tree_to_json(&tree)).expect("valid JSON despite quotes");
        assert_eq!(json["children"][0]["value"], "\"a\\b\"");
    }

    /// The serializer is an explicit-stack walk: a tree as deep as the input
    /// is nested must serialize without native recursion (WASM stacks are
    /// small — this is the #47 constraint the module doc states).
    #[test]
    fn serializer_is_iterative_on_deep_trees() {
        let lark = lalr("start: a\na: \"[\" a \"]\" | \"x\"\n");
        const N: usize = 100_000;
        let input = format!("{}x{}", "[".repeat(N), "]".repeat(N));
        let tree = lark.parse(&input).expect("deep nesting parses");
        let json = parse_tree_to_json(&tree);
        assert!(json.len() > N * 2, "every level serialized");
    }
}
