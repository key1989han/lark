//! Token position correctness (BUG-5): columns are char-based (not byte-based) and
//! end_line/end_column account for newlines inside a token. Expected values are
//! taken from Python Lark (the oracle), which our tree oracles do not capture.

mod common;

use common::make_lalr;
use lark_rs::Child;

/// `BLOCK` spans a newline and `café` contains a multi-byte char, so this exercises
/// both the multi-line and the non-ASCII path at once.
const GRAMMAR: &str = r#"
start: BLOCK NAME
BLOCK: /<[^>]*>/
NAME: /\w+/
%ignore /[ \n]+/
"#;

fn tok(children: &[Child], i: usize) -> &lark_rs::Token {
    match &children[i] {
        Child::Token(t) => t,
        other => panic!("child {i} is not a token: {other:?}"),
    }
}

#[test]
fn test_token_positions_multiline_and_unicode() {
    let lark = make_lalr(GRAMMAR);
    let result = lark.parse("<a\nbc>\ncafé").expect("parse");
    let tree = result
        .as_tree()
        .expect("start rule is `start: BLOCK NAME`, so the root is a tree");
    let c = &tree.children;
    assert_eq!(c.len(), 2, "expected BLOCK NAME");

    // BLOCK = "<a\nbc>": starts (1,1), the embedded newline pushes the end onto
    // line 2; "bc>" ends at column 4. (Python Lark: end_line=2, end_column=4.)
    let block = tok(c, 0);
    assert_eq!(block.type_, "BLOCK");
    assert_eq!((block.line(), block.column()), (1, 1));
    assert_eq!(
        (block.end_line(), block.end_column()),
        (2, 4),
        "multi-line token end position wrong"
    );

    // NAME = "café": 4 chars but 5 bytes (é is 2 bytes). end_column must be
    // char-based: 1 + 4 = 5, not 1 + 5. (Python Lark: line=3, end_column=5.)
    let name = tok(c, 1);
    assert_eq!(name.type_, "NAME");
    assert_eq!((name.line(), name.column()), (3, 1));
    assert_eq!(
        (name.end_line(), name.end_column()),
        (3, 5),
        "non-ASCII token end_column must count chars, not bytes"
    );
}
