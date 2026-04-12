# Windows port plan for `readMtgaCards` / `readMtgaCardDatabase` / `readMtgaInventory`

**Status**: not started. This document is the plan-of-record produced
after cross-platform investigation on 2026-04-11.

## Premise verified

- macOS Arena (`/Users/Shared/Epic Games/MagicTheGathering/MTGA.app`)
  ships `GameAssembly.dylib` → **IL2CPP**.
- Windows Arena (confirmed via live CrossOver bottle install 2026-04-11)
  ships `MonoBleedingEdge/EmbedRuntime/mono-2.0-bdwgc.dll` and has **no**
  `GameAssembly.dll` → **Mono**.
- Both platforms are built from **Unity 2022.3.62f2 (build
  `7670c08855a9`)**, same C# source, same date (`2026-03-18`). WotC
  compiles each platform independently, selecting the scripting backend
  per target.
- PDB path inside `mono-2.0-bdwgc.dll`:
  `C:\build\output\Unity-Technologies\mono\msvc\build\boehm\x64\bin\Release\mono-2.0-bdwgc.pdb`
  — i.e. the `Unity-Technologies/mono` fork, Boehm GC, MSVC x64.
- PDB path inside `UnityPlayer.dll`:
  `C:\build\output\unity\unity\artifacts\UnityPlayer\Win64_VS2019_nondev_m_m\UnityPlayer_Win64_player_mono_x64.pdb`
  — filename literally contains `player_mono_x64`, Unity itself
  confirms this binary is the Mono variant.

## The key insight: C# object field offsets are identical across backends

Both IL2CPP and Mono respect the same C# `[StructLayout]` rules, the
same field ordering, and the same padding. Both use a 16-byte object
header (`klass + monitor` for IL2CPP, `vtable + sync_block` for Mono —
same size). So **everything we learned about C# object layouts on
macOS transfers byte-for-byte to Windows**:

| Verified on macOS IL2CPP | Expected on Windows Mono |
|---|---|
| `CardPrintingData.Record @ 0xC0` (inline struct) | same |
| `CardPrintingRecord.GrpId @ 0x10` | same |
| `CardPrintingRecord.TitleId @ 0x20` | same |
| `CardPrintingRecord.ExpansionCode @ 0x50` (string ptr) | same |
| `CardPrintingRecord.CollectorNumber @ 0x70` (string ptr) | same |
| `ClientPlayerInventory.wcCommon @ 0x10` | same |
| `ClientPlayerInventory.wcUncommon @ 0x14` | same |
| `ClientPlayerInventory.wcRare @ 0x18` | same |
| `ClientPlayerInventory.wcMythic @ 0x1c` | same |
| `ClientPlayerInventory.gold @ 0x20` | same |
| `ClientPlayerInventory.gems @ 0x24` | same |
| `ClientPlayerInventory.vaultProgress @ 0x30` (f64) | same |
| `CardsAndQuantity` entries stride 16 (int→int) | same |
| Card-printing dict entries stride 24 (int→ptr) | same |

### What actually differs between backends

1. **`obj → klass_ptr` requires one extra dereference on Mono.**
   IL2CPP instances have the class pointer at `obj[+0x00]`. Mono
   instances have a `MonoVTable*` at `obj[+0x00]`, which then points
   to the `MonoClass*`. So:
   ```
   IL2CPP:  class = read_ptr(obj)
   Mono:    class = read_ptr(read_ptr(obj))
   ```

2. **Runtime metadata struct layouts are completely different.**
   `Il2CppClass` and `MonoClass` share no field offsets. But upstream
   already has working abstractions for both:
   - `src/il2cpp/offsets.rs::Il2CppOffsets::unity_2022_3()` — verified
   - `src/mono/offsets.rs::MonoOffsets::unity_2022_3()` — currently a
     delegation to `unity_2021_3` with a TODO comment; needs live
     verification against MTGA Windows but probably correct since both
     LTS branches descend from the same Unity-Mono fork

3. **Class lookup by name uses different storage.**
   - IL2CPP: `s_TypeInfoTable` in `__DATA` (flat array of
     `Il2CppClass*`). Our macOS scanner iterates `__DATA` segments
     looking for valid class pointers.
   - Mono: `MonoDomain::domain_assemblies` → per-image
     `MonoImage::class_cache` hash table. Upstream's
     `MonoReader::read_mono_root_domain()` +
     `create_type_definitions_for_image()` already walks this.

4. **Managed string layout is coincidentally the same.**
   Both IL2CPP's `Il2CppString` and Mono's `MonoString` on 64-bit are:
   ```
   +0x00  klass/vtable ptr (8)
   +0x08  monitor/sync_block (8)
   +0x10  length (i32)
   +0x14  chars[] (UTF-16, `length` code units)
   ```
   Our `read_il2cpp_string` helper should work unchanged on Mono
   strings — same data, same offsets — but needs to be renamed to
   `read_managed_string` to avoid confusing future readers, and the
   existing upstream `Managed::read_string` in `src/managed.rs` is
   also usable (has some quirks, see below).

5. **Region enumeration (GC heap and module __DATA).** macOS parses
   `vmmap` output. Windows needs `VirtualQueryEx` and
   `EnumProcessModules`. Pure platform porting, runtime-agnostic.

## Scope boundary

**In-scope**: Windows port of the three existing napi functions
(`readMtgaCards`, `readMtgaCardDatabase`, `readMtgaInventory`) such
that they return the same shape of result on Windows as on macOS.

**Out-of-scope**:
- Linux native. Arena has no Linux build. Wine/Proton users run the
  Windows binary, which our Windows scanner will read unchanged
  (inside the Wine container on macOS, we already proved the scanner
  can see the process).
- Fixing upstream's existing Windows `readData` / `get_instance` /
  `get_dictionary` napi surface. Those target a different use case
  (generic introspection) and have their own bugs. Leave them alone.
- Reinstating a functional PAPA walker. We bypassed it on macOS via
  heap signature scans; we'll do the same on Windows. The upstream
  Mono walker in `src/mono_reader.rs` + `src/napi/mod.rs::windows_backend`
  may or may not still work — don't depend on it.

## Implementation order

Each stage is independently testable against the live CrossOver Arena
process at `~/Library/Application Support/CrossOver/Bottles/Magic
The Gathering Arena/...`.

### Stage 0 — Scaffolding (no behavior change)

1. Add a new module `src/mono_scanner.rs` or
   `src/mono/scanner.rs` (mirror the existing `src/mono/` layout).
   This will hold the Mono equivalents of our IL2CPP scanners.
2. Gate the Windows napi exports on `#[cfg(target_os = "windows")]`
   at the `#[napi] pub fn read_mtga_*` wrappers in
   `src/napi/mod.rs`. Currently these have only a macOS branch.
3. Add a `#[cfg(target_os = "windows")] mod windows_mtga;` section
   inside the existing `windows_backend` module (or as a sibling)
   that re-exports the scanner implementations under the
   `read_mtga_*_impl` names the napi wrappers expect.

### Stage 1 — `find_scannable_heap_regions(pid)` on Windows

Runtime-agnostic. `VirtualQueryEx` loop walking the VM space:

```rust
// src/mono/scanner.rs (or wherever)
#[cfg(target_os = "windows")]
fn find_scannable_heap_regions(pid: u32) -> Vec<(usize, usize)> {
    use winapi::um::memoryapi::VirtualQueryEx;
    use winapi::um::winnt::{
        MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION,
        PAGE_READWRITE, PAGE_EXECUTE_READWRITE,
    };
    const MIN_SIZE: usize = 1 << 20; // 1 MB
    const MAX_SIZE: usize = 4usize << 30; // 4 GB
    // ... open process with PROCESS_QUERY_INFORMATION | PROCESS_VM_READ
    // ... loop: VirtualQueryEx(handle, addr, &mbi, ...) until addr overflows
    // ... filter: mbi.State == MEM_COMMIT
    //          && mbi.Type == MEM_PRIVATE
    //          && mbi.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) != 0
    //          && MIN_SIZE <= mbi.RegionSize <= MAX_SIZE
    //          && !inside_module_region(mbi.BaseAddress, "mono-2.0-bdwgc.dll" | "UnityPlayer.dll")
    // ... advance addr by mbi.RegionSize
}
```

**`inside_module_region`** is computed once at startup via
`EnumProcessModules` + `GetModuleInformation` + `GetModuleFileNameExW`,
caching `(base, size)` for `mono-2.0-bdwgc.dll`, `UnityPlayer.dll`,
and `Assembly-CSharp.dll` so we can exclude their own data segments
from the GC heap scan.

**Test**: run against the CrossOver bottle's `MTGA.exe`; should
return dozens of regions, none overlapping the Arena module bases.
Total bytes ≈ a few GB.

### Stage 2 — `MonoMemReader` wrapper for cross-platform use

Upstream already has `MonoReader` in `src/mono_reader.rs` with
`read_u8/u32/u64/ptr/bytes/ascii_string` etc., backed by the
`process_memory` crate (works on Windows and Linux). Use it directly
— no new reader type needed.

Our macOS scanner uses a bespoke `MemReader` struct with a
`task_port` field; the Mono scanner will use `&MonoReader` in its
place. The scanner functions take it as `&MonoReader` and call its
primitives — clean.

### Stage 3 — `read_managed_string` for Mono

Upstream's `Managed::read_string()` in `src/managed.rs:61` has a
known bug: it reads byte-by-byte instead of every 2 bytes for UTF-16
code units (the loop variable `i` isn't multiplied by 2), producing
garbled output for strings longer than 1 char. **Don't use it
as-is.** Instead, port `read_il2cpp_string` from
`src/napi/mod.rs::macos_backend` to take `&MonoReader` instead of
`&MemReader`:

```rust
// src/mono/scanner.rs
fn read_managed_string(reader: &MonoReader, ptr: usize) -> Option<String> {
    const MAX_LEN: i32 = 1024;
    if ptr < 0x100000 { return None; }
    let length = reader.read_i32(ptr + 0x10);
    if length < 0 || length > MAX_LEN { return None; }
    if length == 0 { return Some(String::new()); }
    let bytes = reader.read_bytes(ptr + 0x14, length as usize * 2);
    if bytes.len() < length as usize * 2 { return None; }
    let chars: Vec<u16> = (0..length as usize)
        .map(|i| u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]))
        .collect();
    String::from_utf16(&chars).ok()
}
```

Same byte layout as `Il2CppString`, just typed against `&MonoReader`.

### Stage 4 — `readMtgaCards` on Windows

Smallest helper surface of the three. Needed pieces:
- `find_scannable_heap_regions(pid)` — Stage 1 ✓
- `scan_heap_for_cards_dictionary(reader, pid)` — copy from
  `macos_backend` verbatim, swap `&MemReader` → `&MonoReader`. The
  logic is C#-level (Dictionary<int,int> object layout with
  `hash==key` invariant, Arena-range keys, `[1,4]` values), no
  runtime-specific struct dereferences.
- `read_cards_dictionary_entries(reader, dict_addr)` — same, pure
  copy.

Wire into `read_mtga_cards_impl` under
`#[cfg(target_os = "windows")]`. Total ~200 lines copied from
`macos_backend` with minor adjustments.

**Verification**:
1. Start the CrossOver MTGA.exe process.
2. Run macOS `sudo node try.js` against both processes in sequence.
3. Compare collection dicts — they should return the same set of
   `(cardId, quantity)` tuples because the same account is logged
   in on both (cloud-synced).
4. Expected: 4276 unique printings, 8975 total copies (from earlier
   macOS verification), identical on Windows.

### Stage 5 — `readMtgaCardDatabase` on Windows

More dependencies. Needed pieces:
- `find_class_by_direct_scan(name)` — Mono equivalent. On macOS
  this scans `__DATA` segments for `Il2CppClass*` values and
  dereferences each one to check its name. Mono doesn't have
  `__DATA`-stored class pointers the same way; instead, walk
  domain → assemblies → class_cache using upstream's
  `MonoReader::read_mono_root_domain()` +
  `create_type_definitions_for_image()`.
- `find_all_classes_by_name(name)` — same, returning all matches.
  `MonoReader::get_all_assembly_names()` lists assemblies; iterate
  each assembly's images; iterate each image's class cache; filter
  by name.
- `find_classes_by_name_substr(sub)` — same structure, looser
  match.
- `get_class_fields(class_addr)` — upstream has
  `TypeDefinition::get_fields()` which returns field addresses on
  Mono; wrap the result in our `FieldInfo` struct (our existing
  napi type). Note: upstream's `TypeDefinition` is the Mono
  version, not to be confused with `il2cpp::type_definition`.
- `read_class_name(class)` — Mono reads name at
  `MonoClass + offsets.type_def_name` (0x48 per
  `mono/offsets.rs::unity_2022_3`), dereferences for the string
  pointer, reads ASCII.
- `read_managed_string` — Stage 3 ✓
- `scan_heap_for_card_printing_dictionary(reader, pid, cpr_hint)` —
  copy from `macos_backend`, adapt the class-name verification
  step. On macOS we do `read_class_name(read_ptr(value_ptr))`; on
  Mono we do `read_class_name(read_ptr(read_ptr(value_ptr)))`.
- `resolve_runtime_card_field_offsets` — copy verbatim. The
  effective-offset computation (`record_offset + cpr_field_offset
  - 0x10`) is C#-layout-based and transfers unchanged.
- `read_card_printing_entries(reader, dict_addr, value_class)` —
  copy verbatim. Stride-24 walk with value-pointer class
  verification.
- `read_mtga_card_database_impl(process_name)` — top-level
  function. Copy the macOS version, swap calls to use Mono
  variants.

**New**: Stage 5 needs a helper that iterates all
`MonoClass*` pointers for a given name by walking every image's
`class_cache`. Prototype signature:

```rust
fn enumerate_mono_classes(reader: &mut MonoReader) -> Vec<(usize, String)> {
    let domain = reader.read_mono_root_domain();
    let assemblies = reader.get_all_assembly_names();
    let mut out = Vec::new();
    for asm_name in &assemblies {
        let image = reader.read_assembly_image_by_name(asm_name);
        if image == 0 { continue; }
        for def_addr in reader.create_type_definitions_for_image(image) {
            let td = TypeDefinition::new(def_addr, reader);
            out.push((def_addr, td.name));
        }
    }
    out
}
```

Cache the result — walking the class cache is expensive and the
class list doesn't change within a process lifetime.

**Verification**:
1. Run `readMtgaCardDatabase` against the Windows process.
2. Compare row count and first 20 rows against macOS output.
3. Ground truth: 23,694 rows, 0 grp_id mismatches, 0 failed string
   reads on macOS. Windows should match within rounding (some
   rows can flap if the scan happens while Arena is loading new
   cards, but steady-state should be identical).

### Stage 6 — `readMtgaInventory` on Windows

Easiest after Stage 5's helpers exist. Needed pieces:
- `find_all_classes_by_name` — Stage 5 ✓
- `get_class_fields` — Stage 5 ✓
- `resolve_inventory_field_offsets` — copy verbatim (C#-source-level
  field name matching)
- `inventory_fields_look_plausible` — copy verbatim
- `inventory_activity_score` — copy verbatim
- `scan_heap_for_client_player_inventory(reader, pid, offsets, classes)`
  — copy from `macos_backend`, add the extra `read_ptr` for
  `obj → vtable → class` when filtering by class-pointer set.
- `count_pointer_occurrences_in_heap` — diagnostic helper, copy
  verbatim.
- `find_classes_by_name_substr` — diagnostic helper, Stage 5 ✓

**Critical subtlety**: the class-pointer-set pre-filter on macOS
compares `obj[+0]` against a set of `Il2CppClass*` addresses. On
Mono, `obj[+0]` is a `MonoVTable*`, not a `MonoClass*`. Two
options:
  - (a) Collect the set of *MonoVTable* addresses whose `->klass`
    points to a `ClientPlayerInventory`-named `MonoClass`. One
    class can have multiple vtables (one per domain), so the set
    may have multiple elements.
  - (b) Change the scanner to compare `read_ptr(read_ptr(obj))`
    (the resolved `MonoClass*`) against the set of class addresses.
    Twice as many pointer reads per candidate but easier to reason
    about.

Recommend **(a)** for performance — pre-computing the vtable set
is cheap and keeps the inner loop to a single `read_ptr` per slot.
Build the set during the class-enumeration pass in Stage 5.

**Verification**: ground-truth from macOS is `{wcCommon:37,
wcUncommon:11, wcRare:1, wcMythic:1, gold:825, gems:610,
vaultProgress:58.9}`. Windows must return **exactly these values**
because the account is the same. Any divergence is a layout bug.

### Stage 7 — Privilege handling

Windows requires `SeDebugPrivilege` to `OpenProcess` with
`PROCESS_VM_READ` on another user's process. Two paths:
- **User runs as Administrator**: no code change needed.
  `is_elevated::is_elevated()` (already a dep) can detect and
  return a useful error when not elevated.
- **Programmatic privilege grant**: call
  `AdjustTokenPrivileges(GetCurrentProcessToken(), FALSE, ...)` with
  `SE_DEBUG_NAME` lookup via `LookupPrivilegeValueW`. ~30 lines
  using winapi. Fails gracefully if the user has no debug
  privilege to grant.

On same-user processes (user's own Arena), `PROCESS_VM_READ |
PROCESS_QUERY_INFORMATION` works without debug privilege. MTGA
inside a Wine bottle counts as same-user on the host macOS, so
our `readMtga*` functions can already read it with just
`task_for_pid` — no Windows-side privilege code needed for the
Wine use case. Real-Windows privilege handling is only needed for
native Windows users.

### Stage 8 — Live verification on Wine Arena

Unique to this project: we can run the Windows port inside a Wine
process on macOS, which means **we can develop and test the
Windows Mono scanner without leaving macOS**:

1. The Wine Arena process (currently PID 76678) is a real Windows
   process from the scanner's perspective — it has PE modules,
   `mono-2.0-bdwgc.dll`, a Mono root domain, etc.
2. Accessing it requires `task_for_pid` (mach), not
   `OpenProcess` (winapi), because it's a Mach process hosting a
   Wine runtime.
3. **Compromise approach**: build the Mono scanner against the
   `&MonoReader` abstraction, but implement `MonoReader::new(pid)`
   with a conditional: if `target_os="macos"`, use `mach2` like
   our existing macos_backend; if `target_os="windows"`, use
   `process_memory` like upstream. Same trait, two readers.
4. This means the Mono scanner development can happen entirely on
   macOS, reading the Wine-hosted Arena process via mach. Only the
   final cross-compile for Windows targets needs the winapi
   implementation.

**This is the fastest dev loop**: we already know `task_for_pid`
works on the Wine process; we have a known-good scanner codebase
for macOS; we just need to port the Mono-specific bits and verify
against the same live process.

### Stage 9 — Testing matrix

| Test | Target | Source of truth |
|---|---|---|
| `readMtgaCards` collection parity | Wine Arena | macOS IL2CPP output |
| `readMtgaCardDatabase` row count & first 20 rows | Wine Arena | macOS IL2CPP output |
| `readMtgaInventory` wildcards/gold/gems/vault | Wine Arena | macOS IL2CPP output + Arena UI |
| Native Windows (future) | real Windows Arena | previously recorded Wine run |

## Deliverables checklist

- [ ] `src/mono/scanner.rs` new module with all Mono-specific scanner
      helpers (~500 lines)
- [ ] `src/mono/scanner.rs::find_scannable_heap_regions` for both
      macOS-hosted Wine and native Windows
- [ ] `src/mono/scanner.rs::enumerate_mono_classes` with caching
- [ ] `src/mono/scanner.rs::read_managed_string` (Mono/IL2CPP-compatible)
- [ ] `src/mono/scanner.rs::scan_heap_for_cards_dictionary`
- [ ] `src/mono/scanner.rs::scan_heap_for_card_printing_dictionary`
- [ ] `src/mono/scanner.rs::scan_heap_for_client_player_inventory`
- [ ] `src/mono/scanner.rs::read_mtga_cards_impl` — Windows entry point
- [ ] `src/mono/scanner.rs::read_mtga_card_database_impl` — Windows entry point
- [ ] `src/mono/scanner.rs::read_mtga_inventory_impl` — Windows entry point
- [ ] `src/napi/mod.rs` — extend the three `#[napi]` wrappers to
      dispatch to `mono::scanner::read_mtga_*_impl` under
      `#[cfg(target_os = "windows")]`
- [ ] `src/mono/offsets.rs` — audit `MonoOffsets::unity_2022_3()` against
      live Wine Arena; replace the `Self::unity_2021_3()` delegation with
      verified values if they differ
- [ ] `try_wine_cards.js`, `try_wine_card_db.js`, `try_wine_inventory.js`
      — node smoke tests against the Wine process
- [ ] Update NOTES.md with a "Windows port" section documenting what
      worked and any layout surprises encountered

## Risk register

1. **Mono class struct offsets on 2022.3.62f2 have drifted from
   2021.3**. Upstream's `MonoOffsets::unity_2022_3()` is currently a
   delegation stub; if the real layout differs, Stage 5's class
   enumeration will return garbage names. **Mitigation**: during
   Stage 0, write a `mono_offset_probe` binary that dumps a few
   known classes (e.g. `System.String`, `System.Int32`) from Wine
   Arena and verifies the offsets empirically. Takes 30 minutes,
   catches 90% of drift bugs.

2. **`MonoReader::read_mono_root_domain()` may be broken on current
   MTGA builds** the same way the IL2CPP PAPA walker was broken on
   macOS. The upstream Mono walker hasn't been tested on
   2022.3.62f2 either. **Mitigation**: have a fallback path that
   heap-scans for `MonoClass` structures directly by pattern
   (similar to how we heap-scanned for `Il2CppClass` on macOS). If
   `read_mono_root_domain()` returns 0 or garbage, fall back to a
   signature scan of the process's writable regions for structures
   that look like `MonoClass` (check `name` field deref for ASCII
   validity, check `image` field deref for a plausible
   `MonoImage`, etc.).

3. **Wine's memory layout may not match native Windows exactly**.
   Wine emulates the Windows API but allocates memory through the
   host kernel. GC heap regions, module base addresses, and
   `MonoDomain::class_cache` layout should all be identical to
   native Windows because they're constructed by the Unity Mono
   fork running inside the Wine container — Wine just hosts the
   PE loader and the Win32 API. But: `VirtualQueryEx` inside Wine
   returns Wine's emulated memory map, which may include regions
   that don't exist on native Windows. **Mitigation**: when
   eventually testing on native Windows, expect minor region-list
   differences; the scanner's size filtering and class-set
   pre-filter should handle them.

4. **The Wine Arena process runs as x86_64 PE inside a Mach task**,
   which means pointer conventions and struct alignment match
   Windows x64 (not macOS arm64). Our current macOS scanner
   assumes arm64 pointer ranges `[0x1_0000_0000, 0x4_0000_0000]`.
   Wine Arena's pointers are in different ranges. **Mitigation**:
   widen the pointer-plausibility bounds in the Mono scanner to
   cover typical Windows x64 userspace (`[0x10000, 0x7FFFFFFEFFFF]`
   or similar). Or just accept anything non-null in the low half
   of the 64-bit space.

5. **CrossOver may eventually update or break the bottle**. The
   current bottle was verified 2026-04-11 against Arena
   `0.1.11790.1252588`. Future Arena updates may land automatically
   when the user next runs the launcher. **Mitigation**: snapshot
   the current bottle directory to a backup location so we can
   re-test against a known-good state even if the live bottle
   updates.

## Out-of-band: things to check opportunistically

- **Does upstream's existing `windows_backend::read_data_impl` still
  work** on Arena 2022.3.62f2? If so, its `PAPA → InventoryManager
  → Cards` walker could be a quick `readMtgaCards` equivalent
  without needing any new signature scans. Worth a 10-minute test
  before committing to Stages 4–6.
- **`MonoPosixHelper.dll`** is 597 KB and present in the bottle.
  It's a Mono support library for POSIX ops; we don't need it, but
  its existence confirms this is a "real" Unity Mono ship, not a
  lightweight variant.
- **`MTGA_Data/Plugins/x86_64/` contents** include Wwise, Backtrace,
  EOS SDK, Steam SDK, Firebase, Discord SDK, SQLite, Burst-generated
  code, and DirectX shader compilers. None affect the Mono
  scanner, but good to know for understanding the process's native
  module list when filtering heap regions.
- **The `version` file at `MTGA/version`** has
  `CurrentInstallerURL` pointing at
  `https://mtgarena.downloads.wizards.com/Live/Windows64/versions/11790.1252588/MTGAInstaller_0.1.11790.1252588.msi`.
  If we ever need to pull a specific Windows Arena MSI for offset
  verification without running the game, we can fetch from
  that CDN directly.
