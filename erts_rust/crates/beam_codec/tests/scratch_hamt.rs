//! TEMPORARY diagnostic experiment (not a deliverable). Compares our locally-built
//! HAMT image against the live oracle for several sizes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::encode_term;
use beam_term::map::{map_words_capacity, write_map};
use beam_term::prelude::{make_small, AtomTable, Eterm};

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    { p.is_file() }
}

fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "erl failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

fn parse_binary_list(out: &str) -> Vec<Vec<u8>> {
    let bytes = out.as_bytes();
    let mut images = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'<' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'>' && bytes[j + 1] == b'>') {
                j += 1;
            }
            let body = &out[start..j];
            let image = if body.trim().is_empty() {
                Vec::new()
            } else {
                body.split(',').map(|t| t.trim().parse::<u8>().unwrap()).collect()
            };
            images.push(image);
            i = j + 2;
        } else {
            i += 1;
        }
    }
    images
}

fn our_image(n: i64) -> Vec<u8> {
    let mut h = Heap::new(1 << 20).expect("heap");
    let atoms = AtomTable::new();
    let pairs: Vec<(Eterm, Eterm)> = (1..=n).map(|k| (make_small(k), make_small(k * 2))).collect();
    let hp = unsafe { h.alloc_words(map_words_capacity(pairs.len())) }.unwrap();
    let map = unsafe { write_map(hp, &pairs, &atoms) }.unwrap();
    encode_term(map, &atoms).unwrap()
}

#[test]
fn scratch_compare_hamt() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!("no oracle");
        return;
    };
    for n in [33usize, 40, 50, 64, 100, 1000] {
        // Oracle image
        let eval = format!(
            "io:format(\"~w~n\", [term_to_binary(maps:from_list([{{N, N*2}} || N <- lists:seq(1,{n})]))])"
        );
        let out = run_erl(&oracle, &eval);
        let theirs = parse_binary_list(&out).into_iter().next().unwrap();
        // Our locally-built map's ascending storage order, then reversed.
        let mut h = Heap::new(1 << 22).expect("heap");
        let atoms = AtomTable::new();
        let pairs: Vec<(Eterm, Eterm)> =
            (1..=n).map(|k| (make_small(k as i64), make_small(k as i64 * 2))).collect();
        let hp = unsafe { h.alloc_words(map_words_capacity(pairs.len())) }.unwrap();
        let map = unsafe { write_map(hp, &pairs, &atoms) }.unwrap();
        let ours_asc: Vec<(Eterm, Eterm)> =
            beam_term::map::map_pairs_storage_order(map).unwrap();
        let mut ours: Vec<u8> = Vec::new();
        ours.push(131);
        ours.push(116);
        ours.extend_from_slice(&(n as u32).to_be_bytes());
        for (k, v) in ours_asc.iter().rev() {
            ours.extend_from_slice(&encode_term(*k, &atoms).unwrap()[1..]);
            ours.extend_from_slice(&encode_term(*v, &atoms).unwrap()[1..]);
        }
        eprintln!(
            "n={n}: reversed-storage == oracle ? {}  (ours {} bytes, oracle {} bytes)",
            ours == theirs,
            ours.len(),
            theirs.len()
        );
    }
}
