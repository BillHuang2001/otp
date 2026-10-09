// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! Lexer and recursive-descent / Pratt parser for the `-eval` Erlang grammar.
//!
//! The grammar is the *expression* subset of Erlang — everything `erl_eval` can
//! be handed — with Erlang's exact operator precedence and associativity
//! (`erl_parse.yrl`: `catch` lowest, then `=`/`!`, `orelse`, `andalso`, the
//! comparison family, `++`/`--`, the additive family, the multiplicative family,
//! then the prefix operators). A trailing `.` (the shell statement terminator) is
//! tolerated so the parser is robust even when the CLI did not strip it.
//!
//! Parsing is a pure, total function of the input text: every failure is a typed
//! [`BeamError`] built by [`crate::error::bad_eval`] (a capability-gap raise), so
//! the CLI reports it as *unsupported* (exit 3) rather than crashing.

use beam_common::Result;

use super::ast::*;
use crate::error;

/// The lexer's token set.
///
/// Keywords (`case`, `fun`, `when`, …) are lexed as [`Tok::Atom`]; the parser
/// distinguishes them by position, exactly as the Erlang scanner does.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// An integer literal (an exact `i128`, so a value past 64 bits is still exact).
    Int(i128),
    /// A float literal.
    Float(f64),
    /// A string literal (already unescaped).
    Str(String),
    /// An atom (bare or quoted) — or a keyword.
    Atom(String),
    /// A variable.
    Var(String),
    /// A punctuation / operator token, looked up by its spelling.
    Punct(&'static str),
}

/// The multi-character operator/punctuation spellings, longest first.
const PUNCT: &[&str] = &[
    "=:=", "=/=", "->", "=>", ":=", "==", "=<", ">=", "<-", "<=", "++", "--", "<<", ">>", "||", "#{",
    "(", ")", "[", "]", "{", "}", ",", ";", ".", "|", "#", ":", "=", "<", ">", "+", "-", "*", "/",
    "!",
];

/// The scanning pass over the source text.
struct Lexer<'a> {
    /// The original source (for diagnostics).
    src: &'a str,
    /// The source as characters (so multi-byte input indexes cleanly).
    ch: Vec<char>,
    /// The current character offset.
    pos: usize,
}

impl<'a> Lexer<'a> {
    /// A lexer positioned at the start of `src`.
    fn new(src: &'a str) -> Lexer<'a> {
        Lexer {
            src,
            ch: src.chars().collect(),
            pos: 0,
        }
    }

    /// The current character, if any.
    fn peek(&self) -> Option<char> {
        self.ch.get(self.pos).copied()
    }

    /// The character `n` positions ahead, if any.
    fn peek_at(&self, n: usize) -> Option<char> {
        self.ch.get(self.pos + n).copied()
    }

    /// Consumes and returns the current character.
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// A parse diagnostic for this source.
    fn err(&self, detail: &str) -> beam_common::BeamError {
        error::bad_eval(self.src, detail)
    }

    /// Skips whitespace and `%…` comments.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => self.pos += 1,
                Some('%') => {
                    while let Some(c) = self.bump() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    /// Scans the whole input into a token vector.
    fn tokenize(mut self) -> Result<Vec<Tok>> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia();
            let Some(c) = self.peek() else { return Ok(out) };
            if let Some(tok) = self.punct() {
                out.push(tok);
            } else if c.is_ascii_digit() {
                out.push(self.number()?);
            } else if c == '$' {
                self.pos += 1;
                let code = self.char_code()?;
                out.push(Tok::Int(i128::from(code)));
            } else if c == '"' {
                out.push(Tok::Str(self.quoted('"')?));
            } else if c == '\'' {
                out.push(Tok::Atom(self.quoted('\'')?));
            } else if c.is_alphabetic() || c == '_' {
                out.push(self.word()?);
            } else {
                return Err(self.err(&format!("unexpected character {c:?}")));
            }
        }
    }

    /// Tries to scan a punctuation/operator token.
    fn punct(&mut self) -> Option<Tok> {
        for spelling in PUNCT {
            let chars: Vec<char> = spelling.chars().collect();
            if self
                .ch
                .get(self.pos..self.pos + chars.len())
                .is_some_and(|slice| slice == chars.as_slice())
            {
                self.pos += chars.len();
                return Some(Tok::Punct(spelling));
            }
        }
        None
    }

    /// `[A-Za-z_][A-Za-z0-9_@]*` → a variable (upper-case/`_` start) or an atom.
    fn word(&mut self) -> Result<Tok> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' || c == '@' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text: String = self.ch[start..self.pos].iter().collect();
        Ok(if is_var_name(&text) {
            Tok::Var(text)
        } else {
            Tok::Atom(text)
        })
    }

    /// A decimal integer, `Base#Digits` or a float.
    fn number(&mut self) -> Result<Tok> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        let head: String = self.ch[start..self.pos].iter().collect();

        if self.peek() == Some('#') {
            // `Base#Digits`: the leading digits are the radix.
            let base: u32 = head
                .parse()
                .map_err(|_| self.err("bad radix in Base#Digits"))?;
            if !(2..=36).contains(&base) {
                return Err(self.err("digit base must be between 2 and 36"));
            }
            self.pos += 1;
            let digits_start = self.pos;
            while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
                self.pos += 1;
            }
            let digits: String = self.ch[digits_start..self.pos].iter().collect();
            if digits.is_empty() {
                return Err(self.err("Base#Digits needs digits"));
            }
            let value = i128::from_str_radix(&digits.chars().filter(|c| *c != '_').collect::<String>(), base)
                .map_err(|_| self.err("Base#Digits is out of range"))?;
            return Ok(Tok::Int(value));
        }

        // A float needs a digit on both sides of the `.`.
        if self.peek() == Some('.') && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
            if matches!(self.peek(), Some('e' | 'E')) {
                let save = self.pos;
                self.pos += 1;
                if matches!(self.peek(), Some('+' | '-')) {
                    self.pos += 1;
                }
                if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                        self.pos += 1;
                    }
                } else {
                    self.pos = save;
                }
            }
            let text: String = self.ch[start..self.pos].iter().collect();
            let value: f64 = text
                .parse()
                .map_err(|_| self.err("not a float literal"))?;
            return Ok(Tok::Float(value));
        }

        let value: i128 = head
            .parse()
            .map_err(|_| self.err("integer literal is out of range"))?;
        Ok(Tok::Int(value))
    }

    /// A single character code following `$`.
    fn char_code(&mut self) -> Result<u32> {
        match self.bump() {
            None => Err(self.err("unterminated character literal")),
            Some('\\') => Ok(self.escape()? as u32),
            Some(c) => Ok(c as u32),
        }
    }

    /// A `"…"` string or `'…'` quoted atom (the quote is the current character).
    fn quoted(&mut self, quote: char) -> Result<String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated quoted literal")),
                Some(c) if c == quote => return Ok(out),
                Some('\\') => out.push(self.escape()?),
                Some(c) => out.push(c),
            }
        }
    }

    /// The character denoted by an escape sequence (the `\` was consumed).
    fn escape(&mut self) -> Result<char> {
        match self.bump() {
            None => Err(self.err("unterminated escape sequence")),
            Some('n') => Ok('\n'),
            Some('t') => Ok('\t'),
            Some('r') => Ok('\r'),
            Some('b') => Ok('\u{8}'),
            Some('f') => Ok('\u{c}'),
            Some('v') => Ok('\u{b}'),
            Some('e') => Ok('\u{1b}'),
            Some('s') => Ok(' '),
            Some('d') => Ok('\u{7f}'),
            Some('\\') => Ok('\\'),
            Some('\'') => Ok('\''),
            Some('"') => Ok('"'),
            Some('x') => self.hex_escape(),
            Some(c) if c.is_ascii_digit() => {
                // Octal: up to three digits, the first already consumed.
                let mut value = c.to_digit(8).ok_or_else(|| self.err("bad octal escape"))?;
                for _ in 0..2 {
                    match self.peek() {
                        Some(d) if d.is_ascii_digit() && d < '8' => {
                            value = value * 8 + d.to_digit(8).unwrap_or(0);
                            self.pos += 1;
                        }
                        _ => break,
                    }
                }
                char::from_u32(value).ok_or_else(|| self.err("escape is not a character"))
            }
            Some(c) => Ok(c),
        }
    }

    /// `\xHH` / `\x{…}` (the `\x` was consumed).
    fn hex_escape(&mut self) -> Result<char> {
        let mut value = 0u32;
        if self.peek() == Some('{') {
            self.pos += 1;
            let mut digits = 0;
            while let Some(c) = self.peek() {
                if c == '}' {
                    break;
                }
                let d = c.to_digit(16).ok_or_else(|| self.err("bad \\x{…} escape"))?;
                value = value.saturating_mul(16).saturating_add(d);
                self.pos += 1;
                digits += 1;
            }
            if self.peek() != Some('}') || digits == 0 {
                return Err(self.err("unterminated \\x{…} escape"));
            }
            self.pos += 1;
        } else {
            let mut digits = 0;
            while digits < 2 {
                match self.peek() {
                    Some(c) => {
                        let d = c.to_digit(16).ok_or_else(|| self.err("bad \\xHH escape"))?;
                        value = value * 16 + d;
                        self.pos += 1;
                        digits += 1;
                    }
                    None => break,
                }
            }
            if digits == 0 {
                return Err(self.err("bad \\xHH escape"));
            }
        }
        char::from_u32(value).ok_or_else(|| self.err("escape is not a character"))
    }
}

/// The Erlang operator a token denotes **in infix position**, with its binding
/// powers (`lbp`, `rbp`; left-associative = `rbp = lbp + 1`, right = `rbp = lbp - 1`).
fn binop_of(tok: &Tok, in_segment: bool) -> Option<(BinOp, u8, u8)> {
    let name: &str = match tok {
        Tok::Punct(p) => p,
        Tok::Atom(a) => a.as_str(),
        _ => return None,
    };
    Some(match name {
        "=" => (BinOp::Match, 2, 1),
        "!" => (BinOp::Send, 2, 1),
        "orelse" => (BinOp::OrElse, 4, 3),
        "andalso" => (BinOp::AndAlso, 6, 5),
        "==" => (BinOp::Eq, 7, 8),
        "/=" => (BinOp::NotEq, 7, 8),
        "=:=" => (BinOp::ExactEq, 7, 8),
        "=/=" => (BinOp::ExactNotEq, 7, 8),
        "<" => (BinOp::Lt, 7, 8),
        "=<" => (BinOp::LtEq, 7, 8),
        ">" => (BinOp::Gt, 7, 8),
        ">=" => (BinOp::GtEq, 7, 8),
        "++" => (BinOp::ListConcat, 10, 9),
        "--" => (BinOp::ListSubtract, 10, 9),
        "+" => (BinOp::Add, 11, 12),
        "-" => (BinOp::Sub, 11, 12),
        "bor" => (BinOp::Bor, 11, 12),
        "bxor" => (BinOp::Bxor, 11, 12),
        "bsl" => (BinOp::Bsl, 11, 12),
        "bsr" => (BinOp::Bsr, 11, 12),
        "or" => (BinOp::Or, 11, 12),
        "xor" => (BinOp::Xor, 11, 12),
        "*" => (BinOp::Mul, 13, 14),
        "/" if !in_segment => (BinOp::FloatDiv, 13, 14),
        "div" => (BinOp::IntDiv, 13, 14),
        "rem" => (BinOp::Rem, 13, 14),
        "band" => (BinOp::Band, 13, 14),
        "and" => (BinOp::And, 13, 14),
        _ => return None,
    })
}

/// The token stream parser.
struct Parser<'a> {
    /// The original source (for diagnostics).
    src: &'a str,
    /// The tokens.
    toks: Vec<Tok>,
    /// The current token offset.
    pos: usize,
    /// Whether `/` must be read as a binary segment specifier, not division.
    in_segment: bool,
    /// Whether `Atom:Function` must NOT be read as a remote call — set while
    /// parsing a `catch` clause head, where `Class:Reason` is a pattern pair.
    no_remote: bool,
}

/// Parses an `-eval` expression.
///
/// # Errors
///
/// [`BeamError`](beam_common::BeamError) carrying a parse diagnostic for anything
/// outside the expression grammar (a capability gap, exit 3 in the CLI).
pub fn parse(src: &str) -> Result<Expr> {
    let toks = Lexer::new(src).tokenize()?;
    let mut parser = Parser {
        src,
        toks,
        pos: 0,
        in_segment: false,
        no_remote: false,
    };
    let expr = parser.parse_body()?;
    // Tolerate one trailing `.` even if the CLI did not strip it.
    let _ = parser.eat_punct(".");
    if !parser.at_end() {
        let found = parser.peek().cloned();
        return Err(error::bad_eval(src, &format!("unexpected {found:?}")));
    }
    // `-eval` takes a comma sequence (`X = 7, X + 1`); a single expression stays bare
    // so its tree is exactly what a caller expects.
    let mut exprs = expr;
    if exprs.len() == 1 {
        Ok(exprs.pop().unwrap_or(Expr::Nil))
    } else {
        Ok(Expr::Block(exprs))
    }
}

impl<'a> Parser<'a> {
    /// A parse diagnostic for this source.
    fn err(&self, detail: &str) -> beam_common::BeamError {
        error::bad_eval(self.src, detail)
    }

    /// Whether the token stream is exhausted.
    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    /// The current token, if any.
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    /// The token `n` positions ahead, if any.
    fn peek_at(&self, n: usize) -> Option<&Tok> {
        self.toks.get(self.pos + n)
    }

    /// Consumes the current token.
    fn bump(&mut self) -> Option<Tok> {
        let tok = self.toks.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    /// Whether the current token is the punctuation `spelling`.
    fn is_punct(&self, spelling: &str) -> bool {
        matches!(self.peek(), Some(Tok::Punct(p)) if *p == spelling)
    }

    /// Whether the current token is the bare atom/keyword `name`.
    fn is_atom(&self, name: &str) -> bool {
        matches!(self.peek(), Some(Tok::Atom(a)) if a == name)
    }

    /// Consumes the punctuation `spelling` when present.
    fn eat_punct(&mut self, spelling: &str) -> bool {
        if self.is_punct(spelling) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consumes the atom/keyword `name` when present.
    fn eat_atom(&mut self, name: &str) -> bool {
        if self.is_atom(name) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Requires the punctuation `spelling`.
    fn expect_punct(&mut self, spelling: &str) -> Result<()> {
        if self.eat_punct(spelling) {
            Ok(())
        } else {
            let found = self.peek().cloned();
            Err(self.err(&format!("expected {spelling:?}, found {found:?}")))
        }
    }

    /// Requires the atom/keyword `name`.
    fn expect_atom(&mut self, name: &str) -> Result<()> {
        if self.eat_atom(name) {
            Ok(())
        } else {
            let found = self.peek().cloned();
            Err(self.err(&format!("expected {name:?}, found {found:?}")))
        }
    }

    /// Requires an atom (bare or quoted) and returns its name.
    fn expect_atom_name(&mut self) -> Result<String> {
        match self.bump() {
            Some(Tok::Atom(name)) => Ok(name),
            other => Err(self.err(&format!("expected an atom, found {other:?}"))),
        }
    }

    /// Requires an integer literal that fits an `i64` (a segment `unit:` or a fun arity).
    fn expect_int(&mut self) -> Result<i64> {
        match self.bump() {
            Some(Tok::Int(value)) => {
                i64::try_from(value).map_err(|_| self.err("integer literal is out of range"))
            }
            other => Err(self.err(&format!("expected an integer, found {other:?}"))),
        }
    }

    /// Runs `f` with the binary-segment flag cleared.
    fn with_seg_off<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.with_segment_flag(false, f)
    }

    /// Runs `f` with the binary-segment flag set.
    ///
    /// Inside a segment a `/` opens the type specifier rather than the division
    /// operator (`binop_of` guards `FloatDiv` on the flag), so a segment's value **and**
    /// its `:Size` are parsed with the flag set — `<<255:12/little>>` is the low 12 bits
    /// of 255 laid out little-endian, not `12 / little`.
    fn with_seg_on<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.with_segment_flag(true, f)
    }

    /// Runs `f` with the binary-segment flag set to `flag`, restoring it afterwards.
    fn with_segment_flag<T>(
        &mut self,
        flag: bool,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let saved = self.in_segment;
        self.in_segment = flag;
        let result = f(self);
        self.in_segment = saved;
        result
    }

    /// Parses an expression with the Pratt algorithm, stopping at any operator
    /// binding looser than `min_bp`.
    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr> {
        let mut lhs = self.parse_prefix()?;
        // Postfix call application: `(fun … end)(Args)`, i.e. calling a fun-valued
        // expression. A *named* call was already consumed in `parse_primary`.
        while self.is_punct("(") {
            self.pos += 1;
            let args = self.parse_call_args()?;
            lhs = Expr::Apply(Box::new(lhs), args);
        }
        loop {
            let Some((op, lbp, rbp)) = self.peek().and_then(|t| binop_of(t, self.in_segment)) else {
                break;
            };
            if lbp < min_bp {
                break;
            }
            self.pos += 1;
            let rhs = self.parse_expr(rbp)?;
            lhs = Expr::Op(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Parses a prefix-operator application or a primary.
    fn parse_prefix(&mut self) -> Result<Expr> {
        if self.is_punct("+") {
            self.pos += 1;
            return Ok(Expr::Unary(UnOp::Plus, Box::new(self.parse_expr(15)?)));
        }
        if self.is_punct("-") {
            self.pos += 1;
            return Ok(Expr::Unary(UnOp::Minus, Box::new(self.parse_expr(15)?)));
        }
        if self.is_atom("not") {
            self.pos += 1;
            return Ok(Expr::Unary(UnOp::Not, Box::new(self.parse_expr(15)?)));
        }
        if self.is_atom("bnot") {
            self.pos += 1;
            return Ok(Expr::Unary(UnOp::Bnot, Box::new(self.parse_expr(15)?)));
        }
        if self.is_atom("catch") {
            self.pos += 1;
            return Ok(Expr::Catch(Box::new(self.parse_expr(0)?)));
        }
        self.parse_primary()
    }

    /// Parses a primary expression (including the keyword-led control forms).
    fn parse_primary(&mut self) -> Result<Expr> {
        let tok = self
            .bump()
            .ok_or_else(|| self.err("unexpected end of expression"))?;
        match tok {
            Tok::Int(value) => Ok(Expr::Int(value)),
            Tok::Float(value) => Ok(Expr::Float(value)),
            Tok::Str(text) => Ok(Expr::Str(text)),
            Tok::Var(name) => {
                if self.eat_punct("(") {
                    let args = self.parse_call_args()?;
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Var(name))
                }
            }
            Tok::Atom(name) => match name.as_str() {
                "case" => self.parse_case(),
                "if" => self.parse_if(),
                "begin" => self.parse_begin(),
                "try" => self.parse_try(),
                "receive" => self.parse_receive(),
                "fun" => self.parse_fun(),
                "end" | "of" | "when" | "after" => {
                    Err(self.err(&format!("unexpected keyword {name:?}")))
                }
                _ => self.after_atom(name),
            },
            Tok::Punct("(") => self.with_seg_off(|p| p.parse_paren()),
            Tok::Punct("[") => self.with_seg_off(|p| p.parse_list()),
            Tok::Punct("{") => self.with_seg_off(|p| p.parse_tuple()),
            Tok::Punct("#{") => self.with_seg_off(|p| p.parse_map()),
            Tok::Punct("<<") => self.parse_binary(),
            Tok::Punct("#") => Err(self.err("record syntax is not supported")),
            other => Err(self.err(&format!("unexpected {other:?}"))),
        }
    }

    /// Continues after a bare atom: a local call, a remote call, or the atom.
    fn after_atom(&mut self, name: String) -> Result<Expr> {
        if !self.no_remote && self.eat_punct(":") {
            let function = self.expect_atom_name()?;
            self.expect_punct("(")?;
            let args = self.parse_call_args()?;
            return Ok(Expr::Remote(name, function, args));
        }
        if self.eat_punct("(") {
            let args = self.parse_call_args()?;
            return Ok(Expr::Call(name, args));
        }
        Ok(Expr::Atom(name))
    }

    /// Parses a call's arguments; the `(` was already consumed.
    fn parse_call_args(&mut self) -> Result<Vec<Expr>> {
        self.with_seg_off(|p| {
            let mut args = Vec::new();
            if p.eat_punct(")") {
                return Ok(args);
            }
            args.push(p.parse_expr(0)?);
            while p.eat_punct(",") {
                args.push(p.parse_expr(0)?);
            }
            p.expect_punct(")")?;
            Ok(args)
        })
    }

    /// Parses a parenthesised expression or comma sequence; the `(` was consumed.
    fn parse_paren(&mut self) -> Result<Expr> {
        let mut exprs = vec![self.parse_expr(0)?];
        while self.eat_punct(",") {
            exprs.push(self.parse_expr(0)?);
        }
        self.expect_punct(")")?;
        if exprs.len() == 1 {
            Ok(exprs.pop().unwrap_or(Expr::Nil))
        } else {
            Ok(Expr::Block(exprs))
        }
    }

    /// Parses a list, cons or list comprehension; the `[` was consumed.
    fn parse_list(&mut self) -> Result<Expr> {
        if self.eat_punct("]") {
            return Ok(Expr::Nil);
        }
        let first = self.parse_expr(0)?;
        if self.eat_punct("||") {
            let quals = self.parse_quals()?;
            self.expect_punct("]")?;
            return Ok(Expr::ListComp(Box::new(first), quals));
        }
        let mut elems = vec![first];
        while self.eat_punct(",") {
            elems.push(self.parse_expr(0)?);
        }
        // An optional `| Tail` after the elements makes the list improper (`[1,2|3]`),
        // exactly as `erl_parse`'s `list -> '[' … '|' expr ']'` rule does.
        let tail = if self.eat_punct("|") {
            Some(Box::new(self.parse_expr(0)?))
        } else {
            None
        };
        self.expect_punct("]")?;
        Ok(Expr::List(elems, tail))
    }

    /// Parses a tuple; the `{` was consumed.
    fn parse_tuple(&mut self) -> Result<Expr> {
        let mut elems = Vec::new();
        if self.eat_punct("}") {
            return Ok(Expr::Tuple(elems));
        }
        elems.push(self.parse_expr(0)?);
        while self.eat_punct(",") {
            elems.push(self.parse_expr(0)?);
        }
        self.expect_punct("}")?;
        Ok(Expr::Tuple(elems))
    }

    /// Parses a map construction or map comprehension; the `#{` was consumed.
    fn parse_map(&mut self) -> Result<Expr> {
        if self.eat_punct("}") {
            return Ok(Expr::Map(Vec::new()));
        }
        let (first, key, value) = self.parse_map_field()?;
        if self.eat_punct("||") {
            let quals = self.parse_quals()?;
            self.expect_punct("}")?;
            return Ok(Expr::MapComp(Box::new(key), Box::new(value), quals));
        }
        let mut fields = vec![first];
        while self.eat_punct(",") {
            let (field, _, _) = self.parse_map_field()?;
            fields.push(field);
        }
        self.expect_punct("}")?;
        Ok(Expr::Map(fields))
    }

    /// One `Key => Value` / `Key := Value` field (returned whole and split out).
    fn parse_map_field(&mut self) -> Result<(MapField, Expr, Expr)> {
        let key = self.parse_expr(0)?;
        let update = if self.eat_punct("=>") {
            false
        } else if self.eat_punct(":=") {
            true
        } else {
            let found = self.peek().cloned();
            return Err(self.err(&format!("expected \"=>\" or \":=\", found {found:?}")));
        };
        let value = self.parse_expr(0)?;
        Ok((
            MapField {
                key: key.clone(),
                value: value.clone(),
                update,
            },
            key,
            value,
        ))
    }

    /// Parses a binary construction or comprehension; the `<<` was consumed.
    fn parse_binary(&mut self) -> Result<Expr> {
        let mut segs = Vec::new();
        if self.eat_punct(">>") {
            return Ok(Expr::Binary(segs));
        }
        segs.push(self.parse_segment()?);
        if self.eat_punct("||") {
            let quals = self.parse_quals()?;
            self.expect_punct(">>")?;
            return Ok(Expr::BinComp(segs, quals));
        }
        while self.eat_punct(",") {
            segs.push(self.parse_segment()?);
        }
        self.expect_punct(">>")?;
        Ok(Expr::Binary(segs))
    }

    /// One binary segment.
    fn parse_segment(&mut self) -> Result<Seg> {
        let value = self.with_seg_on(|p| p.parse_expr(0))?;

        let size = if self.eat_punct(":") {
            Some(self.with_seg_on(|p| p.parse_expr(0))?)
        } else {
            None
        };

        let mut seg = Seg {
            value,
            size,
            kind: None,
            signed: None,
            endian: None,
            unit: None,
        };
        if self.eat_punct("/") {
            self.parse_seg_spec(&mut seg)?;
        }
        Ok(seg)
    }

    /// The `Type-Spec` list following a segment's `/`.
    fn parse_seg_spec(&mut self, seg: &mut Seg) -> Result<()> {
        loop {
            match self.peek().cloned() {
                Some(Tok::Int(bits)) => {
                    self.pos += 1;
                    seg.kind = Some(SegKind::Integer);
                    seg.size = Some(Expr::Int(bits));
                }
                Some(Tok::Atom(name)) => {
                    self.pos += 1;
                    match name.as_str() {
                        "integer" => seg.kind = Some(SegKind::Integer),
                        "float" => seg.kind = Some(SegKind::Float),
                        "binary" => seg.kind = Some(SegKind::Binary),
                        "bytes" => seg.kind = Some(SegKind::Bytes),
                        "bitstring" => seg.kind = Some(SegKind::Bitstring),
                        "bits" => seg.kind = Some(SegKind::Bits),
                        "utf8" => seg.kind = Some(SegKind::Utf8),
                        "utf16" => seg.kind = Some(SegKind::Utf16),
                        "utf32" => seg.kind = Some(SegKind::Utf32),
                        "signed" => seg.signed = Some(true),
                        "unsigned" => seg.signed = Some(false),
                        "big" => seg.endian = Some(Endian::Big),
                        "little" => seg.endian = Some(Endian::Little),
                        "native" => seg.endian = Some(Endian::Native),
                        "unit" => {
                            self.expect_punct(":")?;
                            let unit = self.expect_int()?;
                            seg.unit = Some(u32::try_from(unit).map_err(|_| self.err("bad unit"))?);
                        }
                        other => return Err(self.err(&format!("unknown segment specifier {other:?}"))),
                    }
                }
                other => return Err(self.err(&format!("bad binary segment specifier {other:?}"))),
            }
            if !self.eat_punct("-") {
                return Ok(());
            }
        }
    }

    /// A comprehension's qualifier list (up to the closing bracket).
    fn parse_quals(&mut self) -> Result<Vec<Qual>> {
        let mut quals = Vec::new();
        loop {
            quals.push(self.parse_qual()?);
            if !self.eat_punct(",") {
                return Ok(quals);
            }
        }
    }

    /// One comprehension qualifier.
    fn parse_qual(&mut self) -> Result<Qual> {
        let first = self.with_seg_off(|p| p.parse_expr(0))?;
        if self.eat_punct("<-") {
            let source = self.with_seg_off(|p| p.parse_expr(0))?;
            return Ok(Qual::Generator {
                pattern: first,
                source,
            });
        }
        if self.eat_punct("<=") {
            let source = self.with_seg_off(|p| p.parse_expr(0))?;
            return Ok(Qual::BinaryGenerator {
                pattern: first,
                source,
            });
        }
        Ok(Qual::Filter(first))
    }

    /// `case Subject of Clauses end`; `case` was already consumed.
    fn parse_case(&mut self) -> Result<Expr> {
        let subject = self.with_seg_off(|p| p.parse_expr(0))?;
        self.expect_atom("of")?;
        let clauses = self.parse_clauses()?;
        self.expect_atom("end")?;
        Ok(Expr::Case(Box::new(subject), clauses))
    }

    /// The `P [when G] -> Body; …` clause list of `case`/`receive`.
    fn parse_clauses(&mut self) -> Result<Vec<Clause>> {
        let mut clauses = Vec::new();
        loop {
            let pattern = self.with_seg_off(|p| p.parse_expr(0))?;
            let guards = if self.eat_atom("when") {
                self.parse_guards()?
            } else {
                Vec::new()
            };
            self.expect_punct("->")?;
            let body = self.parse_body()?;
            clauses.push(Clause {
                pattern: Some(pattern),
                guards,
                body,
            });
            if !self.eat_punct(";") {
                return Ok(clauses);
            }
        }
    }

    /// `if G -> Body; … end`; `if` was already consumed.
    fn parse_if(&mut self) -> Result<Expr> {
        let mut clauses = Vec::new();
        loop {
            let guards = self.parse_guards()?;
            self.expect_punct("->")?;
            let body = self.parse_body()?;
            clauses.push(Clause {
                pattern: None,
                guards,
                body,
            });
            if !self.eat_punct(";") {
                break;
            }
        }
        self.expect_atom("end")?;
        Ok(Expr::If(clauses))
    }

    /// `begin E1, E2 end`; `begin` was already consumed.
    fn parse_begin(&mut self) -> Result<Expr> {
        let body = self.parse_body()?;
        self.expect_atom("end")?;
        Ok(Expr::Block(body))
    }

    /// `try … end`; `try` was already consumed.
    fn parse_try(&mut self) -> Result<Expr> {
        let body = self.parse_body()?;
        let mut of_clauses = Vec::new();
        let mut catches = Vec::new();
        let mut after = Vec::new();
        if self.eat_atom("of") {
            of_clauses = self.parse_clauses()?;
        }
        if self.eat_atom("catch") {
            catches = self.parse_catch_clauses()?;
        }
        if self.eat_atom("after") {
            after = self.parse_body()?;
        }
        self.expect_atom("end")?;
        Ok(Expr::Try(Box::new(Try {
            body,
            of_clauses,
            catches,
            after,
        })))
    }

    /// The `C:R [:S] [when G] -> Body; …` clause list of a `try`.
    fn parse_catch_clauses(&mut self) -> Result<Vec<CatchClause>> {
        let mut clauses = Vec::new();
        loop {
            // The class pattern of `Class:Reason` is a plain atom/variable, never a
            // remote call, so suppress `Atom:Function` parsing while reading it.
            let saved_no_remote = self.no_remote;
            self.no_remote = true;
            let first = self.with_seg_off(|p| p.parse_expr(0));
            self.no_remote = saved_no_remote;
            let first = first?;
            let (class, reason, stacktrace) = if self.eat_punct(":") {
                let second = self.with_seg_off(|p| p.parse_expr(0))?;
                if self.eat_punct(":") {
                    let third = self.with_seg_off(|p| p.parse_expr(0))?;
                    (Some(first), second, Some(third))
                } else {
                    (Some(first), second, None)
                }
            } else {
                (None, first, None)
            };
            let guards = if self.eat_atom("when") {
                self.parse_guards()?
            } else {
                Vec::new()
            };
            self.expect_punct("->")?;
            let body = self.parse_body()?;
            clauses.push(CatchClause {
                class,
                reason,
                stacktrace,
                guards,
                body,
            });
            if !self.eat_punct(";") {
                return Ok(clauses);
            }
        }
    }

    /// `receive … [after T -> Body] end`; `receive` was already consumed.
    fn parse_receive(&mut self) -> Result<Expr> {
        if self.eat_atom("after") {
            let timeout = self.with_seg_off(|p| p.parse_expr(0))?;
            self.expect_punct("->")?;
            let body = self.parse_body()?;
            self.expect_atom("end")?;
            return Ok(Expr::Receive(Box::new(Receive {
                clauses: Vec::new(),
                after: Some((timeout, body)),
            })));
        }
        let clauses = self.parse_clauses()?;
        let after = if self.eat_atom("after") {
            let timeout = self.with_seg_off(|p| p.parse_expr(0))?;
            self.expect_punct("->")?;
            let body = self.parse_body()?;
            Some((timeout, body))
        } else {
            None
        };
        self.expect_atom("end")?;
        Ok(Expr::Receive(Box::new(Receive { clauses, after })))
    }

    /// `fun …`; `fun` was already consumed.
    fn parse_fun(&mut self) -> Result<Expr> {
        match self.peek().cloned() {
            Some(Tok::Punct("(")) => {
                self.pos += 1;
                let clauses = self.parse_fun_clauses()?;
                Ok(Expr::Fun {
                    name: None,
                    clauses,
                })
            }
            Some(Tok::Var(name)) if self.is_punct_at(1, "(") => {
                self.pos += 2;
                let clauses = self.parse_fun_clauses()?;
                Ok(Expr::Fun {
                    name: Some(name),
                    clauses,
                })
            }
            Some(Tok::Atom(name)) => {
                self.pos += 1;
                if self.eat_punct("/") {
                    let arity = self.expect_int()?;
                    return Ok(Expr::FunRef {
                        module: None,
                        name,
                        arity: u32::try_from(arity)
                            .map_err(|_| self.err("fun arity must be non-negative"))?,
                    });
                }
                if self.eat_punct(":") {
                    let function = self.expect_atom_name()?;
                    self.expect_punct("/")?;
                    let arity = self.expect_int()?;
                    return Ok(Expr::FunRef {
                        module: Some(name),
                        name: function,
                        arity: u32::try_from(arity)
                            .map_err(|_| self.err("fun arity must be non-negative"))?,
                    });
                }
                if self.eat_punct("(") {
                    let clauses = self.parse_fun_clauses()?;
                    return Ok(Expr::Fun {
                        name: Some(name),
                        clauses,
                    });
                }
                Err(self.err("malformed fun expression"))
            }
            other => Err(self.err(&format!("malformed fun expression: {other:?}"))),
        }
    }

    /// Whether the token `n` ahead is the punctuation `spelling`.
    fn is_punct_at(&self, n: usize, spelling: &str) -> bool {
        matches!(self.peek_at(n), Some(Tok::Punct(p)) if *p == spelling)
    }

    /// The clause list of a `fun` whose first `(` has already been consumed.
    fn parse_fun_clauses(&mut self) -> Result<Vec<FunClause>> {
        let mut clauses = Vec::new();
        loop {
            let clause = self.parse_fun_clause()?;
            clauses.push(clause);
            if !self.eat_punct(";") {
                break;
            }
            // Subsequent clauses repeat the `(Params)` shape; a NAMED fun also
            // repeats its name (`fun F(1) -> a; F(N) -> b end`).
            if matches!(self.peek(), Some(Tok::Var(_))) && self.is_punct_at(1, "(") {
                self.pos += 2;
            } else {
                self.expect_punct("(")?;
            }
        }
        self.expect_atom("end")?;
        Ok(clauses)
    }

    /// One `(Params) [when G] -> Body` fun clause (the `(` was consumed).
    fn parse_fun_clause(&mut self) -> Result<FunClause> {
        let mut params = Vec::new();
        if !self.eat_punct(")") {
            params.push(self.with_seg_off(|p| p.parse_expr(0))?);
            while self.eat_punct(",") {
                params.push(self.with_seg_off(|p| p.parse_expr(0))?);
            }
            self.expect_punct(")")?;
        }
        let guards = if self.eat_atom("when") {
            self.parse_guards()?
        } else {
            Vec::new()
        };
        self.expect_punct("->")?;
        let body = self.parse_body()?;
        Ok(FunClause {
            params,
            guards,
            body,
        })
    }

    /// A comma sequence of expressions (a clause body / `begin` body).
    fn parse_body(&mut self) -> Result<Vec<Expr>> {
        let mut body = vec![self.with_seg_off(|p| p.parse_expr(0))?];
        while self.eat_punct(",") {
            body.push(self.with_seg_off(|p| p.parse_expr(0))?);
        }
        Ok(body)
    }

    /// The guard sequence `G1, G2 ; G3` (outer `;` = or, inner `,` = and).
    fn parse_guards(&mut self) -> Result<Vec<Vec<Expr>>> {
        let mut alternatives = Vec::new();
        let mut current = vec![self.with_seg_off(|p| p.parse_expr(0))?];
        loop {
            if self.eat_punct(",") {
                current.push(self.with_seg_off(|p| p.parse_expr(0))?);
            } else if self.eat_punct(";") {
                alternatives.push(std::mem::take(&mut current));
                current.push(self.with_seg_off(|p| p.parse_expr(0))?);
            } else {
                break;
            }
        }
        alternatives.push(current);
        Ok(alternatives)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(src: &str) -> Expr {
        parse(src).unwrap_or_else(|error| panic!("{src:?} must parse: {error}"))
    }

    #[test]
    fn literals_parse() {
        assert_eq!(parsed("42"), Expr::Int(42));
        assert_eq!(parsed("16#FF"), Expr::Int(255));
        assert_eq!(parsed("2#1010"), Expr::Int(10));
        assert_eq!(parsed("1.5"), Expr::Float(1.5));
        assert_eq!(parsed("1.0e3"), Expr::Float(1000.0));
        assert_eq!(parsed("hello"), Expr::Atom("hello".into()));
        assert_eq!(parsed("'a b'"), Expr::Atom("a b".into()));
        assert_eq!(parsed("\"hi\\n\""), Expr::Str("hi\n".into()));
        assert_eq!(parsed("$a"), Expr::Int(97));
        assert_eq!(parsed("$\\n"), Expr::Int(10));
        assert_eq!(parsed("X"), Expr::Var("X".into()));
        assert_eq!(parsed("_"), Expr::Var("_".into()));
        assert_eq!(parsed("[]"), Expr::Nil);
    }

    #[test]
    fn compound_literals_parse() {
        assert_eq!(parsed("{}"), Expr::Tuple(vec![]));
        assert_eq!(parsed("{a}"), Expr::Tuple(vec![Expr::Atom("a".into())]));
        assert_eq!(parsed("[]"), Expr::Nil);
        assert_eq!(
            parsed("[a|T]"),
            Expr::List(
                vec![Expr::Atom("a".into())],
                Some(Box::new(Expr::Var("T".into())))
            )
        );
        assert_eq!(parsed("#{}"), Expr::Map(vec![]));
        assert_eq!(parsed("<<>>"), Expr::Binary(vec![]));
        assert!(matches!(parsed("<<\"x\">>"), Expr::Binary(segs) if segs.len() == 1));
        assert!(matches!(parsed("<<1,2,3>>"), Expr::Binary(segs) if segs.len() == 3));
        assert!(matches!(parsed("<<X:8/little-integer>>"), Expr::Binary(segs) if segs.len() == 1));
    }

    #[test]
    fn operator_precedence_matches_erlang() {
        // `2*3+4` = 10, i.e. `*` binds tighter than `+`.
        let mul = parsed("2*3+4");
        assert_eq!(
            mul,
            Expr::Op(
                BinOp::Add,
                Box::new(Expr::Op(
                    BinOp::Mul,
                    Box::new(Expr::Int(2)),
                    Box::new(Expr::Int(3))
                )),
                Box::new(Expr::Int(4))
            )
        );
        // `=` is looser than `+`.
        let m = parsed("X = 1 + 2");
        assert!(matches!(m, Expr::Op(BinOp::Match, _, _)));
        // `andalso` is looser than `==`.
        let a = parsed("A == 1 andalso B == 2");
        assert!(matches!(a, Expr::Op(BinOp::AndAlso, _, _)));
        // `orelse` is looser than `andalso`.
        let o = parsed("A orelse B andalso C");
        assert!(matches!(o, Expr::Op(BinOp::OrElse, _, _)));
        // Unary minus binds tighter than `*`.
        let u = parsed("-2*3");
        assert!(matches!(
            u,
            Expr::Op(BinOp::Mul, _, _)
        ));
    }

    #[test]
    fn division_inside_a_binary_segment_is_a_specifier() {
        let e = parsed("<<X/binary, Y:8>>");
        assert!(matches!(e, Expr::Binary(ref segs) if segs.len() == 2));
    }

    #[test]
    fn control_forms_parse() {
        assert!(matches!(parsed("case X of 1 -> a; _ -> b end"), Expr::Case(..)));
        assert!(matches!(parsed("if true -> a; true -> b end"), Expr::If(_)));
        assert!(matches!(parsed("begin a, b end"), Expr::Block(_)));
        assert!(matches!(parsed("(a, b)"), Expr::Block(_)));
        assert!(matches!(parsed("catch 1/0"), Expr::Catch(_)));
        assert!(matches!(
            parsed("try f() of V -> V catch C:R -> {C,R} after g() end"),
            Expr::Try(_)
        ));
        assert!(matches!(parsed("receive X -> X after 0 -> none end"), Expr::Receive(_)));
        assert!(matches!(parsed("fun(X) -> X end"), Expr::Fun { name: None, .. }));
        assert!(matches!(
            parsed("fun F(0) -> 1; F(N) -> N end"),
            Expr::Fun { name: Some(_), .. }
        ));
        assert!(matches!(
            parsed("fun lists:reverse/1"),
            Expr::FunRef { module: Some(_), .. }
        ));
        assert!(matches!(parsed("fun length/1"), Expr::FunRef { module: None, .. }));
    }

    #[test]
    fn comprehensions_parse() {
        assert!(matches!(parsed("[X || X <- L, X > 1]"), Expr::ListComp(..)));
        assert!(matches!(parsed("#{K => V || {K,V} <- L}"), Expr::MapComp(..)));
        assert!(matches!(parsed("<<X || X <- L>>"), Expr::BinComp(..)));
    }

    #[test]
    fn a_binary_generator_uses_the_double_arrow() {
        // A `<<…>>` comprehension with a `<=` qualifier is a *binary* comprehension
        // whose qualifier is a binary generator.
        let Expr::BinComp(_, quals) = parsed("<<X || X <= L>>") else {
            panic!("`<<X || X <= L>>` must parse as a binary comprehension");
        };
        assert!(matches!(quals.as_slice(), [Qual::BinaryGenerator { .. }]));

        // … while a list generator keeps `<-` (the flag must not have repurposed it).
        let Expr::ListComp(_, quals) = parsed("[X || X <- L]") else {
            panic!("`[X || X <- L]` must parse as a list comprehension");
        };
        assert!(matches!(quals.as_slice(), [Qual::Generator { .. }]));

        // `<=` is the generator arrow, **not** a comparison: Erlang's "less than or
        // equal" operator is `=<`, which must still parse to `BinOp::LtEq`.
        assert!(matches!(parsed("A =< B"), Expr::Op(BinOp::LtEq, _, _)));
    }

    #[test]
    fn a_segment_size_does_not_swallow_its_type_specifier() {
        // A segment's size is an `expr_max`, so the `/` that follows it opens the type
        // specifier rather than the division operator: the segment keeps both a 12-bit
        // size and its `little` endianness. (Parsing the size as `12 / little` is what
        // made `<<255:12/little>>` raise `badarith`.)
        let Expr::Binary(segs) = parsed("<<255:12/little>>") else {
            panic!("`<<255:12/little>>` must parse as a binary construction");
        };
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].size, Some(Expr::Int(12)));
        assert!(segs[0].endian.is_some(), "the `little` specifier is kept");
    }

    #[test]
    fn calls_parse() {
        assert_eq!(
            parsed("length([a])"),
            Expr::Call("length".into(), vec![Expr::List(vec![Expr::Atom("a".into())], None)])
        );
        assert!(matches!(
            parsed("erlang:system_info(version)"),
            Expr::Remote(_, _, _)
        ));
        assert!(matches!(parsed("F(1)"), Expr::Call(name, _) if name == "F"));
    }

    #[test]
    fn a_multi_element_improper_list_keeps_its_tail() {
        // `[1,2|3]` is the multi-element improper list (Erlang's `list -> '[' … '|' expr ']'`);
        // a tail after the elements used to be rejected as a capability gap.
        assert_eq!(
            parsed("[1,2|3]"),
            Expr::List(
                vec![Expr::Int(1), Expr::Int(2)],
                Some(Box::new(Expr::Int(3)))
            )
        );
        // A proper list still has no tail, and a single-element improper list is unchanged.
        assert_eq!(
            parsed("[1,2]"),
            Expr::List(vec![Expr::Int(1), Expr::Int(2)], None)
        );
        assert_eq!(
            parsed("[1|2]"),
            Expr::List(vec![Expr::Int(1)], Some(Box::new(Expr::Int(2))))
        );
    }

    #[test]
    fn an_integer_literal_past_i64_stays_exact() {
        // The parser is not the range limiter: a literal past `i64` (2^63 here) keeps its
        // exact value as an `i128`, so the evaluator can materialize the bignum.
        assert_eq!(
            parsed("9223372036854775808"),
            Expr::Int(9_223_372_036_854_775_808)
        );
        assert_eq!(
            parsed("18446744073709551617"),
            Expr::Int(18_446_744_073_709_551_617)
        );
        // Past `i128` is the honest capability gap.
        let too_big = "100000000000000000000000000000000000000000000000000000";
        let error = parse(too_big).expect_err("past i128");
        assert!(crate::error::unsupported_reason(&error).is_some(), "{error}");
    }

    #[test]
    fn malformed_input_is_a_typed_error() {
        for src in ["", "1+", "(1", "case", "foo(", "{1,", "[1,", "1 2", "#r{}", "@"] {
            let error = parse(src).expect_err(&format!("{src:?} must be rejected"));
            assert!(
                crate::error::unsupported_reason(&error).is_some(),
                "{src:?}: {error}"
            );
        }
    }

    #[test]
    fn a_trailing_dot_is_tolerated() {
        assert_eq!(parse("1+2.").expect("parse"), parse("1+2").expect("parse"));
    }
}
