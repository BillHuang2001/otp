//! The `-eval` integer-expression subset (contract A.6).
//!
//! `-eval Expr` in this build evaluates a **documented integer subset**: non-negative
//! integer literals, the binary operators `+`, `-` and `*` (`*` binds tighter than
//! `+`/`-`) and parentheses, with ASCII whitespace allowed between tokens. Anything
//! else — a float, a division, an atom, a trailing operator — is a typed
//! [`BeamError`], never a panic.
//!
//! The arithmetic itself is **not** re-implemented here: every operator is dispatched
//! through the [`beam_bifs`] `erlang` registry (`erlang:'+'/2`, `erlang:'-'/2`,
//! `erlang:'*'/2`), so the semantics (including their error behaviour) are exactly the
//! ones the loaded code sees.

use beam_alloc::Heap;
use beam_bifs::{BifContext, BifRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{try_make_small, AtomTable, Eterm};

use crate::error;

/// The deepest parenthesized nesting this evaluator accepts.
///
/// The recursive-descent parser and the evaluator both walk the expression tree
/// recursively, so the depth is bounded explicitly rather than left to the process
/// stack — an absurdly nested input becomes a clear error, not a crash.
const MAX_DEPTH: usize = 64;

/// Parses and evaluates `expr` against the `erlang` BIF registry.
///
/// # Errors
///
/// * [`BeamError`] carrying a parse diagnostic ([`crate::error::bad_eval`]) for
///   anything outside the subset described in the module docs;
/// * [`BeamError::SystemLimit`] when an intermediate value leaves the 60-bit small-int
///   range (`beam_bifs` has no bignums in Milestone-1) or the nesting limit is hit;
/// * whatever the dispatched BIF itself reports.
pub fn eval(
    expr: &str,
    registry: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    self_pid: ProcessId,
) -> Result<Eterm> {
    let mut parser = Parser::new(expr);
    let tree = parser.parse_expression(0)?;
    parser.skip_whitespace();
    if !parser.at_end() {
        return Err(error::bad_eval(
            expr,
            &format!("unexpected input at offset {}", parser.offset()),
        ));
    }
    let mut evaluator = Evaluator {
        registry,
        heap,
        atoms,
        self_pid,
    };
    evaluator.value(&tree)
}

/// The parsed expression tree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    /// A non-negative integer literal.
    Int(i64),
    /// `left op right`, where `op` is `+`, `-` or `*`.
    Binary(char, Box<Node>, Box<Node>),
}

/// The recursive-descent parser.
struct Parser<'a> {
    /// The original expression (for diagnostics).
    source: &'a str,
    /// The expression bytes.
    bytes: &'a [u8],
    /// The current byte offset.
    pos: usize,
}

impl<'a> Parser<'a> {
    /// A parser positioned at the start of `source`.
    fn new(source: &'a str) -> Parser<'a> {
        Parser {
            source,
            bytes: source.as_bytes(),
            pos: 0,
        }
    }

    /// The current offset (for diagnostics).
    fn offset(&self) -> usize {
        self.pos
    }

    /// Whether the whole input has been consumed.
    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// Skips ASCII whitespace.
    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.bytes.get(self.pos) {
            if byte.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// The next non-whitespace byte, without consuming it.
    fn peek(&mut self) -> Option<u8> {
        self.skip_whitespace();
        self.bytes.get(self.pos).copied()
    }

    /// Consumes and returns the next non-whitespace byte.
    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    /// `expression := term (('+' | '-') term)*`.
    fn parse_expression(&mut self, depth: usize) -> Result<Node> {
        self.check_depth(depth)?;
        let mut left = self.parse_term(depth)?;
        while let Some(byte @ (b'+' | b'-')) = self.peek() {
            self.bump();
            let right = self.parse_term(depth)?;
            left = Node::Binary(byte as char, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `term := factor ('*' factor)*`.
    fn parse_term(&mut self, depth: usize) -> Result<Node> {
        self.check_depth(depth)?;
        let mut left = self.parse_factor(depth)?;
        while self.peek() == Some(b'*') {
            self.bump();
            let right = self.parse_factor(depth)?;
            left = Node::Binary('*', Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `factor := NUMBER | '(' expression ')'`.
    fn parse_factor(&mut self, depth: usize) -> Result<Node> {
        self.check_depth(depth)?;
        match self.peek() {
            Some(b'(') => {
                self.bump();
                let inner = self.parse_expression(depth + 1)?;
                match self.peek() {
                    Some(b')') => {
                        self.bump();
                        Ok(inner)
                    }
                    _ => Err(error::bad_eval(self.source, "missing ')'")),
                }
            }
            Some(byte) if byte.is_ascii_digit() => self.parse_number(),
            Some(byte) => Err(error::bad_eval(
                self.source,
                &format!("unexpected character {:?}", byte as char),
            )),
            None => Err(error::bad_eval(self.source, "unexpected end of expression")),
        }
    }

    /// Parses a decimal integer literal.
    fn parse_number(&mut self) -> Result<Node> {
        let start = self.pos;
        while let Some(byte) = self.bytes.get(self.pos) {
            if byte.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| error::bad_eval(self.source, "not an integer literal"))?;
        let value = text
            .parse::<i64>()
            .map_err(|_| error::bad_eval(self.source, &format!("integer {text} is out of range")))?;
        Ok(Node::Int(value))
    }

    /// Rejects an expression nested deeper than [`MAX_DEPTH`].
    fn check_depth(&self, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            Err(error::bad_eval(
                self.source,
                &format!("expression nested deeper than {MAX_DEPTH} levels"),
            ))
        } else {
            Ok(())
        }
    }
}

/// The tree-walking evaluator: dispatches every operator through the BIF registry.
struct Evaluator<'a> {
    /// The BIF registry (the only arithmetic implementation in the workspace).
    registry: &'a BifRegistry,
    /// The heap BIF results are built on.
    heap: &'a mut Heap,
    /// The atom table BIF-built atoms resolve against.
    atoms: &'a AtomTable,
    /// The calling process.
    self_pid: ProcessId,
}

impl Evaluator<'_> {
    /// Evaluates `node`.
    fn value(&mut self, node: &Node) -> Result<Eterm> {
        match node {
            Node::Int(value) => try_make_small(*value).map_err(|_| BeamError::SystemLimit),
            Node::Binary(op, left, right) => {
                let left = self.value(left)?;
                let right = self.value(right)?;
                self.apply(*op, left, right)
            }
        }
    }

    /// Applies the `erlang` BIF implementing `op` to `left` and `right`.
    fn apply(&mut self, op: char, left: Eterm, right: Eterm) -> Result<Eterm> {
        let name = match op {
            '+' => "+",
            '-' => "-",
            '*' => "*",
            other => return Err(error::unsupported(&format!("operator {other}"))),
        };
        let Evaluator {
            registry,
            heap,
            atoms,
            self_pid,
        } = self;
        let mut context = BifContext::new(heap, atoms, *self_pid);
        registry.call("erlang", name, 2, &mut context, &[left, right])
    }
}

#[cfg(test)]
mod tests {
    use beam_bifs::BifRegistry;
    use beam_term::{is_small, make_small, signed_value, AtomTable};

    use super::*;

    fn run(expr: &str) -> Result<Eterm> {
        let registry = BifRegistry::with_core_bifs();
        let mut heap = Heap::new(256).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok"]);
        let value = eval(expr, &registry, &mut heap, &atoms, ProcessId::new(1, 0));
        value.map(|term| {
            assert!(is_small(term), "the subset only produces small integers");
            make_small(signed_value(term))
        })
    }

    fn value(expr: &str) -> i64 {
        signed_value(run(expr).unwrap_or_else(|error| panic!("{expr} failed: {error}")))
    }

    #[test]
    fn literals_and_the_operator_precedence() {
        assert_eq!(value("0"), 0);
        assert_eq!(value("42"), 42);
        assert_eq!(value("2+3"), 5);
        assert_eq!(value("2*3+4"), 10);
        assert_eq!(value("4+2*3"), 10);
        assert_eq!(value("(1+2)*3"), 9);
        assert_eq!(value("10-3-2"), 5);
        assert_eq!(value("2*(3+4)*2"), 28);
        assert_eq!(value("  7  "), 7);
        assert_eq!(value("1 + 2 * ( 3 + 4 )"), 15);
        assert_eq!(value("(2+3)*(4-1)-5"), 10);
    }

    #[test]
    fn subtraction_can_go_negative() {
        assert_eq!(value("3-10"), -7);
        assert_eq!(value("(1-2)*(1-3)"), 2);
    }

    #[test]
    fn malformed_expressions_are_typed_errors() {
        for expr in [
            "",
            "abc",
            "1.5",
            "1/2",
            "2+",
            "+2",
            "*3",
            "(1+2",
            "1+2)",
            "2 3",
            "3.0*2",
            "7 div 2",
            "1e3",
            "'a'",
            "-1",
        ] {
            let result = run(expr);
            assert!(result.is_err(), "{expr:?} must be rejected");
        }
    }

    #[test]
    fn integer_overflow_of_a_literal_is_reported() {
        let huge = "9".repeat(30);
        let error = run(&huge).expect_err("out of range");
        assert!(error.to_string().contains("out of range"), "{error}");
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = format!("{}1{}", "(".repeat(200), ")".repeat(200));
        let error = run(&deep).expect_err("too deep");
        assert!(error.to_string().contains("nested"), "{error}");
    }

    #[test]
    fn arithmetic_errors_come_from_the_bif() {
        // `5 * 10^17` fits a small integer; doubling it does not, so the failure is
        // produced by the `erlang:'*'/2` BIF (`system_limit`) rather than by a check
        // re-implemented in this module.
        let error = run("0-(500000000000000000*2)").expect_err("out of range");
        assert_eq!(error.as_atom(), Some("system_limit"));
    }
}
