//! Mono-runtime scanner for MTG Arena on Windows (and Wine).
//!
//! Mirrors the IL2CPP scanner logic in `src/napi/mod.rs::macos_backend`
//! but uses Mono runtime metadata layouts. C# object field offsets are
//! identical across IL2CPP and Mono because both backends compile from
//! the same Unity 2022.3.62f2 source — only runtime-metadata access
//! patterns differ.

use crate::mono_reader::MonoReader;
use std::collections::{HashMap, HashSet};
use std::process::Command;

/// Pointer plausibility bounds. Wine on macOS maps the Windows process's
/// virtual memory into the low half of 64-bit address space. We accept
/// anything above 0x10000 (minimum Windows allocation granularity) and
/// below 0x7FFF_FFFF_FFFF (Windows user-mode ceiling). These are wider
/// than the macOS IL2CPP scanner's [0x1_0000_0000, 0x4_0000_0000]
/// because Wine's address layout differs from native macOS arm64.
const MIN_PTR: usize = 0x10000;
const MAX_PTR: usize = 0x7FFF_FFFF_FFFF;

/// MonoClass struct offsets from `src/mono/offsets.rs::unity_2022_3()`.
/// Inlined here for clarity; if Arena changes Unity version, update
/// these from the offsets module.
mod mono_class_offsets {
    /// MonoClass.name — pointer to ASCII class name string
    pub const NAME: usize = 0x48;
    /// MonoClass.namespace — pointer to ASCII namespace string
    pub const NAMESPACE: usize = 0x50;
    /// MonoClass.fields — pointer to MonoClassField array (or inline on some forks)
    pub const FIELDS: usize = 0x98;
    /// MonoClass.field.count
    pub const FIELD_COUNT: usize = 0xE0;
}

/// Read a Mono class's name from its MonoClass pointer.
/// On Mono, an object's class is at `read_ptr(read_ptr(obj))` (obj →
/// MonoVTable → MonoClass). The caller passes the MonoClass pointer.
pub fn read_mono_class_name(reader: &MonoReader, class_ptr: usize) -> String {
    if class_ptr < MIN_PTR || class_ptr > MAX_PTR {
        return String::new();
    }
    let name_ptr = reader.read_ptr(class_ptr + mono_class_offsets::NAME);
    if name_ptr < MIN_PTR || name_ptr > MAX_PTR {
        return String::new();
    }
    reader.read_ascii_string(name_ptr)
}

/// Resolve MonoClass from an object address via Mono's vtable indirection.
/// `obj[+0x00]` is a `MonoVTable*`; `vtable[+0x00]` is the `MonoClass*`.
pub fn obj_to_mono_class(reader: &MonoReader, obj: usize) -> usize {
    let vtable = reader.read_ptr(obj);
    if vtable < MIN_PTR || vtable > MAX_PTR {
        return 0;
    }
    reader.read_ptr(vtable)
}

/// Field info extracted from a MonoClass's field array.
#[derive(Debug, Clone)]
pub struct MonoFieldInfo {
    pub name: String,
    pub offset: i32,
    pub is_static: bool,
}

/// Enumerate instance fields on a MonoClass.
///
/// On Unity's Mono fork, `MonoClass.fields` at offset 0x98 points to
/// (or contains inline) an array of `MonoClassField` entries. Each entry
/// is 0x20 bytes. We read up to `field_count` entries and extract
/// name + offset for each.
///
/// The field-entry internal layout is discovered empirically — the first
/// run's diagnostic dump (via `MTGA_DEBUG_MONO=1`) confirms which bytes
/// are the name pointer and which are the instance offset. The initial
/// assumption matches upstream's `FieldDefinition::new`:
///   - `entry + 0x00`: MonoType* (8 bytes)
///   - `entry + 0x08`: name_ptr  (8 bytes) — our primary target
///   - `entry + 0x10`: parent_class_ptr (8 bytes)
///   - `entry + 0x18`: offset (i32) — instance offset
///   - `entry + 0x1C`: token (u32)
/// Total stride: 0x20 = 32 bytes.
///
/// **If the first run produces garbage field names**, adjust the
/// `NAME_OFF` and `OFFSET_OFF` constants below and re-run.
pub fn mono_get_class_fields(reader: &MonoReader, class_ptr: usize) -> Vec<MonoFieldInfo> {
    const STRIDE: usize = 0x20;
    const NAME_OFF: usize = 0x08;   // Offset of name_ptr within MonoClassField
    const OFFSET_OFF: usize = 0x18; // Offset of field_offset (i32) within MonoClassField
    const MAX_FIELDS: usize = 60;

    let field_count = reader.read_i32(class_ptr + mono_class_offsets::FIELD_COUNT);
    if field_count <= 0 || field_count > MAX_FIELDS as i32 {
        return Vec::new();
    }

    // Read the fields base. In Unity's Mono fork this is typically
    // a pointer at MonoClass + 0x98 that we dereference. If reading
    // as a pointer gives a valid address, use it. Otherwise try
    // inline (class_ptr + 0x98 directly, as upstream does).
    let fields_ptr_raw = reader.read_ptr(class_ptr + mono_class_offsets::FIELDS);
    let fields_base = if fields_ptr_raw >= MIN_PTR && fields_ptr_raw <= MAX_PTR {
        // Dereferenced pointer — standard Mono layout.
        fields_ptr_raw
    } else {
        // Inline fields — some Unity Mono forks store fields at class + FIELDS directly.
        class_ptr + mono_class_offsets::FIELDS
    };

    let debug = std::env::var("MTGA_DEBUG_MONO").is_ok();
    let mut result = Vec::with_capacity(field_count as usize);
    for i in 0..field_count as usize {
        let entry = fields_base + i * STRIDE;
        let name_ptr = reader.read_ptr(entry + NAME_OFF);
        if name_ptr < MIN_PTR || name_ptr > MAX_PTR {
            if debug {
                eprintln!(
                    "mono_get_class_fields: field[{}] at 0x{:x}: name_ptr=0x{:x} invalid, stopping",
                    i, entry, name_ptr,
                );
            }
            break;
        }
        let name = reader.read_ascii_string(name_ptr);
        if name.is_empty() || name.len() > 128 {
            if debug {
                eprintln!(
                    "mono_get_class_fields: field[{}] at 0x{:x}: empty/long name, stopping",
                    i, entry,
                );
            }
            break;
        }
        let offset = reader.read_i32(entry + OFFSET_OFF);

        // Determine if static by checking if the type pointer has
        // the static flag set. On Mono, MonoType at entry+0x00 has
        // attributes at +0x08, and static is bit 0x10. If the type
        // pointer is invalid, assume instance (non-static).
        let type_ptr = reader.read_ptr(entry);
        let is_static = if type_ptr >= MIN_PTR && type_ptr <= MAX_PTR {
            let attrs = reader.read_u32(type_ptr + 0x08);
            (attrs & 0x10) != 0
        } else {
            false
        };

        if debug {
            eprintln!(
                "  field[{}] {:?} @ 0x{:x} (static: {})",
                i, name, offset, is_static,
            );
        }
        result.push(MonoFieldInfo { name, offset, is_static });
    }
    result
}

/// Find all writable heap regions for the given process via `vmmap`.
/// Same logic as `macos_backend::find_scannable_heap_regions` but
/// excludes regions containing `mono-2.0-bdwgc` or `UnityPlayer`
/// instead of `GameAssembly` (since Wine Arena uses Mono, not IL2CPP).
pub fn find_scannable_heap_regions(pid: u32) -> Vec<(usize, usize)> {
    let output = Command::new("vmmap")
        .args(["-wide", &pid.to_string()])
        .output()
        .ok();

    let mut result: Vec<(usize, usize)> = Vec::new();
    const MIN_SIZE: usize = 1 << 20; // 1 MB
    const MAX_SIZE: usize = 4usize << 30; // 4 GB

    if let Some(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            // Exclude runtime modules — their data segments are metadata,
            // not GC-managed C# objects.
            if line.contains("mono-2.0-bdwgc")
                || line.contains("UnityPlayer")
                || line.contains("GameAssembly")
            {
                continue;
            }
            if !line.contains("rw-") {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            let addr_field_idx = parts.iter().position(|p| {
                p.contains('-')
                    && p.split('-').count() == 2
                    && p.chars().next().map_or(false, |c| c.is_ascii_hexdigit())
            });
            let idx = match addr_field_idx {
                Some(i) => i,
                None => continue,
            };
            let addr_parts: Vec<&str> = parts[idx].split('-').collect();
            if addr_parts.len() != 2 {
                continue;
            }
            let start = match usize::from_str_radix(addr_parts[0], 16) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let end = match usize::from_str_radix(addr_parts[1], 16) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if end <= start {
                continue;
            }
            let size = end - start;
            if size < MIN_SIZE || size > MAX_SIZE {
                continue;
            }
            result.push((start, end));
        }
    }
    result.sort();
    result.dedup();
    result
}

// ──────────────────────────────────────────────────────────────────
// readMtgaCards — scan for the card-collection Dictionary<int, int>
// ──────────────────────────────────────────────────────────────────

/// Scan heap for a Dictionary<int, int> matching the card-collection
/// signature. Identical to the IL2CPP version — no class metadata
/// involved; pure data-shape invariant.
pub fn scan_heap_for_cards_dictionary(reader: &MonoReader, pid: u32) -> usize {
    const MIN_COUNT: i32 = 500;
    const MAX_COUNT: i32 = 50_000;
    const MIN_CARD_ID: i32 = 1;
    const MAX_CARD_ID: i32 = 200_000;
    const MIN_QUANTITY: i32 = 1;
    const MAX_QUANTITY: i32 = 4;
    const SAMPLE_ENTRIES: usize = 30;
    const MIN_VALID_SAMPLES: usize = 12;

    let debug = std::env::var("MTGA_DEBUG_MONO").is_ok();
    let heap_regions = find_scannable_heap_regions(pid);
    if debug {
        eprintln!(
            "mono::scan_heap_for_cards_dictionary: scanning {} heap regions",
            heap_regions.len(),
        );
    }

    let mut best_addr: usize = 0;
    let mut best_count: i32 = 0;
    let mut candidates_examined = 0usize;

    for (start, end) in heap_regions {
        let size = end - start;
        let buf = reader.read_bytes(start, size);
        if buf.len() != size {
            continue;
        }
        let slot_count = size / 8;
        let mut i = 0;
        while i + 5 < slot_count {
            let base = i * 8;
            if base + 0x24 > buf.len() {
                break;
            }
            let buckets_ptr = u64::from_le_bytes(
                buf[base + 0x10..base + 0x18].try_into().unwrap_or([0; 8]),
            ) as usize;
            let entries_ptr = u64::from_le_bytes(
                buf[base + 0x18..base + 0x20].try_into().unwrap_or([0; 8]),
            ) as usize;
            let count = i32::from_le_bytes(
                buf[base + 0x20..base + 0x24].try_into().unwrap_or([0; 4]),
            );
            if count < MIN_COUNT
                || count > MAX_COUNT
                || buckets_ptr < MIN_PTR
                || buckets_ptr > MAX_PTR
                || entries_ptr < MIN_PTR
                || entries_ptr > MAX_PTR
            {
                i += 1;
                continue;
            }
            candidates_examined += 1;

            let mut valid = 0usize;
            for entry_idx in 0..SAMPLE_ENTRIES {
                let entry_addr = entries_ptr + 0x20 + entry_idx * 16;
                let entry_bytes = reader.read_bytes(entry_addr, 16);
                if entry_bytes.len() != 16 {
                    break;
                }
                let hash = i32::from_le_bytes(entry_bytes[0..4].try_into().unwrap_or([0; 4]));
                let key = i32::from_le_bytes(entry_bytes[8..12].try_into().unwrap_or([0; 4]));
                let value = i32::from_le_bytes(entry_bytes[12..16].try_into().unwrap_or([0; 4]));
                if hash == -1 {
                    continue;
                }
                if hash == key
                    && key >= MIN_CARD_ID
                    && key <= MAX_CARD_ID
                    && value >= MIN_QUANTITY
                    && value <= MAX_QUANTITY
                {
                    valid += 1;
                }
            }
            if valid >= MIN_VALID_SAMPLES && count > best_count {
                best_addr = start + base;
                best_count = count;
            }
            i += 1;
        }
    }
    if debug {
        eprintln!(
            "mono::scan_heap_for_cards_dictionary: examined {} candidates, best=0x{:x} count={}",
            candidates_examined, best_addr, best_count,
        );
    }
    best_addr
}

/// Read card entries from a Dictionary<int, int> at the given address.
pub fn read_cards_dictionary_entries(reader: &MonoReader, dict_addr: usize) -> Vec<(i32, i32)> {
    const MIN_CARD_ID: i32 = 1;
    const MAX_CARD_ID: i32 = 200_000;
    const MIN_QUANTITY: i32 = 1;
    const MAX_QUANTITY: i32 = 4;

    let entries_ptr = reader.read_ptr(dict_addr + 0x18);
    let count = reader.read_i32(dict_addr + 0x20);
    if entries_ptr < MIN_PTR || count <= 0 {
        return Vec::new();
    }
    let mut entries = Vec::new();
    for i in 0..count.min(50_000) as usize {
        let entry_addr = entries_ptr + 0x20 + i * 16;
        let hash = reader.read_i32(entry_addr);
        let key = reader.read_i32(entry_addr + 8);
        let value = reader.read_i32(entry_addr + 12);
        if hash == -1 {
            continue;
        }
        if hash != key || key < MIN_CARD_ID || key > MAX_CARD_ID || value < MIN_QUANTITY || value > MAX_QUANTITY
        {
            continue;
        }
        entries.push((key, value));
    }
    entries
}

/// Public entry point: read the card collection from a Mono-based Arena process.
pub fn read_mtga_cards_mono(process_name: &str) -> Result<Vec<(i32, i32)>, String> {
    let pid = find_wine_pid(process_name)?;
    let reader = MonoReader::new(pid);

    let dict_addr = scan_heap_for_cards_dictionary(&reader, pid);
    if dict_addr == 0 {
        return Err(
            "Cards dictionary not found via Mono heap scan. \
             Either Arena isn't fully loaded or the Dictionary<int,int> \
             layout has changed."
                .to_string(),
        );
    }
    let entries = read_cards_dictionary_entries(&reader, dict_addr);
    if entries.is_empty() {
        return Err(format!(
            "Found Cards dictionary at 0x{:x} but it had no valid entries.",
            dict_addr,
        ));
    }
    Ok(entries)
}

// ──────────────────────────────────────────────────────────────────
// readMtgaCardDatabase — scan for Dictionary<int, CardPrintingData*>
// ──────────────────────────────────────────────────────────────────

/// Scan heap for a Dictionary<int, CardPrintingData*> with stride-24
/// entries. Two-pass: filter by hash==key + count range, then verify
/// entry value class names via Mono vtable indirection.
pub fn scan_heap_for_card_printing_dictionary(
    reader: &MonoReader,
    pid: u32,
) -> Option<(usize, i32, usize)> {
    // Returns (dict_addr, count, runtime_value_class_ptr)
    const MIN_COUNT: i32 = 5_000;
    const MAX_COUNT: i32 = 100_000;
    const SAMPLE_ENTRIES: usize = 30;
    const MIN_HASH_KEY_MATCHES: usize = 10;

    let debug = std::env::var("MTGA_DEBUG_MONO").is_ok();
    let heap_regions = find_scannable_heap_regions(pid);
    if debug {
        eprintln!(
            "mono::scan_heap_for_card_printing_dictionary: scanning {} regions",
            heap_regions.len(),
        );
    }

    // (dict_addr, count, hash_key_matches, first_value_class_ptr)
    let mut candidates: Vec<(usize, i32, usize, usize)> = Vec::new();

    for (start, end) in heap_regions {
        let size = end - start;
        let buf = reader.read_bytes(start, size);
        if buf.len() != size {
            continue;
        }
        let slot_count = size / 8;
        let mut i = 0;
        while i + 5 < slot_count {
            let base = i * 8;
            if base + 0x24 > buf.len() {
                break;
            }
            let buckets_ptr = u64::from_le_bytes(
                buf[base + 0x10..base + 0x18].try_into().unwrap_or([0; 8]),
            ) as usize;
            let entries_ptr = u64::from_le_bytes(
                buf[base + 0x18..base + 0x20].try_into().unwrap_or([0; 8]),
            ) as usize;
            let count = i32::from_le_bytes(
                buf[base + 0x20..base + 0x24].try_into().unwrap_or([0; 4]),
            );
            if count < MIN_COUNT
                || count > MAX_COUNT
                || buckets_ptr < MIN_PTR
                || buckets_ptr > MAX_PTR
                || entries_ptr < MIN_PTR
                || entries_ptr > MAX_PTR
            {
                i += 1;
                continue;
            }

            let mut hash_key_matches = 0usize;
            let mut first_value_class: usize = 0;
            for entry_idx in 0..SAMPLE_ENTRIES {
                let entry_addr = entries_ptr + 0x20 + entry_idx * 24;
                let entry_bytes = reader.read_bytes(entry_addr, 24);
                if entry_bytes.len() != 24 {
                    break;
                }
                let hash = i32::from_le_bytes(entry_bytes[0..4].try_into().unwrap_or([0; 4]));
                if hash == -1 {
                    continue;
                }
                let key = i32::from_le_bytes(entry_bytes[8..12].try_into().unwrap_or([0; 4]));
                if hash != key || key < 1 || key > 200_000 {
                    continue;
                }
                hash_key_matches += 1;
                if first_value_class == 0 {
                    let value_ptr = u64::from_le_bytes(
                        entry_bytes[16..24].try_into().unwrap_or([0; 8]),
                    ) as usize;
                    if value_ptr >= MIN_PTR && value_ptr <= MAX_PTR {
                        // Mono vtable indirection: value → vtable → class
                        let class = obj_to_mono_class(reader, value_ptr);
                        if class >= MIN_PTR && class <= MAX_PTR {
                            first_value_class = class;
                        }
                    }
                }
            }
            if hash_key_matches >= MIN_HASH_KEY_MATCHES && first_value_class != 0 {
                candidates.push((start + base, count, hash_key_matches, first_value_class));
            }
            i += 1;
        }
    }

    // Resolve class names and filter for card-printing classes
    let accepted_names = ["CardPrintingData", "CardPrintingRecord"];
    let mut name_cache: HashMap<usize, String> = HashMap::new();
    for (_, _, _, class_ptr) in &candidates {
        name_cache
            .entry(*class_ptr)
            .or_insert_with(|| read_mono_class_name(reader, *class_ptr));
    }
    if debug {
        eprintln!(
            "mono::scan_heap_for_card_printing_dictionary: {} candidates, {} unique value classes:",
            candidates.len(),
            name_cache.len(),
        );
        for (c, n) in &name_cache {
            eprintln!("  0x{:x} -> {:?}", c, n);
        }
    }

    let cpr_classes: HashSet<usize> = name_cache
        .iter()
        .filter_map(|(c, n)| {
            if accepted_names.iter().any(|a| *a == n.as_str()) {
                Some(*c)
            } else {
                None
            }
        })
        .collect();

    let mut winners: Vec<_> = candidates
        .into_iter()
        .filter(|(_, _, _, c)| cpr_classes.contains(c))
        .collect();
    winners.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| b.1.cmp(&a.1)));
    winners.first().map(|(a, c, _, cl)| (*a, *c, *cl))
}

/// Walk a Dictionary<int, CardPrintingData*> with stride-24 entries.
pub fn read_card_printing_entries(
    reader: &MonoReader,
    dict_addr: usize,
    value_class: usize,
) -> Vec<(i32, usize)> {
    let entries_ptr = reader.read_ptr(dict_addr + 0x18);
    let count = reader.read_i32(dict_addr + 0x20);
    if entries_ptr < MIN_PTR || count <= 0 {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(count as usize);
    for i in 0..count.min(100_000) as usize {
        let entry_addr = entries_ptr + 0x20 + i * 24;
        let hash = reader.read_i32(entry_addr);
        if hash == -1 {
            continue;
        }
        let key = reader.read_i32(entry_addr + 8);
        if hash != key || key < 1 || key > 200_000 {
            continue;
        }
        let value_ptr = reader.read_ptr(entry_addr + 16);
        if value_ptr < MIN_PTR {
            continue;
        }
        // Mono: obj → vtable → class
        let obj_class = obj_to_mono_class(reader, value_ptr);
        if obj_class != value_class {
            continue;
        }
        result.push((key, value_ptr));
    }
    result
}

/// Resolve field offsets for reading card-printing data.
/// Same logic as the IL2CPP version: find `Record` field on
/// CardPrintingData (if applicable), combine with CardPrintingRecord
/// field offsets, adjust for embedded-struct header omission.
#[derive(Debug, Clone)]
pub struct CardFieldOffsets {
    pub grp_id: usize,
    pub title_id: usize,
    pub expansion_code: usize,
    pub collector_number: usize,
}

pub fn resolve_card_field_offsets(
    reader: &MonoReader,
    runtime_value_class: usize,
    cpr_class_hint: usize,
) -> Option<CardFieldOffsets> {
    // Try to find CardPrintingRecord offsets. First try the runtime class,
    // then try cpr_class_hint as a fallback.
    let find_cpr_offsets = |class: usize| -> Option<(usize, usize, usize, usize)> {
        let fields = mono_get_class_fields(reader, class);
        let find = |name: &str| -> Option<usize> {
            fields.iter().find(|f| !f.is_static && f.name == name).map(|f| f.offset as usize)
        };
        Some((find("GrpId")?, find("TitleId")?, find("ExpansionCode")?, find("CollectorNumber")?))
    };

    const IL2CPP_OBJ_HEADER: usize = 0x10;
    let runtime_name = read_mono_class_name(reader, runtime_value_class);

    if runtime_name == "CardPrintingRecord" {
        let (g, t, e, c) = find_cpr_offsets(runtime_value_class)?;
        return Some(CardFieldOffsets { grp_id: g, title_id: t, expansion_code: e, collector_number: c });
    }

    // CardPrintingData: look for embedded Record struct
    let runtime_fields = mono_get_class_fields(reader, runtime_value_class);
    let record_field = runtime_fields.iter().find(|f| {
        !f.is_static
            && (f.name == "Record"
                || f.name == "<Record>k__BackingField"
                || f.name == "_record")
    })?;
    let record_offset = record_field.offset as usize;

    // Get CPR field offsets from the cpr_class_hint (if we have one)
    // or from runtime_value_class itself (if fields bleed past Record
    // boundary into CPR territory).
    let (g, t, e, c) = find_cpr_offsets(cpr_class_hint)
        .or_else(|| {
            // Fallback: hardcoded from macOS verification (2026-04-11)
            Some((0x10usize, 0x20usize, 0x50usize, 0x70usize))
        })?;

    let adjust = |off: usize| -> usize {
        record_offset + off.saturating_sub(IL2CPP_OBJ_HEADER)
    };
    Some(CardFieldOffsets {
        grp_id: adjust(g),
        title_id: adjust(t),
        expansion_code: adjust(e),
        collector_number: adjust(c),
    })
}

/// Public entry point: read the full card database from Mono Arena.
pub fn read_mtga_card_database_mono(
    process_name: &str,
) -> Result<Vec<(i32, String, String, i32)>, String> {
    let pid = find_wine_pid(process_name)?;
    let reader = MonoReader::new(pid);
    let debug = std::env::var("MTGA_DEBUG_MONO").is_ok();

    // We need a CardPrintingRecord class pointer for field-offset
    // resolution. We can't do a __DATA scan on Mono (no TypeInfoTable),
    // so we use 0 as a hint and rely on the fallback path in
    // resolve_card_field_offsets (hardcoded offsets from macOS).
    let cpr_class_hint = 0usize;

    let (dict_addr, dict_count, runtime_value_class) =
        scan_heap_for_card_printing_dictionary(&reader, pid).ok_or_else(|| {
            "Could not find card-printing dictionary in Arena's heap.".to_string()
        })?;
    let runtime_name = read_mono_class_name(&reader, runtime_value_class);
    if debug {
        eprintln!(
            "mono::read_mtga_card_database: dict=0x{:x} count={} value_class=0x{:x} ({:?})",
            dict_addr, dict_count, runtime_value_class, runtime_name,
        );
    }

    // If value class is CardPrintingData, try to resolve field offsets
    // from its own metadata; fall back to hardcoded CPR offsets.
    let offsets = resolve_card_field_offsets(&reader, runtime_value_class, cpr_class_hint)
        .ok_or_else(|| {
            "Could not resolve card field offsets on Mono runtime class.".to_string()
        })?;
    if debug {
        eprintln!(
            "mono::read_mtga_card_database: offsets grp_id=0x{:x} title_id=0x{:x} expansion=0x{:x} collector=0x{:x}",
            offsets.grp_id, offsets.title_id, offsets.expansion_code, offsets.collector_number,
        );
    }

    let entries = read_card_printing_entries(&reader, dict_addr, runtime_value_class);
    if entries.is_empty() {
        return Err("Found printing dict but walked zero valid entries.".to_string());
    }

    let mut result = Vec::with_capacity(entries.len());
    for (grp_id, value_ptr) in &entries {
        let title_id = reader.read_i32(value_ptr + offsets.title_id);
        let set_ptr = reader.read_ptr(value_ptr + offsets.expansion_code);
        let num_ptr = reader.read_ptr(value_ptr + offsets.collector_number);
        let set = reader.read_mono_string(set_ptr).unwrap_or_default();
        let num = reader.read_mono_string(num_ptr).unwrap_or_default();
        result.push((*grp_id, set, num, title_id));
    }
    if debug {
        eprintln!("mono::read_mtga_card_database: produced {} rows", result.len());
    }
    Ok(result)
}

// ──────────────────────────────────────────────────────────────────
// readMtgaInventory — scan for ClientPlayerInventory
// ──────────────────────────────────────────────────────────────────

/// Field offsets for ClientPlayerInventory (C# level, same across IL2CPP and Mono).
#[derive(Debug, Clone)]
pub struct InventoryFieldOffsets {
    pub wc_common: usize,
    pub wc_uncommon: usize,
    pub wc_rare: usize,
    pub wc_mythic: usize,
    pub gold: usize,
    pub gems: usize,
    pub vault_progress: usize,
}

pub fn resolve_inventory_field_offsets(
    fields: &[MonoFieldInfo],
) -> Option<InventoryFieldOffsets> {
    let find = |candidates: &[&str]| -> Option<usize> {
        for name in candidates {
            if let Some(f) = fields.iter().find(|f| !f.is_static && f.name == *name) {
                return Some(f.offset as usize);
            }
        }
        None
    };
    Some(InventoryFieldOffsets {
        wc_common: find(&["wcCommon", "<wcCommon>k__BackingField"])?,
        wc_uncommon: find(&["wcUncommon", "<wcUncommon>k__BackingField"])?,
        wc_rare: find(&["wcRare", "<wcRare>k__BackingField"])?,
        wc_mythic: find(&["wcMythic", "<wcMythic>k__BackingField"])?,
        gold: find(&["gold", "<gold>k__BackingField"])?,
        gems: find(&["gems", "<gems>k__BackingField"])?,
        vault_progress: find(&["vaultProgress", "<vaultProgress>k__BackingField"])?,
    })
}

fn inventory_plausible(wc: i32, wu: i32, wr: i32, wm: i32, g: i32, ge: i32) -> bool {
    (0..=99_999).contains(&wc)
        && (0..=99_999).contains(&wu)
        && (0..=99_999).contains(&wr)
        && (0..=99_999).contains(&wm)
        && (0..=1_000_000_000).contains(&g)
        && (0..=10_000_000).contains(&ge)
        && (wc | wu | wr | wm | g | ge) != 0
}

fn inventory_score(wc: i32, wu: i32, wr: i32, wm: i32, g: i32, ge: i32) -> i64 {
    wc as i64 + wu as i64 + wr as i64 + wm as i64 + g as i64 + ge as i64
}

/// Scan heap for a ClientPlayerInventory instance.
/// Uses lazy class-name resolution: for each candidate that passes
/// the plausibility filter, resolve obj → vtable → class → name,
/// caching per unique class pointer.
pub fn scan_heap_for_client_player_inventory(
    reader: &MonoReader,
    pid: u32,
    offsets: &InventoryFieldOffsets,
) -> Option<usize> {
    let debug = std::env::var("MTGA_DEBUG_MONO").is_ok();
    let max_off = [
        offsets.wc_common,
        offsets.wc_uncommon,
        offsets.wc_rare,
        offsets.wc_mythic,
        offsets.gold,
        offsets.gems,
        offsets.vault_progress,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    let min_obj_size = max_off + 8; // vault is f64 = 8 bytes

    let heap_regions = find_scannable_heap_regions(pid);
    if debug {
        eprintln!(
            "mono::scan_heap_for_client_player_inventory: scanning {} regions (field span={} bytes)",
            heap_regions.len(),
            min_obj_size,
        );
    }

    // (obj_addr, class_ptr, score)
    let mut candidates: Vec<(usize, usize, i64)> = Vec::new();
    let mut class_cache: HashMap<usize, String> = HashMap::new();

    for (start, end) in heap_regions {
        let size = end - start;
        let buf = reader.read_bytes(start, size);
        if buf.len() != size {
            continue;
        }
        let mut i = 0usize;
        while i + min_obj_size <= buf.len() {
            let vtable = u64::from_le_bytes(
                buf[i..i + 8].try_into().unwrap_or([0; 8]),
            ) as usize;
            if vtable < MIN_PTR || vtable > MAX_PTR {
                i += 8;
                continue;
            }
            let read_i32_at = |off: usize| -> i32 {
                let s = i + off;
                if s + 4 > buf.len() { return i32::MIN; }
                i32::from_le_bytes(buf[s..s + 4].try_into().unwrap_or([0; 4]))
            };
            let wc = read_i32_at(offsets.wc_common);
            let wu = read_i32_at(offsets.wc_uncommon);
            let wr = read_i32_at(offsets.wc_rare);
            let wm = read_i32_at(offsets.wc_mythic);
            let g = read_i32_at(offsets.gold);
            let ge = read_i32_at(offsets.gems);
            if !inventory_plausible(wc, wu, wr, wm, g, ge) {
                i += 8;
                continue;
            }

            // Resolve class name via Mono vtable indirection
            let class_ptr = reader.read_ptr(vtable); // vtable → class
            if class_ptr < MIN_PTR || class_ptr > MAX_PTR {
                i += 8;
                continue;
            }
            let name = class_cache
                .entry(class_ptr)
                .or_insert_with(|| read_mono_class_name(reader, class_ptr))
                .clone();
            if name == "ClientPlayerInventory" {
                let score = inventory_score(wc, wu, wr, wm, g, ge);
                candidates.push((start + i, class_ptr, score));
            }
            i += 8;
        }
    }

    candidates.sort_by_key(|(_, _, s)| std::cmp::Reverse(*s));
    if debug {
        eprintln!(
            "mono::scan_heap_for_client_player_inventory: {} candidates matched",
            candidates.len(),
        );
        for (addr, cls, score) in candidates.iter().take(10) {
            eprintln!("  0x{:x} class=0x{:x} score={}", addr, cls, score);
        }
    }
    candidates.first().map(|(a, _, _)| *a)
}

/// Public entry point: read inventory from Mono Arena.
pub fn read_mtga_inventory_mono(
    process_name: &str,
) -> Result<(i32, i32, i32, i32, i32, i32, f64), String> {
    let pid = find_wine_pid(process_name)?;
    let reader = MonoReader::new(pid);
    let debug = std::env::var("MTGA_DEBUG_MONO").is_ok();

    // Resolve field offsets. We need to find a ClientPlayerInventory class
    // to read its field list. We'll discover it during the scan — for now,
    // use hardcoded offsets from macOS verification as the starting point
    // (C# offsets are identical across IL2CPP and Mono).
    let offsets = InventoryFieldOffsets {
        wc_common: 0x10,
        wc_uncommon: 0x14,
        wc_rare: 0x18,
        wc_mythic: 0x1c,
        gold: 0x20,
        gems: 0x24,
        vault_progress: 0x30,
    };

    let inst = scan_heap_for_client_player_inventory(&reader, pid, &offsets).ok_or_else(|| {
        "ClientPlayerInventory not found in Mono heap. Either the player \
         is not logged in or the field offsets have drifted."
            .to_string()
    })?;

    let wc_common = reader.read_i32(inst + offsets.wc_common);
    let wc_uncommon = reader.read_i32(inst + offsets.wc_uncommon);
    let wc_rare = reader.read_i32(inst + offsets.wc_rare);
    let wc_mythic = reader.read_i32(inst + offsets.wc_mythic);
    let gold = reader.read_i32(inst + offsets.gold);
    let gems = reader.read_i32(inst + offsets.gems);
    let vault_progress = reader.read_f64(inst + offsets.vault_progress);

    if debug {
        eprintln!(
            "mono::read_mtga_inventory: inst=0x{:x} wc={{C:{}, U:{}, R:{}, M:{}}} gold={} gems={} vault={}",
            inst, wc_common, wc_uncommon, wc_rare, wc_mythic, gold, gems, vault_progress,
        );
    }
    Ok((wc_common, wc_uncommon, wc_rare, wc_mythic, gold, gems, vault_progress))
}

// ──────────────────────────────────────────────────────────────────
// Utility
// ──────────────────────────────────────────────────────────────────

/// Find the Wine Arena process PID. Uses `pgrep -f` with the Wine
/// process path pattern. Falls back to sysinfo.
fn find_wine_pid(process_name: &str) -> Result<u32, String> {
    // Try pgrep first (works on macOS for Wine processes)
    if let Ok(output) = Command::new("pgrep").arg("-f").arg(process_name).output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(pid) = stdout.trim().lines().next().and_then(|s| s.parse::<u32>().ok()) {
            return Ok(pid);
        }
    }

    // Fallback: sysinfo (0.30.x — methods are inherent on System, no trait imports needed)
    use sysinfo::System;
    let mut sys = System::new_all();
    sys.refresh_all();
    for (pid, process) in sys.processes() {
        let name = process.name();
        if name.contains(process_name) {
            return Ok(pid.as_u32());
        }
    }

    Err(format!("Process '{}' not found", process_name))
}
