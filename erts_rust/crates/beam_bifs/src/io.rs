//! `io:format/1,2` and the format-directive engine.
//!
//! Implements the Milestone-1 `io` subset (compatibility contract Part I §A.7):
//! `Format` is a flat proper list of byte-sized small integers, `Args` a proper
//! list, and the call writes the rendered text to the context's [`OutputSink`]
//! ([`crate::context::BifContext::write_output`]) and returns the atom `ok`.
//!
//! # The supported directive subset
//!
//! `~w` (write), `~p` (print — the printable-list rule, see [`crate::print`]),
//! `~s` (string: a proper list of small ints or an atom's name), `~c` (a character
//! code point), `~n` (newline), `~~` (a literal `~`), `~i` (consume and ignore one
//! argument), and `~b`/`~B` (an integer in base 10, or `~.<base>b`/`~.<base>B` for
//! base 2–36). Every directive accepts an optional decimal **field width**
//! (`~10w`), which right-justifies with spaces. `~n` with a width repeats the
//! newline (matching the oracle), and `~i` ignores its width.
//!
//! # What is rejected (`badarg`)
//!
//! `~d` — the OTP oracle raises `badarg` for `~d` (`io_lib:format("~d",[255])` ⇒
//! `error:badarg`); `~b` is the base-10 integer directive. Also rejected:
//! `~e ~f ~g ~W ~P ~x ~X ~+ ~#`, `~*`, any negative/other adjustment, a base
//! outside `2..=36`, a precision on a directive that takes no base, a wrong
//! argument type for a directive, running out of arguments, leaving arguments
//! unconsumed, and a `~` at the end of the format. A directive the printer cannot
//! render (`~w`/`~p` of a non-M1 term) is likewise `badarg`.
//!
//! # Deviations from the oracle (documented)
//!
//! * The output is a Rust `String` (UTF-8): raw bytes `>= 128` in the format and
//!   in `~s`/`~c` are emitted as their UTF-8 encoding, not raw latin1 bytes. The
//!   ASCII subset is byte-exact.
//! * A precision on a directive other than `~b`/`~B` (e.g. `~.10w`) is `badarg`;
//!   the supported surface is fixed to `~.<base>b`/`~.<base>B`.

use beam_common::Result;
use beam_term::{atom_val, signed_value, Eterm};

use crate::access::{is_atom, is_small};
use crate::context::BifContext;
use crate::error::badarg;
use crate::print::{format_term, proper_list};
use crate::registry::BifRegistry;

/// Registers `io:format/1` and `io:format/2`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("io", "format", 1, format_1);
    reg.register("io", "format", 2, format_2);
}

/// `io:format(Format)` — writes `Format` with no arguments and returns `ok`.
fn format_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    run(ctx, args[0], None)
}

/// `io:format(Format, Args)` — writes `Format` with `Args` and returns `ok`.
fn format_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    run(ctx, args[0], Some(args[1]))
}

/// The shared body: decode the format list, render the directives against the
/// arguments, write the result and return `ok`.
fn run(ctx: &mut BifContext<'_>, format: Eterm, args: Option<Eterm>) -> Result<Eterm> {
    let bytes = format_bytes(format)?;
    let arg_terms = match args {
        Some(term) => proper_list(term).ok_or_else(badarg)?,
        None => Vec::new(),
    };
    let rendered = render_format(ctx, &bytes, &arg_terms)?;
    ctx.write_output(&rendered)?;
    ctx.ok()
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

        // Optional decimal field width.
        let width_start = i;
        let mut width_value = 0usize;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            width_value = width_value
                .saturating_mul(10)
                .saturating_add((bytes[i] - b'0') as usize);
            i += 1;
        }
        let width = (i > width_start).then_some(width_value);

        // Optional `.<base>` precision (only meaningful for `b`/`B`).
        let mut base = None;
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            let base_start = i;
            let mut base_value = 0u32;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                base_value = base_value
                    .saturating_mul(10)
                    .saturating_add((bytes[i] - b'0') as u32);
                i += 1;
            }
            if i == base_start {
                return Err(badarg());
            }
            base = Some(base_value);
        }

        if i >= bytes.len() {
            return Err(badarg());
        }
        let directive = bytes[i];
        i += 1;

        match directive {
            b'~' => {
                if width.is_some() || base.is_some() {
                    return Err(badarg());
                }
                out.push('~');
            }
            b'n' => {
                if base.is_some() {
                    return Err(badarg());
                }
                for _ in 0..width.unwrap_or(1) {
                    out.push('\n');
                }
            }
            b'i' => {
                if base.is_some() {
                    return Err(badarg());
                }
                // Consume and ignore one argument (its width, if any, is ignored).
                let _ = take(args, &mut next_arg)?;
            }
            b'w' | b'p' => {
                if base.is_some() {
                    return Err(badarg());
                }
                let arg = take(args, &mut next_arg)?;
                let piece = format_term(ctx, arg, directive == b'p')?;
                out.push_str(&apply_width(piece, width));
            }
            b's' => {
                if base.is_some() {
                    return Err(badarg());
                }
                let arg = take(args, &mut next_arg)?;
                let piece = string_arg(ctx, arg)?;
                out.push_str(&apply_width(piece, width));
            }
            b'c' => {
                if base.is_some() {
                    return Err(badarg());
                }
                let arg = take(args, &mut next_arg)?;
                let piece = char_arg(arg)?;
                out.push_str(&apply_width(piece, width));
            }
            b'b' | b'B' => {
                let arg = take(args, &mut next_arg)?;
                if !is_small(arg) {
                    return Err(badarg());
                }
                let radix = base.unwrap_or(10);
                if !(2..=36).contains(&radix) {
                    return Err(badarg());
                }
                let piece = int_to_base_string(signed_value(arg), radix, directive == b'B');
                out.push_str(&apply_width(piece, width));
            }
            _ => return Err(badarg()),
        }
    }

    if next_arg != args.len() {
        return Err(badarg());
    }
    Ok(out)
}

/// Takes the next argument, or `badarg` when the list is exhausted.
fn take(args: &[Eterm], next: &mut usize) -> Result<Eterm> {
    let arg = args.get(*next).copied().ok_or_else(badarg)?;
    *next += 1;
    Ok(arg)
}

/// Renders the `~s` argument: an atom's name, or a proper list of byte-sized
/// small integers.
fn string_arg(ctx: &BifContext<'_>, arg: Eterm) -> Result<String> {
    if is_atom(arg) {
        let name = ctx.atoms().name(atom_val(arg)).ok_or_else(badarg)?;
        return Ok(name.to_string());
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

/// Renders the `~c` argument (a character code point).
fn char_arg(arg: Eterm) -> Result<String> {
    if !is_small(arg) {
        return Err(badarg());
    }
    let value = signed_value(arg);
    if value < 0 {
        return Err(badarg());
    }
    let ch = char::from_u32(value as u32).ok_or_else(badarg)?;
    Ok(ch.to_string())
}

/// Renders `value` in `base` (2–36), lowercase when `!upper`.
fn int_to_base_string(value: i64, base: u32, upper: bool) -> String {
    const LOWER: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    const UPPER: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let digits = if upper { UPPER } else { LOWER };

    let negative = value < 0;
    // `i128` avoids overflow at `i64::MIN`.
    let mut magnitude = (value as i128).unsigned_abs();
    let base = base as u128;

    let mut reversed = String::new();
    while magnitude > 0 {
        reversed.push(digits[(magnitude % base) as usize] as char);
        magnitude /= base;
    }
    if reversed.is_empty() {
        reversed.push('0');
    }
    if negative {
        reversed.push('-');
    }
    reversed.chars().rev().collect()
}

/// Right-justifies `piece` to `width` characters with spaces (a no-op when it
/// already reaches the width, or when no width was given).
fn apply_width(piece: String, width: Option<usize>) -> String {
    match width {
        Some(width) => {
            let len = piece.chars().count();
            if len < width {
                let mut padded = " ".repeat(width - len);
                padded.push_str(&piece);
                padded
            } else {
                piece
            }
        }
        None => piece,
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

    /// Runs `io:format` over a fresh context, returning the captured output, the
    /// atom table (so a returned term's name can be read back) and the BIF result.
    /// `build_args` returns the argument list (or `None` for `io:format/1`).
    fn run_format(
        format: &str,
        build_args: impl FnOnce(&mut BifContext<'_>) -> Option<Eterm>,
    ) -> (String, AtomTable, Result<Eterm>) {
        let mut heap = Heap::new(2048).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "hello", "a", "b", "abc", "hi"]);
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
        (sink.as_str(), atoms, result)
    }

    /// Builds a format list term from raw bytes on `ctx`'s heap.
    fn list_from_bytes(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        let elems: Vec<Eterm> = bytes.iter().map(|&b| make_small(b as i64)).collect();
        ctx.list(&elems).expect("the format list fits")
    }

    fn format_p_and_newline_body() -> (String, AtomTable, Result<Eterm>) {
        run_format("~p~n", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one]).expect("fits");
            Some(ctx.list(&[tuple]).expect("fits"))
        })
    }

    #[test]
    fn format_p_and_newline() {
        let (output, atoms, result) = format_p_and_newline_body();
        assert_eq!(output, "{a,1}\n");
        let returned = result.expect("format returns a term");
        assert!(beam_term::is_atom(returned));
        assert_eq!(atoms.name(atom_val(returned)), Some("ok"));
    }

    #[test]
    fn format_w_s_b() {
        let (output, _, result) = run_format("~w ~s ~b~n", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            let hi = ctx.list(&[ctx.int(104).expect("fits"), ctx.int(105).expect("fits")])
                .expect("fits");
            let byte = ctx.int(255).expect("fits");
            Some(ctx.list(&[abc, hi, byte]).expect("fits"))
        });
        assert_eq!(output, "abc hi 255\n");
        assert!(result.is_ok());
    }

    #[test]
    fn format_w_of_a_string_list_is_not_a_string() {
        let (output, _, _) = run_format("~w", |ctx| {
            let abc = ctx
                .list(&[
                    ctx.int(97).expect("fits"),
                    ctx.int(98).expect("fits"),
                    ctx.int(99).expect("fits"),
                ])
                .expect("fits");
            Some(ctx.list(&[abc]).expect("fits"))
        });
        assert_eq!(output, "[97,98,99]");
    }

    #[test]
    fn format_p_of_a_string_is_a_string() {
        let (output, _, _) = run_format("~p", |ctx| {
            let abc = ctx
                .list(&[
                    ctx.int(97).expect("fits"),
                    ctx.int(98).expect("fits"),
                    ctx.int(99).expect("fits"),
                ])
                .expect("fits");
            Some(ctx.list(&[abc]).expect("fits"))
        });
        assert_eq!(output, "\"abc\"");
    }

    #[test]
    fn tilde_d_is_badarg() {
        let (_, _, result) = run_format("~d", |ctx| Some(ctx.list(&[ctx.int(255).expect("fits")]).expect("fits")));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn unsupported_directive_is_badarg() {
        for directive in ["~x", "~W", "~f", "~e", "~g", "~P", "~*w", "~+w", "~#w"] {
            let (_, _, result) =
                run_format(directive, |ctx| Some(ctx.list(&[ctx.int(255).expect("fits")]).expect("fits")));
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "directive {directive} should be badarg"
            );
        }
    }

    #[test]
    fn field_width_right_justifies() {
        let (output, _, _) = run_format("~10w", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            Some(ctx.list(&[abc]).expect("fits"))
        });
        assert_eq!(output, "       abc");
    }

    #[test]
    fn field_width_applies_to_integers_and_strings() {
        let (output, _, _) = run_format("~10b", |ctx| {
            Some(ctx.list(&[ctx.int(42).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "        42");

        let (output, _, _) = run_format("~10s", |ctx| {
            let hi = ctx.list(&[ctx.int(104).expect("fits"), ctx.int(105).expect("fits")])
                .expect("fits");
            Some(ctx.list(&[hi]).expect("fits"))
        });
        assert_eq!(output, "        hi");
    }

    #[test]
    fn base_directive_upper_and_lower() {
        let (output, _, _) = run_format("~.16B", |ctx| {
            Some(ctx.list(&[ctx.int(255).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "FF");

        let (output, _, _) = run_format("~.16b", |ctx| {
            Some(ctx.list(&[ctx.int(255).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "ff");

        let (output, _, _) = run_format("~.2B", |ctx| {
            Some(ctx.list(&[ctx.int(5).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "101");

        let (output, _, _) = run_format("~.36B", |ctx| {
            Some(ctx.list(&[ctx.int(1295).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "ZZ");

        // Negative values keep the sign.
        let (output, _, _) = run_format("~.16B", |ctx| {
            Some(ctx.list(&[ctx.int(-255).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "-FF");
    }

    #[test]
    fn bad_base_is_badarg() {
        for format in ["~.0b", "~.1b", "~.37b", "~.b"] {
            let (_, _, result) =
                run_format(format, |ctx| Some(ctx.list(&[ctx.int(5).expect("fits")]).expect("fits")));
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "format {format} should be badarg"
            );
        }
    }

    #[test]
    fn integer_directive_rejects_non_integers() {
        let (_, _, result) = run_format("~b", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            Some(ctx.list(&[a]).expect("fits"))
        });
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn ignore_directive_consumes_without_output() {
        let (output, _, _) = run_format("~i~w", |ctx| {
            let five = ctx.int(5).expect("fits");
            let seven = ctx.int(7).expect("fits");
            Some(ctx.list(&[five, seven]).expect("fits"))
        });
        assert_eq!(output, "7");

        // `~i` with no argument is badarg ("not enough arguments").
        let (_, _, result) = run_format("~i", |_ctx| Some(NIL));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn newline_and_literal_tilde() {
        let (output, _, _) = run_format("a~~b~n", |_ctx| Some(NIL));
        assert_eq!(output, "a~b\n");

        // A width on `~~` is badarg; a width on `~n` repeats the newline.
        let (_, _, result) = run_format("~5~~", |_ctx| Some(NIL));
        assert!(matches!(result, Err(BeamError::BadArg)));
        let (output, _, _) = run_format("~3n", |_ctx| Some(NIL));
        assert_eq!(output, "\n\n\n");
    }

    #[test]
    fn character_directive() {
        let (output, _, _) = run_format("~c", |ctx| {
            Some(ctx.list(&[ctx.int(65).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "A");

        // `~c` requires a small integer.
        let (_, _, result) = run_format("~c", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            Some(ctx.list(&[a]).expect("fits"))
        });
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn string_directive_accepts_atoms() {
        let (output, _, _) = run_format("~s", |ctx| {
            let hello = ctx.atom("hello").expect("seeded");
            Some(ctx.list(&[hello]).expect("fits"))
        });
        assert_eq!(output, "hello");
    }

    #[test]
    fn argument_count_is_exact() {
        // Too few arguments.
        let (_, _, result) = run_format("~w ~w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            Some(ctx.list(&[a]).expect("fits"))
        });
        assert!(matches!(result, Err(BeamError::BadArg)));

        // Too many arguments.
        let (_, _, result) = run_format("~w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            Some(ctx.list(&[a, b]).expect("fits"))
        });
        assert!(matches!(result, Err(BeamError::BadArg)));

        // A trailing `~` with no directive letter.
        let (_, _, result) = run_format("abc~", |_ctx| Some(NIL));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn format_1_uses_empty_args() {
        let (output, _, result) = run_format("hello~n", |_ctx| None);
        assert_eq!(output, "hello\n");
        assert!(result.is_ok());
    }

    #[test]
    fn bad_format_term_is_badarg() {
        let mut heap = Heap::new(64).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "hello", "a", "b", "abc", "hi"]);
        let mut sink = BytesSink::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let atom = ctx.atom("abc").expect("seeded");
        // A non-list format term.
        assert!(matches!(format_1(&mut ctx, &[atom]), Err(BeamError::BadArg)));
        // A format element that is not a byte-sized integer.
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
        assert_eq!(int_to_base_string(-255, 10, false), "-255");
        assert_eq!(int_to_base_string(35, 36, true), "Z");
    }
}
