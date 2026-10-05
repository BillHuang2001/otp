//! The `Line` chunk: the source-location table.
//!
//! Layout: `u32 version` (must be 0, else the whole chunk is ignored), `u32 flags`,
//! `u32 instr_count`, `u32 item_count`, `u32 name_count`; then `item_count - 1`
//! tagged items (`TAG_a <name_index>` / `TAG_i <line>`); then `name_count` ×
//! `{ i16 len, len bytes UTF-8 }` file names (`beam_file.c:443-602`).
//!
//! `items[0]` is the undefined location `{0, 0}` and `files[0]` is synthesized as
//! `<module>.erl`, so the returned table always has at least those entries when the
//! chunk is usable.
use beam_common::Result;

use crate::ir::LineTable;

/// Parses a `Line` chunk payload into the IR [`LineTable`].
///
/// `module_name` is used to synthesize `files[0]` (`<module>.erl`). A `version`
/// other than 0 means the chunk is ignored and a default (empty) table is
/// returned; a truncated or otherwise malformed chunk yields
/// `BeamError::BadArg`.
// TODO(beam_loader-impl): read the header, synthesize the undefined `items[0]` and
// TODO(beam_loader-impl): `<module_name>.erl` as `files[0]`, then decode the tagged
// TODO(beam_loader-impl): location items and the UTF-8 file-name table.
pub fn parse(data: &[u8], module_name: &str) -> Result<LineTable> {
    let _ = (data, module_name);
    Err(beam_common::BeamError::BadArg)
}
