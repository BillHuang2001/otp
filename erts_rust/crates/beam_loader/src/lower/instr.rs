//! The instruction-lowering match: one decoded [`RawInstr`] → one IR [`Instr`].
//!
//! This is the semantic core of the crate's lowering step, split out of
//! [`crate::lower`] so each file stays a readable unit (`../CONTEXT.md`,
//! "Files ~1000 lines"): `lower` owns function grouping, basic blocks and the
//! module assembly, this module owns the per-opcode vocabulary mapping.
//!
//! [`lower_instr`] maps every opcode the compiler emits to its named
//! [`Instr`] variant; an opcode the loader deliberately does not model is
//! preserved losslessly as [`Instr::Generic`]. Operand order is the file's
//! (`erts/emulator/beam/emu/ops.tab` + `genop.tab`'s `@spec` lines), never
//! re-ordered for convenience. A structurally mismatched operand (a non-label
//! where an `f` operand is required, a non-integer where a `u` operand is
//! required) yields `BeamError::BadArg` — the module never panics and never
//! `unwrap()`s file data.
use beam_common::{BeamError, Result};
use beam_term::Atom;

use crate::decode::RawInstr;
use crate::error::bad_file;
use crate::ir::{ImportRef, Instr, TestKind};
use crate::opcode::Opcode;
use crate::operand::{Label, Operand};

/// Lowers one decoded [`RawInstr`] into the IR [`Instr`].
///
/// Every opcode the compiler emits maps to its named variant; every other opcode is
/// preserved losslessly as [`Instr::Generic`]. Operand order is the file's
/// (`ops.tab`), e.g. `move Src Dst` and `gc_bif* Fail Live Bif Arg… Dst`. A
/// structurally mismatched operand (a non-label where a `f` operand is required, a
/// non-integer where a `u` operand is required) yields `BeamError::BadArg`.
pub(super) fn lower_instr(raw: &RawInstr) -> Result<Instr> {
    let ops = &raw.operands;
    match raw.opcode {
        // `label/1`: the compiler writes the label *number* with the `u` tag
        // (`encode_arg(Int, …)` in `beam_asm.erl`; `beam_disasm` prints a plain
        // integer), unlike jump/failure targets which use the `f` tag. Both forms
        // denote a label, so both are accepted here.
        Opcode::label => Ok(Instr::Label(label_value(operand(ops, 0)?)?)),
        // The `line` operand is the index into the `Line` chunk items (0 = undefined
        // location), which is exactly the IR's `LineRef`.
        Opcode::line => Ok(Instr::Line(as_u32(operand(ops, 0)?)?)),
        Opcode::func_info => Ok(Instr::FuncInfo {
            module: as_atom(operand(ops, 0)?)?,
            function: as_atom(operand(ops, 1)?)?,
            arity: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::int_code_end => Ok(Instr::IntCodeEnd),
        Opcode::r#move => Ok(Instr::Move {
            src: operand(ops, 0)?.clone(),
            dst: operand(ops, 1)?.clone(),
        }),
        // `gc_bif{1,2,3} Fail Live Bif Arg… Dst` (`erts/emulator/beam/emu/ops.tab`).
        // The BIF operand is the index into the `ImpT` chunk, not a `bif.tab` number.
        Opcode::gc_bif1 | Opcode::gc_bif2 | Opcode::gc_bif3 => {
            if ops.len() < 4 {
                return Err(bad_file("gc_bif has too few operands"));
            }
            let args = ops[3..ops.len() - 1].to_vec();
            let arity = u8::try_from(args.len()).map_err(|_| BeamError::SystemLimit)?;
            Ok(Instr::GcBif {
                arity,
                bif: ImportRef(as_u32(operand(ops, 2)?)?),
                fail: as_label(operand(ops, 0)?)?,
                live: as_u32(operand(ops, 1)?)?,
                args,
                dst: ops[ops.len() - 1].clone(),
            })
        }
        // `select_val Value Fail {Value,Label}*` — the alternatives arrive as one
        // `z,1` argument list, flattened into alternating value/label operands.
        Opcode::select_val => Ok(Instr::SelectVal {
            value: op(ops, 0)?,
            fail: as_label(operand(ops, 1)?)?,
            pairs: value_label_pairs(operand(ops, 2)?)?,
        }),
        Opcode::swap => Ok(Instr::Swap {
            a: operand(ops, 0)?.clone(),
            b: operand(ops, 1)?.clone(),
        }),
        Opcode::call => Ok(Instr::Call {
            arity: as_u32(operand(ops, 0)?)?,
            target: as_label(operand(ops, 1)?)?,
        }),
        Opcode::call_last => Ok(Instr::CallLast {
            arity: as_u32(operand(ops, 0)?)?,
            target: as_label(operand(ops, 1)?)?,
            deallocate: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::call_only => Ok(Instr::CallOnly {
            arity: as_u32(operand(ops, 0)?)?,
            target: as_label(operand(ops, 1)?)?,
        }),
        Opcode::call_ext => Ok(Instr::CallExt {
            arity: as_u32(operand(ops, 0)?)?,
            target: ImportRef(as_u32(operand(ops, 1)?)?),
        }),
        Opcode::call_ext_only => Ok(Instr::CallExtOnly {
            arity: as_u32(operand(ops, 0)?)?,
            target: ImportRef(as_u32(operand(ops, 1)?)?),
        }),
        Opcode::call_ext_last => Ok(Instr::CallExtLast {
            arity: as_u32(operand(ops, 0)?)?,
            target: ImportRef(as_u32(operand(ops, 1)?)?),
            deallocate: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::allocate => Ok(Instr::Allocate {
            stack_need: as_u32(operand(ops, 0)?)?,
            live: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::allocate_heap => Ok(Instr::AllocateHeap {
            stack_need: as_u32(operand(ops, 0)?)?,
            heap_need: as_u32(operand(ops, 1)?)?,
            live: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::test_heap => Ok(Instr::TestHeap {
            heap_need: as_u32(operand(ops, 0)?)?,
            live: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::deallocate => Ok(Instr::Deallocate(as_u32(operand(ops, 0)?)?)),
        Opcode::trim => Ok(Instr::Trim {
            n: as_u32(operand(ops, 0)?)?,
            remaining: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::r#return => Ok(Instr::Return),

        // ---- control flow: jumps, frames, tuples, lists ----------------------
        Opcode::jump => Ok(Instr::Jump {
            target: as_label(operand(ops, 0)?)?,
        }),
        // `init_yregs {Yregs}` — the registers arrive as one `z,1` argument list.
        Opcode::init_yregs => Ok(Instr::InitYregs {
            registers: list(operand(ops, 0)?)?.to_vec(),
        }),
        // `put_tuple2 Dst {Elements}` — OTP 22 replaced the `put_tuple`/`put` pair.
        Opcode::put_tuple2 => Ok(Instr::PutTuple2 {
            dst: op(ops, 0)?,
            elements: list(operand(ops, 1)?)?.to_vec(),
        }),
        Opcode::get_tuple_element => Ok(Instr::GetTupleElement {
            source: op(ops, 0)?,
            element: as_u32(operand(ops, 1)?)?,
            dst: op(ops, 2)?,
        }),
        // `select_tuple_arity Tuple Fail {Arity,Label}*` — the alternatives arrive as
        // one flattened `z,1` argument list, exactly like `select_val`.
        Opcode::select_tuple_arity => Ok(Instr::SelectTupleArity {
            tuple: op(ops, 0)?,
            fail: as_label(operand(ops, 1)?)?,
            destinations: value_label_pairs(operand(ops, 2)?)?,
        }),
        Opcode::put_list => Ok(Instr::PutList {
            head: op(ops, 0)?,
            tail: op(ops, 1)?,
            dst: op(ops, 2)?,
        }),
        Opcode::get_list => Ok(Instr::GetList {
            source: op(ops, 0)?,
            head: op(ops, 1)?,
            tail: op(ops, 2)?,
        }),
        Opcode::get_hd => Ok(Instr::GetHd {
            source: op(ops, 0)?,
            head: op(ops, 1)?,
        }),
        Opcode::get_tl => Ok(Instr::GetTl {
            source: op(ops, 0)?,
            tail: op(ops, 1)?,
        }),

        // ---- calls and funs ---------------------------------------------------
        Opcode::call_fun => Ok(Instr::CallFun {
            arity: as_u32(operand(ops, 0)?)?,
        }),
        // `call_fun2 Tag Arity Func` — `Tag` is the atom `safe`/`unsafe` or, when the
        // target fun is known statically, the file's fun-table (`FunT`) index.
        Opcode::call_fun2 => Ok(Instr::CallFun2 {
            tag: op(ops, 0)?,
            arity: as_u32(operand(ops, 1)?)?,
            func: op(ops, 2)?,
        }),
        Opcode::apply => Ok(Instr::Apply {
            arity: as_u32(operand(ops, 0)?)?,
        }),
        Opcode::apply_last => Ok(Instr::ApplyLast {
            arity: as_u32(operand(ops, 0)?)?,
            deallocate: as_u32(operand(ops, 1)?)?,
        }),
        // `make_fun3 Index Dst {Env}` (`make_fun2` is obsolete and never emitted).
        Opcode::make_fun3 => Ok(Instr::MakeFun3 {
            index: as_u32(operand(ops, 0)?)?,
            dst: op(ops, 1)?,
            env: list(operand(ops, 2)?)?.to_vec(),
        }),

        // ---- non-garbage-collecting BIF calls ---------------------------------
        // `bif0 Bif Dst` — the BIF operand is the index into the `ImpT` chunk.
        Opcode::bif0 => Ok(Instr::Bif0 {
            bif: ImportRef(as_u32(operand(ops, 0)?)?),
            dst: op(ops, 1)?,
        }),
        // `bif{1,2,3} Fail Bif Arg… Dst` — the same layout as `gc_bif*`, minus the
        // live-register count.
        Opcode::bif1 | Opcode::bif2 | Opcode::bif3 => {
            if ops.len() < 3 {
                return Err(bad_file("bif has too few operands"));
            }
            let args = ops[2..ops.len() - 1].to_vec();
            let arity = u8::try_from(args.len()).map_err(|_| BeamError::SystemLimit)?;
            Ok(Instr::Bif {
                arity,
                bif: ImportRef(as_u32(operand(ops, 1)?)?),
                fail: as_label(operand(ops, 0)?)?,
                args,
                dst: ops[ops.len() - 1].clone(),
            })
        }

        // ---- exceptions -------------------------------------------------------
        Opcode::badmatch => Ok(Instr::Badmatch { value: op(ops, 0)? }),
        Opcode::case_end => Ok(Instr::CaseEnd { value: op(ops, 0)? }),
        Opcode::if_end => Ok(Instr::IfEnd),
        Opcode::r#try => Ok(Instr::Try {
            frame: op(ops, 0)?,
            fail: as_label(operand(ops, 1)?)?,
        }),
        Opcode::catch => Ok(Instr::Catch {
            frame: op(ops, 0)?,
            fail: as_label(operand(ops, 1)?)?,
        }),
        Opcode::try_case => Ok(Instr::TryCase { frame: op(ops, 0)? }),
        Opcode::try_end => Ok(Instr::TryEnd { frame: op(ops, 0)? }),
        Opcode::try_case_end => Ok(Instr::TryCaseEnd { value: op(ops, 0)? }),
        Opcode::catch_end => Ok(Instr::CatchEnd { frame: op(ops, 0)? }),
        Opcode::raise => Ok(Instr::Raise {
            trace: op(ops, 0)?,
            value: op(ops, 1)?,
        }),
        Opcode::badrecord => Ok(Instr::Badrecord { value: op(ops, 0)? }),
        Opcode::build_stacktrace => Ok(Instr::BuildStacktrace),
        Opcode::raw_raise => Ok(Instr::RawRaise),

        // ---- floating point ---------------------------------------------------
        Opcode::fmove => Ok(Instr::Fmove {
            src: op(ops, 0)?,
            dst: op(ops, 1)?,
        }),
        Opcode::fconv => Ok(Instr::Fconv {
            src: op(ops, 0)?,
            dst: op(ops, 1)?,
        }),
        // `f{add,sub,mul,div} Fail Lhs Rhs Dst` — four opcodes, one operand layout.
        Opcode::fadd | Opcode::fsub | Opcode::fmul | Opcode::fdiv => {
            let fail = as_label(operand(ops, 0)?)?;
            let lhs = op(ops, 1)?;
            let rhs = op(ops, 2)?;
            let dst = op(ops, 3)?;
            Ok(match raw.opcode {
                Opcode::fadd => Instr::Fadd {
                    fail,
                    lhs,
                    rhs,
                    dst,
                },
                Opcode::fsub => Instr::Fsub {
                    fail,
                    lhs,
                    rhs,
                    dst,
                },
                Opcode::fmul => Instr::Fmul {
                    fail,
                    lhs,
                    rhs,
                    dst,
                },
                _ => Instr::Fdiv {
                    fail,
                    lhs,
                    rhs,
                    dst,
                },
            })
        }
        Opcode::fnegate => Ok(Instr::Fnegate {
            fail: as_label(operand(ops, 0)?)?,
            src: op(ops, 1)?,
            dst: op(ops, 2)?,
        }),

        // ---- line markers, receive and send -----------------------------------
        Opcode::executable_line => Ok(Instr::ExecutableLine {
            location: as_u32(operand(ops, 0)?)?,
            index: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::debug_line => Ok(Instr::DebugLine {
            kind: op(ops, 0)?,
            location: as_u32(operand(ops, 1)?)?,
            index: as_u32(operand(ops, 2)?)?,
            live: as_u32(operand(ops, 3)?)?,
        }),
        Opcode::send => Ok(Instr::Send),
        Opcode::loop_rec => Ok(Instr::LoopRec {
            fail: as_label(operand(ops, 0)?)?,
            dst: op(ops, 1)?,
        }),
        Opcode::loop_rec_end => Ok(Instr::LoopRecEnd {
            target: as_label(operand(ops, 0)?)?,
        }),
        Opcode::wait => Ok(Instr::Wait {
            target: as_label(operand(ops, 0)?)?,
        }),
        Opcode::wait_timeout => Ok(Instr::WaitTimeout {
            target: as_label(operand(ops, 0)?)?,
            time: op(ops, 1)?,
        }),
        Opcode::timeout => Ok(Instr::Timeout),
        Opcode::remove_message => Ok(Instr::RemoveMessage),
        Opcode::recv_marker_reserve => Ok(Instr::RecvMarkerReserve {
            marker: op(ops, 0)?,
        }),
        Opcode::recv_marker_bind => Ok(Instr::RecvMarkerBind {
            marker: op(ops, 0)?,
            reference: op(ops, 1)?,
        }),
        Opcode::recv_marker_clear => Ok(Instr::RecvMarkerClear {
            reference: op(ops, 0)?,
        }),
        Opcode::recv_marker_use => Ok(Instr::RecvMarkerUse {
            reference: op(ops, 0)?,
        }),

        // ---- maps and records -------------------------------------------------
        Opcode::get_map_elements => Ok(Instr::GetMapElements {
            fail: as_label(operand(ops, 0)?)?,
            source: op(ops, 1)?,
            pairs: key_value_pairs(operand(ops, 2)?)?,
        }),
        Opcode::has_map_fields => Ok(Instr::HasMapFields {
            fail: as_label(operand(ops, 0)?)?,
            source: op(ops, 1)?,
            keys: list(operand(ops, 2)?)?.to_vec(),
        }),
        Opcode::put_map_assoc => Ok(Instr::PutMapAssoc {
            fail: as_label(operand(ops, 0)?)?,
            source: op(ops, 1)?,
            dst: op(ops, 2)?,
            live: as_u32(operand(ops, 3)?)?,
            pairs: key_value_pairs(operand(ops, 4)?)?,
        }),
        Opcode::put_map_exact => Ok(Instr::PutMapExact {
            fail: as_label(operand(ops, 0)?)?,
            source: op(ops, 1)?,
            dst: op(ops, 2)?,
            live: as_u32(operand(ops, 3)?)?,
            pairs: key_value_pairs(operand(ops, 4)?)?,
        }),
        // `update_record Hint Size Src Dst {Index,Value}*` — `Hint` is the atom
        // `copy`/`reuse`, `Size` the arity of the tuple being copied.
        Opcode::update_record => Ok(Instr::UpdateRecord {
            hint: op(ops, 0)?,
            size: as_u32(operand(ops, 1)?)?,
            src: op(ops, 2)?,
            dst: op(ops, 3)?,
            updates: key_value_pairs(operand(ops, 4)?)?,
        }),

        // ---- binary syntax ----------------------------------------------------
        Opcode::bs_start_match3 => Ok(Instr::BsStartMatch3 {
            fail: as_label(operand(ops, 0)?)?,
            bin: op(ops, 1)?,
            live: as_u32(operand(ops, 2)?)?,
            dst: op(ops, 3)?,
        }),
        // `bs_start_match4 Fail Live Src Dst` — `Fail` is a label, or the atom
        // `no_fail`/`resume`, so it stays a raw operand rather than a label.
        Opcode::bs_start_match4 => Ok(Instr::BsStartMatch4 {
            fail: op(ops, 0)?,
            live: as_u32(operand(ops, 1)?)?,
            src: op(ops, 2)?,
            dst: op(ops, 3)?,
        }),
        // `bs_match Fail Ctx {Commands}` — the commands stay a flat operand list.
        Opcode::bs_match => Ok(Instr::BsMatch {
            fail: as_label(operand(ops, 0)?)?,
            ctx: op(ops, 1)?,
            commands: list(operand(ops, 2)?)?.to_vec(),
        }),
        // `bs_match_string Fail Ctx Bits Offset` — `Offset` indexes the `StrT` table.
        Opcode::bs_match_string => Ok(Instr::BsMatchString {
            fail: as_label(operand(ops, 0)?)?,
            ctx: op(ops, 1)?,
            bits: as_u32(operand(ops, 2)?)?,
            offset: as_u32(operand(ops, 3)?)?,
        }),
        Opcode::bs_get_tail => Ok(Instr::BsGetTail {
            ctx: op(ops, 0)?,
            dst: op(ops, 1)?,
            live: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::bs_get_position => Ok(Instr::BsGetPosition {
            ctx: op(ops, 0)?,
            dst: op(ops, 1)?,
            live: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::bs_set_position => Ok(Instr::BsSetPosition {
            ctx: op(ops, 0)?,
            pos: op(ops, 1)?,
        }),
        // `bs_get_{integer,float,binary}2 Fail Ctx Live Size Unit Flags Dst` — three
        // opcodes, one operand layout.
        Opcode::bs_get_integer2 | Opcode::bs_get_float2 | Opcode::bs_get_binary2 => {
            let fail = as_label(operand(ops, 0)?)?;
            let ctx = op(ops, 1)?;
            let live = as_u32(operand(ops, 2)?)?;
            let size = op(ops, 3)?;
            let unit = as_u32(operand(ops, 4)?)?;
            let flags = as_u32(operand(ops, 5)?)?;
            let dst = op(ops, 6)?;
            Ok(match raw.opcode {
                Opcode::bs_get_integer2 => Instr::BsGetInteger2 {
                    fail,
                    ctx,
                    live,
                    size,
                    unit,
                    flags,
                    dst,
                },
                Opcode::bs_get_float2 => Instr::BsGetFloat2 {
                    fail,
                    ctx,
                    live,
                    size,
                    unit,
                    flags,
                    dst,
                },
                _ => Instr::BsGetBinary2 {
                    fail,
                    ctx,
                    live,
                    size,
                    unit,
                    flags,
                    dst,
                },
            })
        }
        Opcode::bs_skip_bits2 => Ok(Instr::BsSkipBits2 {
            fail: as_label(operand(ops, 0)?)?,
            ctx: op(ops, 1)?,
            size: op(ops, 2)?,
            unit: as_u32(operand(ops, 3)?)?,
            flags: as_u32(operand(ops, 4)?)?,
        }),
        Opcode::bs_test_tail2 => Ok(Instr::BsTestTail2 {
            fail: as_label(operand(ops, 0)?)?,
            ctx: op(ops, 1)?,
            bits: as_u32(operand(ops, 2)?)?,
        }),
        // `bs_get_utf{8,16,32} Fail Ctx Live Flags Dst`.
        Opcode::bs_get_utf8 | Opcode::bs_get_utf16 | Opcode::bs_get_utf32 => {
            let fail = as_label(operand(ops, 0)?)?;
            let ctx = op(ops, 1)?;
            let live = as_u32(operand(ops, 2)?)?;
            let flags = as_u32(operand(ops, 3)?)?;
            let dst = op(ops, 4)?;
            Ok(match raw.opcode {
                Opcode::bs_get_utf8 => Instr::BsGetUtf8 {
                    fail,
                    ctx,
                    live,
                    flags,
                    dst,
                },
                Opcode::bs_get_utf16 => Instr::BsGetUtf16 {
                    fail,
                    ctx,
                    live,
                    flags,
                    dst,
                },
                _ => Instr::BsGetUtf32 {
                    fail,
                    ctx,
                    live,
                    flags,
                    dst,
                },
            })
        }
        // `bs_skip_utf{8,16,32} Fail Ctx Live Flags`.
        Opcode::bs_skip_utf8 | Opcode::bs_skip_utf16 | Opcode::bs_skip_utf32 => {
            let fail = as_label(operand(ops, 0)?)?;
            let ctx = op(ops, 1)?;
            let live = as_u32(operand(ops, 2)?)?;
            let flags = as_u32(operand(ops, 3)?)?;
            Ok(match raw.opcode {
                Opcode::bs_skip_utf8 => Instr::BsSkipUtf8 {
                    fail,
                    ctx,
                    live,
                    flags,
                },
                Opcode::bs_skip_utf16 => Instr::BsSkipUtf16 {
                    fail,
                    ctx,
                    live,
                    flags,
                },
                _ => Instr::BsSkipUtf32 {
                    fail,
                    ctx,
                    live,
                    flags,
                },
            })
        }
        Opcode::bs_init_writable => Ok(Instr::BsInitWritable),
        // `bs_create_bin Fail Alloc Live Unit Dst {Segments}` — the segments stay a
        // flat operand list (type/size/unit/flags/value per segment).
        Opcode::bs_create_bin => Ok(Instr::BsCreateBin {
            fail: as_label(operand(ops, 0)?)?,
            alloc: as_u32(operand(ops, 1)?)?,
            live: as_u32(operand(ops, 2)?)?,
            unit: as_u32(operand(ops, 3)?)?,
            dst: op(ops, 4)?,
            segments: list(operand(ops, 5)?)?.to_vec(),
        }),

        // ---- module-level markers ---------------------------------------------
        Opcode::nif_start => Ok(Instr::NifStart),
        Opcode::on_load => Ok(Instr::OnLoad),

        op => {
            // The `is_*`/`test_arity` family: `Test Fail Arg… `. Anything else keeps
            // its opcode and operands verbatim for the engine/JIT to extend into.
            if let Some(kind) = TestKind::from_opcode(op) {
                let fail = as_label(operand(ops, 0)?)?;
                Ok(Instr::Test {
                    kind,
                    fail,
                    args: ops[1..].to_vec(),
                })
            } else {
                Ok(Instr::Generic {
                    op,
                    operands: raw.operands.clone(),
                    offset: raw.offset,
                })
            }
        }
    }
}

/// Fetches the operand at `index`, or reports a structurally short instruction.
fn operand(ops: &[Operand], index: usize) -> Result<&Operand> {
    ops.get(index)
        .ok_or_else(|| bad_file("instruction is missing an operand"))
}

/// Requires a label (`f`) operand — the failure sentinel [`crate::operand::NO_FAIL`]
/// for `f,0` is a valid label.
fn as_label(operand: &Operand) -> Result<Label> {
    match operand {
        Operand::Label(label) => Ok(*label),
        _ => Err(bad_file("expected a label operand")),
    }
}

/// A label *value* from either the `u` integer form the compiler writes for
/// `label/1` or the `f` label form used for jump and failure targets.
fn label_value(operand: &Operand) -> Result<Label> {
    match operand {
        Operand::Label(label) => Ok(*label),
        Operand::Integer(value) if *value >= 0 && *value <= i64::from(u32::MAX) => {
            Ok(Label(*value as u32))
        }
        _ => Err(bad_file("expected a label operand")),
    }
}

/// Requires an unsigned integer (`u`) operand that fits the IR's 32-bit width.
fn as_u32(operand: &Operand) -> Result<u32> {
    match operand {
        Operand::Integer(value) if *value >= 0 && *value <= i64::from(u32::MAX) => {
            Ok(*value as u32)
        }
        _ => Err(bad_file("expected an unsigned integer operand")),
    }
}

/// Requires an atom (`a`) operand.
fn as_atom(operand: &Operand) -> Result<Atom> {
    match operand {
        Operand::Atom(atom) => Ok(*atom),
        _ => Err(bad_file("expected an atom operand")),
    }
}

/// Fetches the operand at `index` as an owned value.
///
/// The IR variants own their operands, so the arms that move an operand into a
/// field clone it through this helper (the counterpart of [`operand`]).
fn op(ops: &[Operand], index: usize) -> Result<Operand> {
    Ok(operand(ops, index)?.clone())
}

/// Requires a `z,1` argument-list operand and borrows its elements.
///
/// A list operand holds both the element count and the elements (the decoder
/// consumes the `u` count inside the `z,1` sub-tag), so the elements are all a
/// caller needs; a non-list operand is a structural mismatch.
fn list(operand: &Operand) -> Result<&[Operand]> {
    match operand {
        Operand::List(items) => Ok(items),
        _ => Err(bad_file("expected an argument-list operand")),
    }
}

/// Splits a flattened `{Value,Label}*` alternative list into `(value, label)`
/// pairs — the shared shape of `select_val` and `select_tuple_arity`.
fn value_label_pairs(operand: &Operand) -> Result<Vec<(Operand, Label)>> {
    let items = list(operand)?;
    if items.len() % 2 != 0 {
        return Err(bad_file("select alternative list has an odd length"));
    }
    let mut pairs = Vec::with_capacity(items.len() / 2);
    for pair in items.chunks_exact(2) {
        pairs.push((pair[0].clone(), as_label(&pair[1])?));
    }
    Ok(pairs)
}

/// Splits a flattened `{Key,Value}*` list into `(key, value)` pairs — the shared
/// shape of the map/record update and lookup instructions.
fn key_value_pairs(operand: &Operand) -> Result<Vec<(Operand, Operand)>> {
    let items = list(operand)?;
    if items.len() % 2 != 0 {
        return Err(bad_file("map/record update list has an odd length"));
    }
    let mut pairs = Vec::with_capacity(items.len() / 2);
    for pair in items.chunks_exact(2) {
        pairs.push((pair[0].clone(), pair[1].clone()));
    }
    Ok(pairs)
}
