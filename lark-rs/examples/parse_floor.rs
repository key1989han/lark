//! Engine-floor measurement: parse control-flow cost vs output materialization.
//!
//! Parses the bench's `json_large` workload through `parse_into` with a
//! do-nothing `OutputBuilder` (Value = ()), so the run pays the lexer + LALR
//! dispatch + shaping control flow but materializes **no** output values.
//! The delta vs the default `parse()` is the absolute ceiling any
//! output-representation change (tape/arena/span/interning) could recover.
//!
//!   cargo run --release --example spike_floor
//!
//! Also times `parse()` (owned tree) and — when built with
//! `--features span-tree` — `parse_span()` for the three-point comparison.

use lark_rs::{Lark, LarkOptions, Meta, OutputBuilder, OutputContext, Token};
use std::hint::black_box;
use std::time::Instant;

const JSON_GRAMMAR: &str = r#"
    ?start: value
    ?value: object
          | array
          | string
          | SIGNED_NUMBER  -> number
          | "true"         -> true
          | "false"        -> false
          | "null"         -> null
    array  : "[" [value ("," value)*] "]"
    object : "{" [pair ("," pair)*] "}"
    pair   : string ":" value
    string : ESCAPED_STRING
    %import common.ESCAPED_STRING
    %import common.SIGNED_NUMBER
    %import common.WS
    %ignore WS
"#;

/// Do-nothing builder: every value is `()`. Everything the engine does per
/// token/reduction still happens; nothing is materialized.
struct NullBuilder;

impl<'i> OutputBuilder<'i> for NullBuilder {
    type Value = ();

    fn token(&mut self, _token: Token, _input: &'i str, _ctx: &OutputContext) {}

    fn reduce(
        &mut self,
        _rule: usize,
        _children: &mut Vec<()>,
        _meta: &Meta,
        _ctx: &OutputContext,
    ) {
    }

    fn placeholder(&mut self, _ctx: &OutputContext) {}
}

fn gen_json(records: usize) -> String {
    let mut s = String::from("[");
    for i in 0..records {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#"{{"id": {i}, "name": "user{i}", "active": true, "score": -{i}.5e2,
                "tags": ["a", "b", "c"], "address": {{"city": "x", "zip": null}}}}"#
        ));
    }
    s.push(']');
    s
}

fn time<F: FnMut()>(mut f: F, iters: usize) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..iters {
        let t = Instant::now();
        f();
        let dt = t.elapsed().as_secs_f64();
        if dt < best {
            best = dt;
        }
    }
    best
}

fn main() {
    let input = gen_json(1000);
    let bytes = input.len();
    let lark = Lark::new(JSON_GRAMMAR, LarkOptions::default()).unwrap();

    // Warm + verify both paths agree the input parses.
    lark.parse(&input).unwrap();
    lark.parse_into(&input, &mut NullBuilder).unwrap();

    let t_tree = time(
        || {
            black_box(lark.parse(&input).unwrap());
        },
        15,
    );
    let t_null = time(
        || {
            lark.parse_into(&input, &mut NullBuilder).unwrap();
        },
        15,
    );

    println!("bytes\t{bytes}");
    println!(
        "parse() owned tree\t{:.3} ms\t{:.1} MB/s",
        t_tree * 1e3,
        bytes as f64 / t_tree / 1e6
    );
    println!(
        "parse_into(Null)  \t{:.3} ms\t{:.1} MB/s",
        t_null * 1e3,
        bytes as f64 / t_null / 1e6
    );
    println!(
        "output-materialization share\t{:.1}%",
        (1.0 - t_null / t_tree) * 100.0
    );

    #[cfg(feature = "span-tree")]
    {
        let t_span = time(
            || {
                black_box(lark.parse_span(&input).unwrap());
            },
            15,
        );
        println!(
            "parse_span()      \t{:.3} ms\t{:.1} MB/s",
            t_span * 1e3,
            bytes as f64 / t_span / 1e6
        );
    }
}
