//! `io:format/1,2`, `io_lib:format/1,2` and the shared format-directive engine.
//!
//! Implements the Milestone-1 `io`/`io_lib` subset (compatibility contract Part I
//! §A.7). `Format` is a flat proper list of byte-sized small integers (`io_lib`
//! also accepts the same shape — the loader hands us bytes), `Args` a proper list.
//!
//! * `io:format/1,2` write the rendered text to the context's [`OutputSink`]
//!   ([`crate::context::BifContext::write_output`]) and return the atom `ok`.
//! * `io_lib:format/1,2` return the rendered text as an **iolist** (here a flat
//!   list of small integers, `NIL` for the empty result) and write nothing.
//!
//! Both share one directive engine ([`render_format`]), so their output is
//! identical by construction.
//!
//! # The supported directive set (verified against erts-17.1)
//!
//! `~w` (write), `~p` (print — the printable-string rule, see [`crate::print`]),
//! `~W`/`~P` (`~w`/`~p` with an explicit *depth* argument), `~s` (string: an atom's
//! name, a proper list of byte-sized small integers, or a **binary**), `~c` (a
//! character; `~tc` uses the full code point), `~n` (newline), `~~` (a literal
//! `~`), `~i` (consume and ignore one argument), `~b`/`~B` (an integer in base 10,
//! or `~.<base>b`/`~.<base>B` for base 2–36), `~x`/`~X` (an integer in `.<base>`
//! with a **two-argument** `(Integer, Prefix)` shape), and the float directives
//! `~e`/`~f`/`~g`.
//!
//! Every directive accepts the full control-sequence prefix of
//! `io_lib_format:collect_cseq/2`: an optional `-` (**left** adjust), a decimal
//! **field width** (or `*` to take it from the next argument), an optional `.`
//! **precision** (decimal or `*`), an optional `.` **pad character** (or `.*`), and
//! the `t`/`l` modifiers (`t` = unicode encoding, `l` = disable the string rule).
//! Field widths reproduce `io_lib_format:term/6` exactly: a rendering longer than
//! the effective precision P prints exactly P `*` characters padded to F, otherwise
//! it is padded to F with the pad character honouring the adjust direction.
//!
//! # What is rejected (`badarg`)
//!
//! `~d` — the OTP oracle raises `badarg` for `~d` (`io_lib:format("~d",[255])` ⇒
//! `error:badarg`); `~b` is the base-10 integer directive. Also rejected: `~+`,
//! `~#`, a base outside `2..=36`, a precision below the directive's minimum
//! (`~.1e`, `~.0f`, `~.0g`), a precision on a directive whose effective precision
//! is smaller than the field width when the directive forbids it, a wrong argument
//! type or count for a directive, running out of arguments, leaving arguments
//! unconsumed, a `~` at the end of the format, a `~-<n>p`/`~-p` (a left-adjusted
//! `~p`, which has no clause in `io_lib_format`), and a `~p`/`~P` whose single-line
//! rendering would have to be re-flowed (see below).
//!
//! # Deviations from the oracle (documented)
//!
//! * **Output is a Rust `String` (UTF-8).** Raw format bytes `>= 128` and
//!   `~s`/`~c` values `>= 128` are emitted as their UTF-8 encoding, not raw latin1
//!   bytes. The ASCII subset is byte-exact.
//! * **No `~p`/`~P` re-flow.** The oracle runs `io_lib_pretty`, which can break a
//!   term across lines to fit `line_length` (80 by default, or a `~p` field width).
//!   This engine always emits the single-line form of [`crate::print`]; when a
//!   term class that the oracle *would* re-flow (a tuple, a map, a non-printable
//!   list, or a non-printable bitstring) does not satisfy the oracle's flat
//!   condition (`length < line_length - column`), the directive is rejected with
//!   `badarg` rather than emitting a line the oracle would have broken. Terms that
//!   never re-flow (`~p` of an atom, number, ref, pid, printable list or printable
//!   binary) are always emitted.
//! * **`~k`/`~K` are rejected.** They select a *map printing order*; this engine's
//!   printer always uses the flatmap internal order, so honouring them is not
//!   possible and a silent wrong order is worse than `badarg`.
//! * **`~t` is the unicode modifier.** (The objective's wording calls `~t` a
//!   column/table directive; in erts-17.1 `~t` sets the encoding and `~tc`/`~ts`
//!   are the unicode forms — reproduced here. There is no column directive.)
//! * **Precision is the base for `~b`/`~B`/`~x`/`~X`** (as in the oracle); it is a
//!   maximum length for `~w`/`~W` and an initial column for `~p`/`~P`.
//! * Integer directives accept **small integers only** (`~b`/`~x`/`~c` of a bignum
//!   is `badarg`); the oracle would print the full bignum. A documented gap.

use beam_common::Result;
use beam_term::{atom_val, signed_value, Eterm};

use crate::access::{
    float_value, is_atom, is_float, is_list, is_map, is_small, is_tuple,
};
use crate::bits;
use crate::context::BifContext;
use crate::error::badarg;
use crate::print::{format_term, format_term_depth, proper_list, UNLIMITED_DEPTH};
use crate::registry::BifRegistry;

/// Registers `io:format/1,2` and `io_lib:format/1,2`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("io", "format", 1, format_1);
    reg.register("io", "format", 2, format_2);
    reg.register("io_lib", "format", 1, format_lib_1);
    reg.register("io_lib", "format", 2, format_lib_2);
}

/// `io:format(Format)` — writes `Format` with no arguments and returns `ok`.
fn format_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    io_format(ctx, args[0], None)
}

/// `io:format(Format, Args)` — writes `Format` with `Args` and returns `ok`.
fn format_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    io_format(ctx, args[0], Some(args[1]))
}

/// `io_lib:format(Format)` — returns the rendering of `Format` as an iolist.
fn format_lib_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    io_lib_format(ctx, args[0], None)
}

/// `io_lib:format(Format, Args)` — returns the rendering as an iolist.
fn format_lib_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    io_lib_format(ctx, args[0], Some(args[1]))
}

/// `io:format`: render, write the result, return `ok`.
fn io_format(ctx: &mut BifContext<'_>, format: Eterm, args: Option<Eterm>) -> Result<Eterm> {
    let rendered = render(ctx, format, args)?;
    ctx.write_output(&rendered)?;
    ctx.ok()
}

/// `io_lib:format`: render and return the bytes as a flat list of small integers.
fn io_lib_format(ctx: &mut BifContext<'_>, format: Eterm, args: Option<Eterm>) -> Result<Eterm> {
    let rendered = render(ctx, format, args)?;
    let elems: Vec<Eterm> = rendered
        .as_bytes()
        .iter()
        .map(|&b| ctx.int(i64::from(b)))
        .collect::<Result<Vec<_>>>()?;
    ctx.list(&elems)
}

/// The shared body: decode the format list, render against the arguments.
fn render(ctx: &mut BifContext<'_>, format: Eterm, args: Option<Eterm>) -> Result<String> {
    let bytes = format_bytes(format)?;
    let arg_terms = match args {
        Some(term) => proper_list(term).ok_or_else(badarg)?,
        None => Vec::new(),
    };
    render_format(ctx, &bytes, &arg_terms)
}

/// Decodes the format term: a flat proper list of byte-sized small integers.
fn format_bytes(format: Eterm) -> Result<Vec<u8>> {
    let elems = proper_list(format).ok_or_else(badarg)?;
    let mut bytes = Vec::with_capacity(elems.len());
    for elem in elems {
        if !is_small(elem) {
            return Err(badarg());
        }
        let value = signed_value(elem);
        if !(0..=255).contains(&value) {
            return Err(badarg());
        }
        bytes.push(value as u8);
    }
    Ok(bytes)
}

/// The control-sequence prefix shared by every directive
/// (`io_lib_format:collect_cseq/2`).
#[derive(Clone, Copy)]
struct Spec {
    /// Field width; `None` when the format gave none.
    width: Option<usize>,
    /// `~-…` left justify.
    left: bool,
    /// `.` precision (never negative).
    precision: Option<usize>,
    /// Pad character (`.`-form), default space.
    pad: u8,
    /// `~t` — unicode encoding.
    unicode: bool,
    /// `~l` — disable the `~p` string rule.
    strings: bool,
}

impl Spec {
    fn plain() -> Self {
        Spec {
            width: None,
            left: false,
            precision: None,
            pad: b' ',
            unicode: false,
            strings: true,
        }
    }
}

/// Renders `bytes` against `args`, returning the captured text.
fn render_format(ctx: &mut BifContext<'_>, bytes: &[u8], args: &[Eterm]) -> Result<String> {
    let mut out = String::new();
    let mut i = 0usize;
    let mut next_arg = 0usize;

    while i < bytes.len() {
        let byte = bytes[i];
        if byte != b'~' {
            out.push(byte as char);
            i += 1;
            continue;
        }
        i += 1;
        let (spec, ctrl) = parse_cseq(bytes, &mut i, args, &mut next_arg)?;
        let piece = dispatch(ctx, ctrl, &spec, args, &mut next_arg)?;
        out.push_str(&piece);
    }

    if next_arg != args.len() {
        return Err(badarg());
    }
    Ok(out)
}

/// Parses one `~…` control sequence prefix, returning the [`Spec`] and the control
/// character; `*` forms consume their argument from `args`.
fn parse_cseq(
    bytes: &[u8],
    i: &mut usize,
    args: &[Eterm],
    next_arg: &mut usize,
) -> Result<(Spec, u8)> {
    let mut spec = Spec::plain();

    // 1. field width: optional `-`, then digits or `*`.
    let negated = bytes.get(*i) == Some(&b'-');
    if negated {
        *i += 1;
    }
    let fv = field_value(bytes, i, args, next_arg)?;
    spec.width = match fv {
        None => {
            if negated {
                // `field_width` negates the value; there is none to negate.
                return Err(badarg());
            }
            None
        }
        Some(f) => {
            let f = if negated { -f } else { f };
            if f < 0 {
                spec.left = true;
                Some(f.unsigned_abs() as usize)
            } else {
                Some(f as usize)
            }
        }
    };

    // 2. precision: optional `.`, then digits or `*`.
    if bytes.get(*i) == Some(&b'.') {
        *i += 1;
        if let Some(p) = field_value(bytes, i, args, next_arg)? {
            if p < 0 {
                return Err(badarg());
            }
            spec.precision = Some(p as usize);
        }
    }

    // 3. pad character: optional `.`, then `*` (from an argument) or one byte.
    if bytes.get(*i) == Some(&b'.') {
        *i += 1;
        if bytes.get(*i) == Some(&b'*') {
            *i += 1;
            let arg = take(args, next_arg)?;
            if !is_small(arg) {
                return Err(badarg());
            }
            let value = signed_value(arg);
            if !(0..=255).contains(&value) {
                return Err(badarg());
            }
            spec.pad = value as u8;
        } else if let Some(&pad) = bytes.get(*i) {
            spec.pad = pad;
            *i += 1;
        } else {
            return Err(badarg());
        }
    }

    // 4. modifiers.
    loop {
        match bytes.get(*i) {
            Some(b't') => {
                spec.unicode = true;
                *i += 1;
            }
            Some(b'l') => {
                spec.strings = false;
                *i += 1;
            }
            // `~k`/`~K` choose a map printing order we cannot honour — reject.
            Some(b'k') | Some(b'K') => return Err(badarg()),
            _ => break,
        }
    }

    // 5. the control character.
    let ctrl = *bytes.get(*i).ok_or_else(badarg)?;
    *i += 1;
    Ok((spec, ctrl))
}

/// `io_lib_format:field_value/2`: a decimal run, or `*` (taking an integer
/// argument), or absent.
fn field_value(
    bytes: &[u8],
    i: &mut usize,
    args: &[Eterm],
    next_arg: &mut usize,
) -> Result<Option<i64>> {
    if bytes.get(*i) == Some(&b'*') {
        *i += 1;
        let arg = take(args, next_arg)?;
        if !is_small(arg) {
            return Err(badarg());
        }
        return Ok(Some(signed_value(arg)));
    }
    let start = *i;
    let mut value: i64 = 0;
    while let Some(&byte) = bytes.get(*i) {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(byte - b'0'));
        *i += 1;
    }
    if *i == start {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

/// Takes the next argument, or `badarg` when the list is exhausted.
fn take(args: &[Eterm], next: &mut usize) -> Result<Eterm> {
    let arg = args.get(*next).copied().ok_or_else(badarg)?;
    *next += 1;
    Ok(arg)
}

/// Dispatches one control character (`io_lib_format:control_small` /
/// `control_limited`).
fn dispatch(
    ctx: &mut BifContext<'_>,
    ctrl: u8,
    spec: &Spec,
    args: &[Eterm],
    next_arg: &mut usize,
) -> Result<String> {
    match ctrl {
        b'~' => render_char('~', spec),
        b'n' => render_newline(spec),
        b'i' => {
            let _ = take(args, next_arg)?;
            Ok(String::new())
        }
        b'w' => {
            let arg = take(args, next_arg)?;
            let text = format_term(ctx, arg, false)?;
            Ok(apply_term(&text, spec))
        }
        b'p' => {
            let arg = take(args, next_arg)?;
            render_p(ctx, arg, spec.strings, UNLIMITED_DEPTH, spec)
        }
        b'W' => {
            let arg = take(args, next_arg)?;
            let depth = take_depth(args, next_arg)?;
            let text = format_term_depth(ctx, arg, false, depth)?;
            Ok(apply_term(&text, spec))
        }
        b'P' => {
            let arg = take(args, next_arg)?;
            let depth = take_depth(args, next_arg)?;
            render_p(ctx, arg, spec.strings, depth, spec)
        }
        b's' => {
            let arg = take(args, next_arg)?;
            let text = string_arg(ctx, arg)?;
            render_string(&text, spec)
        }
        b'c' => {
            let arg = take(args, next_arg)?;
            if !is_small(arg) {
                return Err(badarg());
            }
            let value = signed_value(arg);
            let ch = if spec.unicode {
                let cp = u32::try_from(value).map_err(|_| badarg())?;
                char::from_u32(cp).ok_or_else(badarg)?
            } else {
                (value & 0xff) as u8 as char
            };
            render_char(ch, spec)
        }
        b'b' | b'B' => {
            let arg = take(args, next_arg)?;
            if !is_small(arg) {
                return Err(badarg());
            }
            let base = spec.precision.unwrap_or(10);
            render_radix(signed_value(arg), base, "", ctrl == b'B', spec)
        }
        b'x' | b'X' => {
            let arg = take(args, next_arg)?;
            if !is_small(arg) {
                return Err(badarg());
            }
            let prefix = prefix_arg(ctx, take(args, next_arg)?)?;
            let base = spec.precision.unwrap_or(10);
            render_radix(signed_value(arg), base, &prefix, ctrl == b'X', spec)
        }
        b'e' => {
            let f = take_float(args, next_arg)?;
            let precision = spec.precision.unwrap_or(6);
            if precision < 2 {
                return Err(badarg());
            }
            let text = float_e(f, precision);
            Ok(apply_float_field(&text, spec))
        }
        b'f' => {
            let f = take_float(args, next_arg)?;
            let precision = spec.precision.unwrap_or(6);
            if precision < 1 {
                return Err(badarg());
            }
            let text = float_f(f, precision);
            Ok(apply_float_field(&text, spec))
        }
        b'g' => {
            let f = take_float(args, next_arg)?;
            let precision = spec.precision.unwrap_or(6);
            let text = float_g(f, precision)?;
            Ok(apply_float_field(&text, spec))
        }
        _ => Err(badarg()),
    }
}

/// The `~W`/`~P` depth argument (a small integer; a negative depth is unlimited).
fn take_depth(args: &[Eterm], next_arg: &mut usize) -> Result<i64> {
    let arg = take(args, next_arg)?;
    if !is_small(arg) {
        return Err(badarg());
    }
    Ok(signed_value(arg))
}

/// The `~e`/`~f`/`~g` argument (a float; an integer is `badarg`, as in the oracle).
fn take_float(args: &[Eterm], next_arg: &mut usize) -> Result<f64> {
    let arg = take(args, next_arg)?;
    if !is_float(arg) {
        return Err(badarg());
    }
    Ok(float_value(arg))
}

/// `io_lib_format:prefixed_integer`'s prefix: an atom's name or a character list.
fn prefix_arg(ctx: &BifContext<'_>, arg: Eterm) -> Result<String> {
    if is_atom(arg) {
        return Ok(ctx.atoms().name(atom_val(arg)).ok_or_else(badarg)?.to_string());
    }
    let elems = proper_list(arg).ok_or_else(badarg)?;
    let mut out = String::with_capacity(elems.len());
    for elem in elems {
        if !is_small(elem) {
            return Err(badarg());
        }
        let value = signed_value(elem);
        if !(0..=255).contains(&value) {
            return Err(badarg());
        }
        out.push(value as u8 as char);
    }
    Ok(out)
}

/// Renders the `~s` argument: an atom's name, a proper list of byte-sized small
/// integers, or a binary's bytes.
fn string_arg(ctx: &BifContext<'_>, arg: Eterm) -> Result<String> {
    if is_atom(arg) {
        let name = ctx.atoms().name(atom_val(arg)).ok_or_else(badarg)?;
        return Ok(name.to_string());
    }
    if bits::is_bitstring(arg) {
        if !bits::is_binary(arg) {
            // A non-byte-aligned bitstring has no byte string rendering in M1.
            return Err(badarg());
        }
        return Ok(bits::bytes(arg)?.iter().map(|&b| b as char).collect());
    }
    let elems = proper_list(arg).ok_or_else(badarg)?;
    let mut out = String::with_capacity(elems.len());
    for elem in elems {
        if !is_small(elem) {
            return Err(badarg());
        }
        let value = signed_value(elem);
        if !(0..=255).contains(&value) {
            return Err(badarg());
        }
        out.push(value as u8 as char);
    }
    Ok(out)
}

/// `io_lib_format:print/…` for `~p`/`~P`: the printable single-line rendering,
/// guarded against the re-flow the oracle applies (see the module docs).
fn render_p(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
    spec: &Spec,
) -> Result<String> {
    if spec.left {
        // A left-adjusted `~p`/`~P` has no clause in `io_lib_format`.
        return Err(badarg());
    }
    let text = format_term_depth(ctx, term, printable, depth)?;
    let line_length = spec.width.unwrap_or(80);
    let column = spec.precision.unwrap_or(1);
    let length = text.chars().count();
    if reflowable(term) && length >= line_length.saturating_sub(column) {
        // The oracle would break this across lines; we cannot reproduce that.
        return Err(badarg());
    }
    Ok(text)
}

/// Whether the oracle's `io_lib_pretty` may break `term` across lines (a tuple,
/// map, non-printable list, or non-printable bitstring) rather than printing it
/// flat like an atom, number, ref, printable string or printable binary.
fn reflowable(term: Eterm) -> bool {
    if is_tuple(term) || is_map(term) {
        return true;
    }
    if bits::is_bitstring(term) {
        return !binary_printable(term);
    }
    if is_list(term) {
        return !list_printable(term);
    }
    false
}

/// Whether `term` is a non-empty proper list of small integers in `32..=126`
/// (the printable-string rule of [`crate::print`]).
fn list_printable(term: Eterm) -> bool {
    match proper_list(term) {
        Some(elems) if !elems.is_empty() => elems.iter().all(|&e| {
            is_small(e) && (32..=126).contains(&signed_value(e))
        }),
        _ => false,
    }
}

/// Whether `term` is a non-empty binary whose every byte is in `32..=126`.
fn binary_printable(term: Eterm) -> bool {
    match bits::bytes(term) {
        Ok(bytes) if !bytes.is_empty() => bytes.iter().all(|&b| (32..=126).contains(&b)),
        _ => false,
    }
}

/// `io_lib_format:term/6` — the field-width / truncation rule.
fn apply_term(text: &str, spec: &Spec) -> String {
    match spec.width {
        None => match spec.precision {
            None => text.to_string(),
            Some(p) => apply_term(text, &Spec { width: Some(p), ..*spec }),
        },
        Some(field) => {
            let length = text.chars().count();
            let limit = spec.precision.map_or(field, |p| p.min(field));
            let effective = length.min(limit);
            if length > effective {
                // Too long: `effective` stars, padded to the field width.
                adjust(&"*".repeat(effective), spec.pad, field - effective, spec.left)
            } else {
                adjust(text, spec.pad, field - length, spec.left)
            }
        }
    }
}

/// `fwrite_e`/`fwrite_f`/`fwrite_g` use the field width as both the width and the
/// precision (`term(_Chars, F, Adj, F, Pad, _)`).
fn apply_float_field(text: &str, spec: &Spec) -> String {
    match spec.width {
        None => text.to_string(),
        Some(field) => apply_term(text, &Spec { precision: Some(field), ..*spec }),
    }
}

/// `io_lib_format:string/6` for `~s`.
fn render_string(text: &str, spec: &Spec) -> Result<String> {
    let length = text.chars().count();
    match (spec.width, spec.precision) {
        (None, None) => Ok(text.to_string()),
        (Some(field), None) => Ok(string_field(text, field, spec.left, length, spec.pad)),
        (None, Some(p)) => Ok(string_field(text, p, true, length, spec.pad)),
        (Some(field), Some(p)) if field >= p => {
            if field > p {
                if length > p {
                    let head = truncate(text, p);
                    Ok(adjust(&head, spec.pad, field - p, spec.left))
                } else if length < p {
                    let tail = pad_of(spec.pad, p - length);
                    Ok(adjust(
                        &format!("{text}{tail}"),
                        spec.pad,
                        field - p,
                        spec.left,
                    ))
                } else {
                    Ok(adjust(text, spec.pad, field - p, spec.left))
                }
            } else {
                Ok(string_field(text, field, spec.left, length, spec.pad))
            }
        }
        _ => Err(badarg()),
    }
}

/// `io_lib_format:string_field/6`.
fn string_field(text: &str, field: usize, left: bool, length: usize, pad: u8) -> String {
    if length > field {
        truncate(text, field)
    } else if length < field {
        adjust(text, pad, field - length, left)
    } else {
        text.to_string()
    }
}

/// `io_lib_format:char/5` for `~c`/`~~`.
fn render_char(ch: char, spec: &Spec) -> Result<String> {
    match (spec.width, spec.precision) {
        (None, None) => Ok(ch.to_string()),
        (Some(field), None) => Ok(ch.to_string().repeat(field)),
        (None, Some(p)) => Ok(ch.to_string().repeat(p)),
        (Some(field), Some(p)) if field >= p => {
            let body = ch.to_string().repeat(p);
            Ok(adjust(&body, spec.pad, field - p, spec.left))
        }
        _ => Err(badarg()),
    }
}

/// `io_lib_format:newline/4` for `~n` (only the right-adjust clause exists).
fn render_newline(spec: &Spec) -> Result<String> {
    match spec.width {
        None => Ok("\n".to_string()),
        Some(field) if !spec.left => Ok("\n".repeat(field)),
        _ => Err(badarg()),
    }
}

/// `unprefixed_integer`/`prefixed_integer`: sign, prefix and `.<base>` digits.
fn render_radix(value: i64, base: usize, prefix: &str, upper: bool, spec: &Spec) -> Result<String> {
    if !(2..=36).contains(&base) {
        return Err(badarg());
    }
    let mut out = String::new();
    if value < 0 {
        out.push('-');
    }
    out.push_str(prefix);
    out.push_str(&int_to_base_string(value.unsigned_abs(), base as u32, upper));
    Ok(apply_term(&out, &Spec { precision: None, ..*spec }))
}

/// Renders `magnitude` in `base` (2–36), lowercase when `!upper`.
fn int_to_base_string(magnitude: u64, base: u32, upper: bool) -> String {
    const LOWER: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    const UPPER: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let digits = if upper { UPPER } else { LOWER };
    let base = u64::from(base);

    let mut reversed = String::new();
    let mut value = magnitude;
    while value > 0 {
        reversed.push(digits[(value % base) as usize] as char);
        value /= base;
    }
    if reversed.is_empty() {
        reversed.push('0');
    }
    reversed.chars().rev().collect()
}

/// `io_lib_format:adjust/3`.
fn adjust(data: &str, pad: u8, count: usize, left: bool) -> String {
    if count == 0 {
        return data.to_string();
    }
    let fill = pad_of(pad, count);
    if left {
        format!("{data}{fill}")
    } else {
        format!("{fill}{data}")
    }
}

/// `io_lib_format:chars/2` — `count` copies of `pad` as a string.
fn pad_of(pad: u8, count: usize) -> String {
    std::iter::repeat(pad as char).take(count).collect()
}

/// `io_lib_format:flat_trunc/3` — the first `n` characters.
fn truncate(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// `fwrite_e`/`float_e` — a float in scientific notation with `p` significands
/// after the leading digit (`p >= 2`).
fn float_e(f: f64, p: usize) -> String {
    let (digits, scale) = float_data(f);
    let (mantissa, exponent) = abs_float_e(&digits, scale, p);
    format!("{}{mantissa}{}", signbit(f), float_exp(exponent))
}

/// `abs_float_e/3`.
fn abs_float_e(digits: &[u8], scale: i64, p: usize) -> (String, i64) {
    let (mantissa, carry) = float_man3(digits, 1, p as i64 - 1);
    if carry {
        // The leading digit carried out of the number: `[$0|Fs]` becomes `1Fs`.
        let rest = mantissa.strip_prefix('0').unwrap_or(&mantissa);
        (format!("1{rest}"), scale)
    } else {
        (mantissa, scale - 1)
    }
}

/// `fwrite_f`/`float_f` — a float in fixed notation with `p` decimals (`p >= 1`).
fn float_f(f: f64, p: usize) -> String {
    let (digits, scale) = float_data(f);
    let (digits, scale) = if scale <= 0 {
        // Prepend `-E+1` zeros so the point lands after the first integer digit.
        let mut padded = vec![b'0'; (-scale + 1) as usize];
        padded.extend_from_slice(&digits);
        (padded, 1i64)
    } else {
        (digits, scale)
    };
    let (mantissa, carry) = float_man3(&digits, scale, p as i64);
    let body = if carry {
        format!("1{mantissa}")
    } else {
        mantissa
    };
    format!("{}{body}", signbit(f))
}

/// `fwrite_g/5` — `f` form for `[0.1, 1.0e4)` when it carries enough significant
/// digits, else the `e` form (precision = number of significant digits).
fn float_g(f: f64, p: usize) -> Result<String> {
    if p < 1 {
        return Err(badarg());
    }
    let magnitude = f.abs();
    if magnitude < 1.0e4 {
        let size: i64 = if magnitude < 1.0e-1 {
            -2
        } else if magnitude < 1.0 {
            -1
        } else if magnitude < 1.0e1 {
            0
        } else if magnitude < 1.0e2 {
            1
        } else if magnitude < 1.0e3 {
            2
        } else {
            3
        };
        if (p <= 1 && size == -1) || ((p as i64 - 1) > size && size >= -1) {
            // `f` form with enough decimals: `p - 1 - size` (always >= 1 here).
            return Ok(float_f(f, (p as i64 - 1 - size) as usize));
        }
    }
    if p <= 1 {
        Ok(float_e(f, 2))
    } else {
        Ok(float_e(f, p))
    }
}

/// `io_lib_format:float_data/1` — the decimal digits and exponent of
/// `float_to_list(F, [{scientific, 20}])` (21 significant digits, exact).
fn float_data(f: f64) -> (Vec<u8>, i64) {
    // Rust's `{:.20e}` produces the same correctly-rounded 21 significant digits
    // as Erlang's `float_to_list(F, [{scientific,20}])`; only the exponent
    // formatting differs, which we parse here.
    let formatted = format!("{:.20e}", f.abs());
    let mut digits = Vec::with_capacity(21);
    let mut exponent = 0i64;
    if let Some((mantissa, exp)) = formatted.split_once('e') {
        digits.extend(mantissa.bytes().filter(|b| b.is_ascii_digit()));
        exponent = exp.parse().unwrap_or(0);
    }
    (digits, exponent + 1)
}

/// `io_lib_format:signbit/1` — `-` for a negative float, including `-0.0`.
fn signbit(f: f64) -> &'static str {
    if f.is_sign_negative() {
        "-"
    } else {
        ""
    }
}

/// `io_lib_format:float_exp/1` — the exponent with an explicit sign.
fn float_exp(exponent: i64) -> String {
    if exponent >= 0 {
        format!("e+{exponent}")
    } else {
        format!("e{exponent}")
    }
}

/// `float_man/3`: `i` digits before the point, `decimals` after, with carry.
fn float_man3(digits: &[u8], i: i64, decimals: i64) -> (String, bool) {
    if i == 0 {
        let (fraction, carry) = float_man2(digits, decimals);
        return (format!(".{fraction}"), carry);
    }
    match digits.split_first() {
        Some((&digit, rest)) => {
            let (tail, carry) = float_man3(rest, i - 1, decimals);
            round_digit(digit, tail, carry)
        }
        None => (
            format!("{}.{}", "0".repeat(i as usize), "0".repeat(decimals as usize)),
            false,
        ),
    }
}

/// `float_man/2`: `decimals` decimals (no integer part), with carry.
fn float_man2(digits: &[u8], decimals: i64) -> (String, bool) {
    if digits.is_empty() {
        return ("0".repeat(decimals.max(0) as usize), false);
    }
    if decimals == 0 {
        return (String::new(), digits[0] >= b'5');
    }
    let (tail, carry) = float_man2(&digits[1..], decimals - 1);
    round_digit(digits[0], tail, carry)
}

/// Shared rounding step of `float_man/2` and `float_man/3`.
fn round_digit(digit: u8, tail: String, carry: bool) -> (String, bool) {
    if carry {
        if digit == b'9' {
            (format!("0{tail}"), true)
        } else {
            (format!("{}{tail}", (digit + 1) as char), false)
        }
    } else {
        (format!("{}{tail}", digit as char), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    use crate::context::BytesSink;

    const SEED: [&str; 12] = [
        "true", "false", "ok", "hello", "a", "b", "abc", "hi", "foo", "abcdefgh", "", "e",
    ];

    /// Runs `io:format` over a fresh context, returning the captured output and the
    /// BIF result. `build_args` returns the argument list (or `None` for arity 1).
    fn run_io(
        format: &str,
        build_args: impl FnOnce(&mut BifContext<'_>) -> Option<Eterm>,
    ) -> (String, Result<Eterm>) {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut sink = BytesSink::new();
        let result = {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            let format_term = list_from_bytes(&mut ctx, format.as_bytes());
            match build_args(&mut ctx) {
                Some(args) => format_2(&mut ctx, &[format_term, args]),
                None => format_1(&mut ctx, &[format_term]),
            }
        };
        (sink.as_str(), result)
    }

    /// Runs `io_lib:format/2` and flattens the returned iolist back to a `String`.
    fn run_io_lib(
        format: &str,
        build_args: impl FnOnce(&mut BifContext<'_>) -> Eterm,
    ) -> (String, Result<Eterm>) {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut sink = BytesSink::new();
        let (result, rendered) = {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            let format_term = list_from_bytes(&mut ctx, format.as_bytes());
            let args = build_args(&mut ctx);
            let result = format_lib_2(&mut ctx, &[format_term, args]);
            let rendered = match result {
                Ok(term) => match proper_list(term) {
                    Some(elems) => elems
                        .iter()
                        .map(|&e| {
                            assert!(is_small(e), "io_lib:format returns byte-sized ints");
                            signed_value(e) as u8 as char
                        })
                        .collect(),
                    None => panic!("io_lib:format must return a proper list"),
                },
                Err(BeamError::BadArg) => String::new(),
                Err(other) => panic!("unexpected error {other:?}"),
            };
            (result.map(|_| ()).map_err(|e| e), rendered)
        };
        (rendered, result.map(|()| make_small(0)).map_err(|e| e))
    }

    /// Builds a format list term from raw ASCII bytes on `ctx`'s heap.
    fn list_from_bytes(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        let elems: Vec<Eterm> = bytes.iter().map(|&b| make_small(i64::from(b))).collect();
        ctx.list(&elems).expect("the format list fits")
    }

    /// A one-element argument list builder.
    fn one(ctx: &mut BifContext<'_>, term: Eterm) -> Option<Eterm> {
        Some(ctx.list(&[term]).expect("fits"))
    }

    fn ints(ctx: &mut BifContext<'_>, values: &[i64]) -> Option<Eterm> {
        let elems: Vec<Eterm> = values.iter().map(|&v| ctx.int(v).expect("fits")).collect();
        Some(ctx.list(&elems).expect("fits"))
    }

    fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        let elems: Vec<Eterm> = bytes.iter().map(|&b| ctx.int(i64::from(b)).expect("fits")).collect();
        ctx.list(&elems).expect("fits")
    }

    // ---- the original, still-required behaviours -------------------------------

    #[test]
    fn format_p_and_newline() {
        let (output, result) = run_io("~p~n", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one]).expect("fits");
            one_element(ctx, tuple)
        });
        assert_eq!(output, "{a,1}\n");
        assert!(result.is_ok());
    }

    fn one_element(ctx: &mut BifContext<'_>, term: Eterm) -> Option<Eterm> {
        one(ctx, term)
    }

    #[test]
    fn format_w_s_b_writes_exact_bytes() {
        let (output, result) = run_io("~w ~s ~b~n", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            let hi = byte_list(ctx, b"hi");
            let byte = ctx.int(255).expect("fits");
            Some(ctx.list(&[abc, hi, byte]).expect("fits"))
        });
        assert_eq!(output, "abc hi 255\n");
        assert!(result.is_ok());
    }

    #[test]
    fn tilde_d_is_badarg() {
        let (_, result) = run_io("~d", |ctx| ints(ctx, &[255]));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn integer_directive_rejects_non_integers() {
        let (_, result) = run_io("~b", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            one(ctx, a)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn newline_and_literal_tilde() {
        let (output, result) = run_io("a~~b~n", |_ctx| Some(NIL));
        assert_eq!(output, "a~b\n");
        assert!(result.is_ok());
    }

    #[test]
    fn ignore_directive_consumes_without_output() {
        let (output, result) = run_io("~i~w", |ctx| ints(ctx, &[5, 7]));
        assert_eq!(output, "7");
        assert!(result.is_ok());
    }

    #[test]
    fn format_1_uses_empty_args() {
        let (output, result) = run_io("hello~n", |_ctx| None);
        assert_eq!(output, "hello\n");
        assert!(result.is_ok());
    }

    #[test]
    fn bad_format_term_is_badarg() {
        let mut heap = Heap::new(64).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut sink = BytesSink::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let atom = ctx.atom("abc").expect("seeded");
        assert!(matches!(format_1(&mut ctx, &[atom]), Err(BeamError::BadArg)));
        let bad = ctx
            .list(&[ctx.int(1000).expect("fits")])
            .expect("the list fits");
        assert!(matches!(format_1(&mut ctx, &[bad]), Err(BeamError::BadArg)));
    }

    #[test]
    fn int_to_base_string_edge_cases() {
        assert_eq!(int_to_base_string(0, 10, false), "0");
        assert_eq!(int_to_base_string(0, 16, true), "0");
        assert_eq!(int_to_base_string(255, 10, false), "255");
        assert_eq!(int_to_base_string(255, 16, true), "FF");
        assert_eq!(int_to_base_string(35, 36, true), "Z");
    }

    // ---- field widths, precision, adjust (`term/6`) ----------------------------

    #[test]
    fn field_width_pads_right_and_left() {
        let (output, _) = run_io("~10w", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            one(ctx, abc)
        });
        assert_eq!(output, "       abc");

        let (output, _) = run_io("~-10w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one_]).expect("fits");
            one(ctx, tuple)
        });
        assert_eq!(output, "{a,1}     ");
    }

    #[test]
    fn overlong_rendering_becomes_stars() {
        // `io_lib:format("~4w",[{a,1}])` => "****".
        let (output, _) = run_io("~4w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one_]).expect("fits");
            one(ctx, tuple)
        });
        assert_eq!(output, "****");

        // `io_lib:format("~5.3w",["abcdefgh"])` => "  ***".
        let (output, _) = run_io("~5.3w", |ctx| {
            let s = byte_list(ctx, b"abcdefgh");
            one(ctx, s)
        });
        assert_eq!(output, "  ***");

        // `io_lib:format("~.0w",["abc"])` => "".
        let (output, _) = run_io("~.0w", |ctx| {
            let s = byte_list(ctx, b"abc");
            one(ctx, s)
        });
        assert_eq!(output, "");
    }

    #[test]
    fn newline_counts_and_adjust() {
        let (output, _) = run_io("~3n", |_ctx| Some(NIL));
        assert_eq!(output, "\n\n\n");
        let (output, _) = run_io("~0n", |_ctx| Some(NIL));
        assert_eq!(output, "");
        let (_, result) = run_io("~-3n", |_ctx| Some(NIL));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn bad_base_and_precision_are_badarg() {
        for format in ["~.0b", "~.1b", "~.37b", "~.b"] {
            let (_, result) = run_io(format, |ctx| ints(ctx, &[5]));
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "format {format} should be badarg"
            );
        }
        for format in ["~.1e", "~.0f", "~.0g"] {
            let (_, result) = run_io(format, |ctx| {
                let f = ctx.make_float(1.5).expect("fits");
                one(ctx, f)
            });
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "format {format} should be badarg"
            );
        }
    }

    // ---- radix directives ------------------------------------------------------

    #[test]
    fn base_directives_and_hex_prefix() {
        let (output, _) = run_io("~.16B", |ctx| ints(ctx, &[255]));
        assert_eq!(output, "FF");
        let (output, _) = run_io("~.16b", |ctx| ints(ctx, &[255]));
        assert_eq!(output, "ff");
        let (output, _) = run_io("~.2B", |ctx| ints(ctx, &[5]));
        assert_eq!(output, "101");
        let (output, _) = run_io("~.16B", |ctx| ints(ctx, &[-255]));
        assert_eq!(output, "-FF");
        let (output, _) = run_io("~8.16b", |ctx| ints(ctx, &[255]));
        assert_eq!(output, "      ff");

        // `~x`/`~X` take `(Integer, Prefix)`; the prefix is a char list or atom.
        let (output, _) = run_io("~x", |ctx| {
            let n = ctx.int(255).expect("fits");
            let prefix = byte_list(ctx, b"#");
            Some(ctx.list(&[n, prefix]).expect("fits"))
        });
        assert_eq!(output, "#255");
        let (output, _) = run_io("~.16x", |ctx| {
            let n = ctx.int(255).expect("fits");
            let prefix = byte_list(ctx, b"#");
            Some(ctx.list(&[n, prefix]).expect("fits"))
        });
        assert_eq!(output, "#ff");
        let (output, _) = run_io("~.16X", |ctx| {
            let n = ctx.int(255).expect("fits");
            let prefix = byte_list(ctx, b"#");
            Some(ctx.list(&[n, prefix]).expect("fits"))
        });
        assert_eq!(output, "#FF");
        let (output, _) = run_io("~x", |ctx| {
            let n = ctx.int(-255).expect("fits");
            let prefix = byte_list(ctx, b"0x");
            Some(ctx.list(&[n, prefix]).expect("fits"))
        });
        assert_eq!(output, "0x-255");

        // A single-argument `~x` is badarg (it needs two arguments) — this is the
        // shape the crate's integration test asserts.
        let (_, result) = run_io("~x", |ctx| ints(ctx, &[255]));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    // ---- floats ----------------------------------------------------------------

    #[test]
    fn float_directives_match_the_oracle() {
        for (format, value, expected) in [
            ("~e", 1.5, "1.50000e+0"),
            ("~f", 1.5, "1.500000"),
            ("~g", 1.5, "1.50000"),
            ("~e", 1234.5678, "1.23457e+3"),
            ("~f", 1234.5678, "1234.567800"),
            ("~g", 1234.5678, "1234.57"),
            ("~e", 0.0, "0.00000e+0"),
            ("~f", 0.0, "0.000000"),
            ("~g", 0.0, "0.00000e+0"),
            ("~g", 0.1, "0.100000"),
            ("~g", 100000.0, "1.00000e+5"),
            ("~g", 99.0, "99.0000"),
            ("~g", 9999.0, "9999.00"),
            ("~g", 10000.0, "1.00000e+4"),
            ("~g", -1.5, "-1.50000"),
            ("~f", 1.0e10, "10000000000.000000"),
            ("~.1f", 1.5, "1.5"),
            ("~.2f", 0.1, "0.10"),
            ("~.2f", 1.567, "1.57"),
            ("~.2e", 1.567, "1.6e+0"),
            ("~.3g", 1.567, "1.57"),
            ("~12.4f", 1.567, "      1.5670"),
            ("~-12.4f", 1.567, "1.5670      "),
            ("~e", -0.0, "-0.00000e+0"),
        ] {
            let (output, result) = run_io(format, |ctx| {
                let f = ctx.make_float(value).expect("fits");
                one(ctx, f)
            });
            assert!(result.is_ok(), "{format} of {value} errored");
            assert_eq!(output, expected, "{format} of {value}");
        }
    }

    #[test]
    fn float_directives_reject_integers() {
        for format in ["~e", "~f", "~g"] {
            let (_, result) = run_io(format, |ctx| ints(ctx, &[1]));
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "{format} of an integer should be badarg"
            );
        }
    }

    // ---- strings and characters ------------------------------------------------

    #[test]
    fn string_directive_accepts_atoms_lists_and_binaries() {
        let (output, _) = run_io("~s", |ctx| {
            let hello = ctx.atom("hello").expect("seeded");
            one(ctx, hello)
        });
        assert_eq!(output, "hello");

        let (output, _) = run_io("~s", |ctx| {
            let hi = byte_list(ctx, b"hi");
            one(ctx, hi)
        });
        assert_eq!(output, "hi");

        let (output, _) = run_io("~s", |ctx| {
            let bin = bits::make_binary(ctx.heap(), b"hi").expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "hi");

        // Width/truncation: `~2s` of "hello" => "he"; `~10.3s` => "       hel".
        let (output, _) = run_io("~2s", |ctx| {
            let hi = byte_list(ctx, b"hello");
            one(ctx, hi)
        });
        assert_eq!(output, "he");
        let (output, _) = run_io("~10.3s", |ctx| {
            let hi = byte_list(ctx, b"hello");
            one(ctx, hi)
        });
        assert_eq!(output, "       hel");
        let (output, _) = run_io("~-10.3s", |ctx| {
            let hi = byte_list(ctx, b"hello");
            one(ctx, hi)
        });
        assert_eq!(output, "hel       ");

        // A list element outside 0..=255 is badarg.
        let (_, result) = run_io("~s", |ctx| ints(ctx, &[8364]));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn character_directive() {
        let (output, _) = run_io("~c", |ctx| ints(ctx, &[65]));
        assert_eq!(output, "A");
        let (output, _) = run_io("~3c", |ctx| ints(ctx, &[65]));
        assert_eq!(output, "AAA");
        let (output, _) = run_io("~tc", |ctx| ints(ctx, &[65]));
        assert_eq!(output, "A");
        let (_, result) = run_io("~c", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            one(ctx, a)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    // ---- width from the argument, depth ---------------------------------------

    #[test]
    fn width_and_precision_from_arguments() {
        let (output, _) = run_io("~*w", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            Some(ctx.list(&[ctx.int(6).expect("fits"), abc]).expect("fits"))
        });
        assert_eq!(output, "   abc");

        let (output, _) = run_io("~*w", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            Some(ctx.list(&[ctx.int(-6).expect("fits"), abc]).expect("fits"))
        });
        assert_eq!(output, "abc   ");

        let (output, _) = run_io("~.*b", |ctx| {
            let n = ctx.int(255).expect("fits");
            Some(ctx.list(&[ctx.int(16).expect("fits"), n]).expect("fits"))
        });
        assert_eq!(output, "ff");

        let (output, _) = run_io("~*w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one_]).expect("fits");
            Some(ctx.list(&[ctx.int(4).expect("fits"), tuple]).expect("fits"))
        });
        assert_eq!(output, "****");
    }

    #[test]
    fn depth_directives() {
        let term = |ctx: &mut BifContext<'_>| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let inner = ctx.tuple(&[c, one_]).expect("fits");
            let mid = ctx.tuple(&[b, inner]).expect("fits");
            ctx.tuple(&[a, mid]).expect("fits")
        };
        let (output, _) = run_io("~W", |ctx| {
            let t = term(ctx);
            Some(ctx.list(&[t, ctx.int(3).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "{a,{...}}");
        let (output, _) = run_io("~W", |ctx| {
            let t = term(ctx);
            Some(ctx.list(&[t, ctx.int(2).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "{a,...}");
        let (output, _) = run_io("~10W", |ctx| {
            let t = term(ctx);
            Some(ctx.list(&[t, ctx.int(3).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, " {a,{...}}");

        // `~P` of a list at depth 3.
        let (output, _) = run_io("~P", |ctx| {
            let list = ctx
                .list(&[
                    ctx.int(1).expect("fits"),
                    ctx.int(2).expect("fits"),
                    ctx.int(3).expect("fits"),
                    ctx.int(4).expect("fits"),
                    ctx.int(5).expect("fits"),
                ])
                .expect("fits");
            Some(ctx.list(&[list, ctx.int(3).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "[1,2|...]");
    }

    // ---- the extended printer --------------------------------------------------

    #[test]
    fn write_and_print_of_binaries_maps_and_bignums() {
        let (output, _) = run_io("~w", |ctx| {
            let bin = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "<<97,98,99>>");

        let (output, _) = run_io("~p", |ctx| {
            let bin = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "<<\"abc\">>");

        let (output, _) = run_io("~p", |ctx| {
            let bin = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "<<1,2,3>>");

        let (output, _) = run_io("~w", |ctx| {
            let bin = bits::make_binary(ctx.heap(), b"").expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "<<>>");

        // A flat map renders `#{k => v,…}` in flatmap key order (a < b < c).
        let (output, _) = run_io("~w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let map = crate::access::make_flatmap(ctx.heap(), &[(b, one_), (a, two)]).expect("fits");
            one(ctx, map)
        });
        assert_eq!(output, "#{a => 2,b => 1}");

        // A bignum renders in decimal.
        let (output, _) = run_io("~w", |ctx| {
            let big = beam_codec::term::make_big(ctx.heap(), false, &[0, 1]).expect("fits");
            one(ctx, big)
        });
        assert_eq!(output, "18446744073709551616");
    }

    #[test]
    fn print_of_a_long_container_is_badarg_instead_of_misformatted() {
        // The oracle would re-flow a 40-element list under `~p`; we reject rather
        // than emit a single line the oracle would have broken.
        let (_, result) = run_io("~p", |ctx| {
            let elems: Vec<Eterm> = (0..40).map(|i| ctx.int(i).expect("fits")).collect();
            let list = ctx.list(&elems).expect("fits");
            one(ctx, list)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));

        // A short list is fine.
        let (output, result) = run_io("~p", |ctx| {
            let elems: Vec<Eterm> = (1..=5).map(|i| ctx.int(i).expect("fits")).collect();
            let list = ctx.list(&elems).expect("fits");
            one(ctx, list)
        });
        assert!(result.is_ok());
        assert_eq!(output, "[1,2,3,4,5]");

        // A left-adjusted `~p` has no clause in the oracle.
        let (_, result) = run_io("~-10p", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            one(ctx, a)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    // ---- io_lib:format ---------------------------------------------------------

    #[test]
    fn io_lib_format_returns_an_iolist() {
        let (rendered, result) = run_io_lib("~b~n", |ctx| {
            ctx.list(&[ctx.int(255).expect("fits")]).expect("fits")
        });
        assert!(result.is_ok());
        assert_eq!(rendered, "255\n");

        // The empty rendering is `NIL`.
        let mut heap = Heap::new(128).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let format_term = list_from_bytes(&mut ctx, b"");
        let result = format_lib_1(&mut ctx, &[format_term]).expect("fits");
        assert_eq!(result.raw(), NIL.raw());

        // `io_lib:format` writes nothing to the output sink.
        let mut sink = BytesSink::new();
        {
            let mut heap = Heap::new(128).expect("test heap");
            let mut atoms = AtomTable::new();
            atoms.seed(SEED);
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_output(&mut sink);
            let format_term = list_from_bytes(&mut ctx, b"hi~n");
            assert!(format_lib_1(&mut ctx, &[format_term]).is_ok());
        }
        assert_eq!(sink.contents(), b"");
    }

    #[test]
    fn io_lib_format_1_and_2_agree_with_io_format() {
        for format in ["~w", "~p", "~s", "~c", "~b", "~x", "~e", "~f", "~g", "~W", "~P"] {
            let (io_out, io_res) = run_io(format, |ctx| {
                let a = ctx.atom("a").expect("seeded");
                let one_ = ctx.int(1).expect("fits");
                let tuple = ctx.tuple(&[a, one_]).expect("fits");
                let prefix = byte_list(ctx, b"#");
                match format {
                    "~x" => Some(ctx.list(&[ctx.int(255).expect("fits"), prefix]).expect("fits")),
                    "~e" | "~f" | "~g" => {
                        let f = ctx.make_float(1.5).expect("fits");
                        Some(ctx.list(&[f]).expect("fits"))
                    }
                    "~W" | "~P" => {
                        Some(ctx.list(&[tuple, ctx.int(2).expect("fits")]).expect("fits"))
                    }
                    "~s" => Some(ctx.list(&[ctx.atom("abc").expect("seeded")]).expect("fits")),
                    _ => one(ctx, tuple),
                }
            });
            let (lib_out, lib_res) = run_io_lib(format, |ctx| {
                let a = ctx.atom("a").expect("seeded");
                let one_ = ctx.int(1).expect("fits");
                let tuple = ctx.tuple(&[a, one_]).expect("fits");
                let prefix = byte_list(ctx, b"#");
                match format {
                    "~x" => ctx.list(&[ctx.int(255).expect("fits"), prefix]).expect("fits"),
                    "~e" | "~f" | "~g" => {
                        let f = ctx.make_float(1.5).expect("fits");
                        ctx.list(&[f]).expect("fits")
                    }
                    "~W" | "~P" => {
                        ctx.list(&[tuple, ctx.int(2).expect("fits")]).expect("fits")
                    }
                    "~s" => ctx.list(&[ctx.atom("abc").expect("seeded")]).expect("fits"),
                    _ => ctx.list(&[tuple]).expect("fits"),
                }
            });
            assert_eq!(io_res.is_ok(), lib_res.is_ok(), "{format}");
            assert_eq!(io_out, lib_out, "{format}");
        }
    }

    // ---- error paths -----------------------------------------------------------

    #[test]
    fn argument_count_is_exact() {
        let (_, result) = run_io("~w ~w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            one(ctx, a)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));

        let (_, result) = run_io("~w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            Some(ctx.list(&[a, b]).expect("fits"))
        });
        assert!(matches!(result, Err(BeamError::BadArg)));

        let (_, result) = run_io("abc~", |_ctx| Some(NIL));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn unsupported_directives_are_badarg() {
        // `~5~~` is badarg in the oracle: after `~5~` the trailing `~` starts an
        // empty control sequence.
        for (format, arg) in [(0u8, 255i64), (1, 255), (2, 255)] {
            let _ = (format, arg);
        }
        for format in ["~5~~", "~-p", "~k w", "~K w", "~+w", "~#w", "~z"] {
            let (_, result) = run_io(format, |ctx| ints(ctx, &[255]));
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "format {format} should be badarg"
            );
        }
    }

    #[test]
    fn field_value_and_adjust_helpers() {
        assert_eq!(adjust("x", b'*', 3, false), "***x");
        assert_eq!(adjust("x", b'*', 3, true), "x***");
        assert_eq!(adjust("x", b'*', 0, true), "x");
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(pad_of(b'0', 3), "000");
    }
}
