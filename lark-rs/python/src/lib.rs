//! PyO3 bindings for `lark-rs` — a drop-in, Rust-powered replacement for the
//! Python Lark parsing toolkit.
//!
//! This compiled extension is imported as `lark_rs._lark_rs`; the user-facing
//! `lark_rs` package re-exports it together with the pure-Python value types.
//! It exposes:
//!
//! * [`PyLark`] — `Lark(grammar, parser=..., lexer=..., start=...)` + `.parse()`
//! * [`PyTree`] — `Tree(data, children)` with `.data`, `.children`, `.meta`,
//!   `.pretty()`
//! * [`PyMeta`] — per-node metadata (`.empty`), always present on a `Tree`
//!
//! `Token` is **not** defined here: it must be a genuine `str` *subclass* to
//! match Python Lark's contract (issue #416), and PyO3 cannot subclass a native
//! type such as `str` under the `abi3` build (ADR-0036). It lives in pure Python
//! (`lark_rs._types.Token`); the Rust core constructs it via the cached class
//! object when shaping a parse tree, so callers get genuine `str`-subclass
//! tokens with no Python-side re-walk.
//!
//! All parsing logic lives in the shared `lark-rs` crate; this file is a thin
//! adapter that translates Python kwargs into [`LarkOptions`] and the Rust
//! `Tree`/`Token` types into Python objects.

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::sync::GILOnceCell;
use pyo3::types::{PyList, PyType};

use lark_core::{
    Ambiguity, Child, Lark, LarkOptions, LexerType, ParseTree, ParserAlgorithm, Token, Tree,
};
use lark_core::{
    GrammarError as RsGrammarError, LarkError as RsLarkError, ParseError as RsParseError,
};

// ─── Exceptions ─────────────────────────────────────────────────────────────
// Mirror Python Lark's exception hierarchy closely enough to be catchable in the
// same way: a `LarkError` base with `GrammarError` / `ParseError` subclasses.

create_exception!(
    lark_rs,
    LarkError,
    PyException,
    "Base class for all lark_rs errors."
);
create_exception!(
    lark_rs,
    GrammarError,
    LarkError,
    "Raised when a grammar fails to compile."
);
create_exception!(
    lark_rs,
    ParseError,
    LarkError,
    "Raised when input fails to parse."
);

fn map_lark_error(e: RsLarkError) -> PyErr {
    match e {
        RsLarkError::Grammar(g) => map_grammar_error(g),
        RsLarkError::Parse(p) => map_parse_error(p),
    }
}

fn map_grammar_error(e: RsGrammarError) -> PyErr {
    GrammarError::new_err(e.to_string())
}

fn map_parse_error(e: RsParseError) -> PyErr {
    ParseError::new_err(e.to_string())
}

// ─── Python value types (defined in `lark_rs._types`) ───────────────────────
//
// `Token` (a `str` subclass) and `Meta` are pure-Python types. We cache their
// class objects on first use so shaping a parse tree is a cheap `call` per node
// rather than an import per token.

static TOKEN_TYPE: GILOnceCell<Py<PyType>> = GILOnceCell::new();
static META_TYPE: GILOnceCell<Py<PyType>> = GILOnceCell::new();

fn token_type(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    TOKEN_TYPE
        .get_or_try_init(py, || {
            py.import("lark_rs._types")?
                .getattr("Token")?
                .downcast_into::<PyType>()
                .map(Into::into)
                .map_err(Into::into)
        })
        .map(|t| t.bind(py))
}

fn meta_type(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    META_TYPE
        .get_or_try_init(py, || {
            py.import("lark_rs._types")?
                .getattr("Meta")?
                .downcast_into::<PyType>()
                .map(Into::into)
                .map_err(Into::into)
        })
        .map(|t| t.bind(py))
}

/// Is `obj` a `lark_rs._types.Token` instance?
fn is_token(obj: &Bound<'_, PyAny>) -> bool {
    token_type(obj.py())
        .and_then(|t| obj.is_instance(t))
        .unwrap_or(false)
}

/// Build a Python `Token` (the `str` subclass) from the Rust core `Token`.
///
/// A parsed token always carries real positions from the lexer (independent of
/// `propagate_positions`, which governs `Tree.meta`, not token positions), so we
/// pass them through as ints — matching Python Lark, whose parsed tokens are
/// likewise positioned.
fn token_to_py(py: Python<'_>, t: &Token) -> PyResult<PyObject> {
    let cls = token_type(py)?;
    let obj = cls.call1((
        t.type_.clone(),
        t.value.clone(),
        t.start_pos(),
        t.line(),
        t.column(),
        t.end_line(),
        t.end_column(),
        t.end_pos(),
    ))?;
    Ok(obj.unbind())
}

fn new_meta(py: Python<'_>) -> PyResult<PyObject> {
    Ok(meta_type(py)?.call0()?.unbind())
}

// ─── Tree ───────────────────────────────────────────────────────────────────

/// A parse-tree node, mirroring `lark.Tree`.
///
/// `children` is a real Python `list` so it can be indexed, iterated and mutated
/// exactly like Python Lark's. Each child is a `Tree`, a `Token`, or `None`
/// (the latter only when the parser is built with `maybe_placeholders=True`).
/// `meta` is always present (an empty `Meta` until positions are propagated),
/// matching Python Lark, whose `Tree` always carries a `Meta` (issue #416).
#[pyclass(name = "Tree", module = "lark_rs._lark_rs")]
struct PyTree {
    #[pyo3(get, set)]
    data: String,
    #[pyo3(get, set)]
    children: Py<PyList>,
    #[pyo3(get, set)]
    meta: PyObject,
}

impl PyTree {
    fn from_tree(py: Python<'_>, t: &Tree) -> PyResult<Py<PyTree>> {
        let children = PyList::empty(py);
        for child in &t.children {
            let obj = child_to_py(py, child)?;
            children.append(obj)?;
        }
        Py::new(
            py,
            PyTree {
                data: t.data.clone(),
                children: children.unbind(),
                meta: new_meta(py)?,
            },
        )
    }
}

/// Convert a single `Child` to its Python representation.
fn child_to_py(py: Python<'_>, child: &Child) -> PyResult<PyObject> {
    match child {
        Child::Tree(t) => Ok(PyTree::from_tree(py, t)?.into_any()),
        Child::Token(tok) => token_to_py(py, tok),
        Child::None => Ok(py.None()),
    }
}

#[pymethods]
impl PyTree {
    #[new]
    #[pyo3(signature = (data, children=None))]
    fn new(py: Python<'_>, data: String, children: Option<Bound<'_, PyList>>) -> PyResult<PyTree> {
        let children = match children {
            Some(list) => list.unbind(),
            None => PyList::empty(py).unbind(),
        };
        Ok(PyTree {
            data,
            children,
            meta: new_meta(py)?,
        })
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let items: Vec<String> = self
            .children
            .bind(py)
            .iter()
            .map(|c| child_repr(&c))
            .collect();
        Ok(format!("Tree({:?}, [{}])", self.data, items.join(", ")))
    }

    /// Structural equality on `data` + `children`, like Python Lark's `Tree`.
    fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        let Ok(other) = other.extract::<PyRef<'_, PyTree>>() else {
            return Ok(false);
        };
        if self.data != other.data {
            return Ok(false);
        }
        let a = self.children.bind(py);
        let b = other.children.bind(py);
        if a.len() != b.len() {
            return Ok(false);
        }
        for (x, y) in a.iter().zip(b.iter()) {
            if !x.eq(&y)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Multi-line indented rendering, matching `lark.Tree.pretty()`.
    #[pyo3(signature = (indent_str="  "))]
    fn pretty(&self, py: Python<'_>, indent_str: &str) -> PyResult<String> {
        let mut out = String::new();
        self.pretty_into(py, 0, indent_str, &mut out)?;
        Ok(out)
    }

    fn __str__(&self, py: Python<'_>) -> PyResult<String> {
        self.__repr__(py)
    }
}

impl PyTree {
    fn pretty_into(
        &self,
        py: Python<'_>,
        level: usize,
        indent_str: &str,
        out: &mut String,
    ) -> PyResult<()> {
        let pad = indent_str.repeat(level);
        let children = self.children.bind(py);
        if children.len() == 1 {
            // A single non-tree child renders inline: `data\tvalue`.
            let only = children.get_item(0)?;
            if is_token(&only) {
                let value: String = only.getattr("value")?.extract()?;
                out.push_str(&format!("{}{}\t{}\n", pad, self.data, value));
                return Ok(());
            }
        }
        out.push_str(&format!("{}{}\n", pad, self.data));
        for child in children.iter() {
            if let Ok(subtree) = child.extract::<PyRef<'_, PyTree>>() {
                subtree.pretty_into(py, level + 1, indent_str, out)?;
            } else if is_token(&child) {
                let value: String = child.getattr("value")?.extract()?;
                out.push_str(&format!("{}{}\n", indent_str.repeat(level + 1), value));
            } else {
                out.push_str(&format!("{}None\n", indent_str.repeat(level + 1)));
            }
        }
        Ok(())
    }
}

fn child_repr(c: &Bound<'_, PyAny>) -> String {
    if let Ok(tree) = c.extract::<PyRef<'_, PyTree>>() {
        tree.__repr__(c.py())
            .unwrap_or_else(|_| "Tree(...)".to_string())
    } else if c.is_none() {
        "None".to_string()
    } else {
        // A `Token` (or any other leaf): defer to its own `repr`. On the unlikely
        // failure path use a placeholder, never the literal `"None"` — `None` is a
        // distinct, legitimate child kind (maybe_placeholders) and must not be
        // confused with a leaf whose repr could not be read.
        c.repr()
            .ok()
            .and_then(|r| r.extract::<String>().ok())
            .unwrap_or_else(|| "<token>".to_string())
    }
}

// ─── Meta ───────────────────────────────────────────────────────────────────
//
// `Meta` is defined in pure Python (`lark_rs._types`); see ADR-0036. It is
// re-exported through the package so `lark_rs.Meta` resolves, but the Rust core
// constructs instances via the cached class object (`new_meta`).

// ─── Lark ───────────────────────────────────────────────────────────────────

/// The compiled parser, mirroring `lark.Lark`.
#[pyclass(name = "Lark", module = "lark_rs._lark_rs", unsendable)]
struct PyLark {
    inner: Lark,
}

#[pymethods]
impl PyLark {
    /// Build a parser from grammar text.
    ///
    /// Accepts the same keyword arguments as Python Lark's `Lark(...)` for the
    /// options the Rust engine supports today.
    #[new]
    #[pyo3(signature = (
        grammar,
        *,
        parser="earley",
        lexer="auto",
        start=None,
        ambiguity="resolve",
        propagate_positions=false,
        keep_all_tokens=false,
        maybe_placeholders=true,
        strict=false,
        g_regex_flags=0,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        grammar: &str,
        parser: &str,
        lexer: &str,
        start: Option<&Bound<'_, PyAny>>,
        ambiguity: &str,
        propagate_positions: bool,
        keep_all_tokens: bool,
        maybe_placeholders: bool,
        strict: bool,
        g_regex_flags: u32,
    ) -> PyResult<Self> {
        let start = parse_start(start)?;
        let options = LarkOptions {
            start,
            parser: parse_parser(parser)?,
            lexer: parse_lexer(lexer)?,
            ambiguity: parse_ambiguity(ambiguity)?,
            propagate_positions,
            keep_all_tokens,
            maybe_placeholders,
            strict,
            g_regex_flags,
            base_path: None,
            // Python callers have a real filesystem; in-memory import sources
            // (#153) are a WASM-binding affordance. Python Lark's own
            // `import_paths` loaders could map here if ever needed.
            import_sources: None,
            postlex: None,
            // No Python kwarg — the binding always uses the default (regex) scanner
            // backend; the DFA backend is an internal lark-rs knob (LEXER_DFA_PLAN).
            lexer_backend: Default::default(),
        };
        let inner = Lark::new(grammar, options).map_err(map_lark_error)?;
        Ok(PyLark { inner })
    }

    /// Parse `text` and return a `Tree` (or a bare `Token` for a collapsing
    /// `?start` rule, exactly as Python Lark does).
    #[pyo3(signature = (text, start=None))]
    fn parse(&self, py: Python<'_>, text: &str, start: Option<&str>) -> PyResult<PyObject> {
        let result = match start {
            Some(s) => self.inner.parse_with_start(text, s),
            None => self.inner.parse(text),
        }
        .map_err(map_parse_error)?;
        parse_tree_to_py(py, &result)
    }
}

fn parse_tree_to_py(py: Python<'_>, pt: &ParseTree) -> PyResult<PyObject> {
    match pt {
        ParseTree::Tree(t) => Ok(PyTree::from_tree(py, t)?.into_any()),
        ParseTree::Token(tok) => token_to_py(py, tok),
        // A bare `None` root (`?start: [A]` on `""`, #289) maps to Python's literal
        // `None`, exactly what Python Lark returns for that collapse.
        ParseTree::None => Ok(py.None()),
    }
}

// ─── Option parsing helpers ─────────────────────────────────────────────────

fn parse_start(start: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<String>> {
    let Some(obj) = start else {
        return Ok(vec!["start".to_string()]);
    };
    if let Ok(s) = obj.extract::<String>() {
        return Ok(vec![s]);
    }
    if let Ok(v) = obj.extract::<Vec<String>>() {
        if v.is_empty() {
            return Err(GrammarError::new_err("start must not be empty"));
        }
        return Ok(v);
    }
    Err(GrammarError::new_err(
        "start must be a string or a list of strings",
    ))
}

fn parse_parser(s: &str) -> PyResult<ParserAlgorithm> {
    match s {
        "earley" => Ok(ParserAlgorithm::Earley),
        "lalr" => Ok(ParserAlgorithm::Lalr),
        "cyk" => Ok(ParserAlgorithm::Cyk),
        other => Err(GrammarError::new_err(format!(
            "unknown parser {other:?} (expected 'earley', 'lalr', or 'cyk')"
        ))),
    }
}

fn parse_lexer(s: &str) -> PyResult<LexerType> {
    match s {
        "auto" => Ok(LexerType::Auto),
        // Python Lark renamed 'standard' to 'basic'; accept both.
        "basic" | "standard" => Ok(LexerType::Basic),
        "contextual" => Ok(LexerType::Contextual),
        "dynamic" => Ok(LexerType::Dynamic),
        "dynamic_complete" => Ok(LexerType::DynamicComplete),
        other => Err(GrammarError::new_err(format!(
            "unknown lexer {other:?} (expected 'auto', 'basic', 'contextual', \
             'dynamic', or 'dynamic_complete')"
        ))),
    }
}

fn parse_ambiguity(s: &str) -> PyResult<Ambiguity> {
    match s {
        "resolve" => Ok(Ambiguity::Resolve),
        "explicit" => Ok(Ambiguity::Explicit),
        "forest" => Ok(Ambiguity::Forest),
        other => Err(GrammarError::new_err(format!(
            "unknown ambiguity {other:?} (expected 'resolve', 'explicit', or 'forest')"
        ))),
    }
}

// ─── Module ─────────────────────────────────────────────────────────────────

#[pymodule]
fn _lark_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyLark>()?;
    m.add_class::<PyTree>()?;
    m.add("LarkError", m.py().get_type::<LarkError>())?;
    m.add("GrammarError", m.py().get_type::<GrammarError>())?;
    m.add("ParseError", m.py().get_type::<ParseError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
