//! GPU-busy% and disk-busy% via Windows PDH (Performance Data Helper), picked over `nvml-wrapper`
//! for the same vendor-neutrality reason ADR-0054 picked DXGI over NVML for VRAM (see this
//! module's parent doc comment) -- PDH's `\GPU Engine(*)\Utilization Percentage` and
//! `\PhysicalDisk(*)\% Idle Time` counters are populated by any WDDM/Storport driver, not just
//! NVIDIA's. See `docs/adr/0070-bottleneck-telemetry.md` for the full decision record.
//!
//! The engine-instance name parsing and per-adapter aggregation below are plain string/arithmetic
//! logic with no Windows dependency, so they're always compiled and tested, including on this
//! (Linux/WSL) sandbox. Only [`PdhLoadSource`] itself -- the part that actually calls into
//! `pdh.dll` -- is `cfg(windows)`.

/// Identifies one GPU Engine counter instance's `(phys, eng)` pair -- the pieces PDH's own naming
/// scheme uses to distinguish one physical adapter's one engine (3D, Copy, VideoDecode, ...) from
/// another, independent of which process is driving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EngineKey {
    pub phys: u32,
    pub eng: u32,
}

/// One parsed `\GPU Engine(*)` instance name, e.g.
/// `pid_1234_luid_0x00000000_0x0000C6BB_phys_0_eng_0_engtype_3D`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInstance {
    /// The adapter LUID, combined via [`combine_luid`] -- compared against the LUID of the same
    /// DXGI adapter `default_vram_source()` reports VRAM for, so a multi-GPU machine's integrated
    /// GPU doesn't add noise to the dedicated adapter's busy% (or vice versa).
    pub luid: u64,
    pub key: EngineKey,
    pub engtype: String,
}

/// Combines a `LUID`'s `HighPart`/`LowPart` into one comparable value, the same way on both the
/// DXGI side (`IDXGIAdapter1::GetDesc1`) and the PDH instance-name side (this module), so the two
/// LUIDs can be compared for equality without caring about `LUID`'s own two-field shape.
pub fn combine_luid(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

/// Reads a run of ASCII digits from the start of `s`, returning the parsed value and the rest of
/// the string. `None` if `s` doesn't start with at least one digit.
fn split_once_uint(s: &str) -> Option<(u32, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let (digits, rest) = s.split_at(end);
    digits.parse::<u32>().ok().map(|n| (n, rest))
}

/// Reads a `0x`-prefixed run of hex digits from the start of `s`, returning the parsed value and
/// the rest of the string. `None` if `s` doesn't start with `0x` followed by at least one hex
/// digit.
fn split_hex(s: &str) -> Option<(u32, &str)> {
    let s = s.strip_prefix("0x")?;
    let end = s.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let (digits, rest) = s.split_at(end);
    u32::from_str_radix(digits, 16).ok().map(|n| (n, rest))
}

/// Parses a `\GPU Engine(*)` instance name into its `pid`/`luid`/`phys`/`eng`/`engtype` parts.
/// `None` for anything that doesn't match the documented shape -- a caller should skip (not
/// panic on) an instance it can't parse, since the exact set of fields is an internal Windows
/// convention, not a stable public contract.
pub fn parse_engine_instance(name: &str) -> Option<EngineInstance> {
    let rest = name.strip_prefix("pid_")?;
    let (_pid, rest) = split_once_uint(rest)?;
    let rest = rest.strip_prefix("_luid_")?;
    let (high, rest) = split_hex(rest)?;
    let rest = rest.strip_prefix('_')?;
    let (low, rest) = split_hex(rest)?;
    let rest = rest.strip_prefix("_phys_")?;
    let (phys, rest) = split_once_uint(rest)?;
    let rest = rest.strip_prefix("_eng_")?;
    let (eng, rest) = split_once_uint(rest)?;
    let engtype = rest.strip_prefix("_engtype_")?;
    if engtype.is_empty() {
        return None;
    }

    Some(EngineInstance {
        luid: combine_luid(high, low),
        key: EngineKey { phys, eng },
        engtype: engtype.to_string(),
    })
}

/// Aggregates per-process `\GPU Engine(*)\Utilization Percentage` readings into one busy%
/// (0-100) for `target_luid`'s adapter, matching how Task Manager's own GPU graph reads this
/// counter: readings for the same `(phys, eng)` pair are summed across processes (multiple
/// processes can drive the same engine concurrently), then the busiest engine on the target
/// adapter wins -- a batch saturating the 3D engine while Copy sits idle is still 100% GPU-bound,
/// not diluted by averaging across engines. Instances for any other adapter (e.g. an integrated
/// GPU) are ignored entirely. Returns `0.0` (not `None`) when `target_luid` has readings but none
/// exceed a rounding floor -- `None`/absence is the caller's job (an empty `readings` slice
/// implies the query itself found nothing, which `PdhLoadSource` treats as unavailable).
pub fn aggregate_engines(readings: &[(EngineInstance, f64)], target_luid: u64) -> f32 {
    use std::collections::HashMap;

    let mut sums: HashMap<EngineKey, f64> = HashMap::new();
    for (instance, value) in readings {
        if instance.luid != target_luid {
            continue;
        }
        *sums.entry(instance.key).or_insert(0.0) += value;
    }

    sums.values()
        .fold(0.0_f64, |max, &v| v.max(max))
        .clamp(0.0, 100.0) as f32
}

#[cfg(windows)]
mod windows_impl {
    //! The real PDH source. Unverified in this sandbox -- no PDH/DXGI adapter reachable under
    //! WSL. See `docs/adr/0070-bottleneck-telemetry.md`'s reference-machine checklist.

    use super::{aggregate_engines, combine_luid, parse_engine_instance};
    use crate::telemetry::{LoadReading, LoadSource};
    use windows::core::w;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W,
        PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    };

    /// Reads adapter 0's LUID via DXGI -- the same adapter `default_vram_source()`'s
    /// `DxgiVramSource` reports VRAM for (`EnumAdapters1(0)`), so GPU busy% and VRAM always
    /// describe the same physical GPU.
    fn query_adapter0_luid() -> Option<u64> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0).ok()?;
            let desc = adapter.GetDesc1().ok()?;
            Some(combine_luid(
                desc.AdapterLuid.HighPart as u32,
                desc.AdapterLuid.LowPart,
            ))
        }
    }

    /// Runs `PdhGetFormattedCounterArrayW`'s two-call buffer-sizing dance: the first call reports
    /// the required buffer size via `PDH_MORE_DATA`, the second fills it. Returns `(instance
    /// name, formatted double value)` pairs for every instance PDH reports, skipping any whose
    /// `CStatus` isn't valid/new data (e.g. an engine with no activity this interval).
    ///
    /// # Safety
    /// `counter` must be a valid handle added to a query that has already had
    /// `PdhCollectQueryData` called on it at least once.
    unsafe fn collect_formatted_array(counter: PDH_HCOUNTER) -> Option<Vec<(String, f64)>> {
        let mut buffer_size: u32 = 0;
        let mut item_count: u32 = 0;
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut buffer_size,
                &mut item_count,
                None,
            )
        };
        if status != PDH_MORE_DATA || buffer_size == 0 {
            return None;
        }

        // `PDH_FMT_COUNTERVALUE_ITEM_W` contains a pointer field (`szName`) and a union with a
        // pointer variant, so it needs pointer alignment (8 on x86_64) -- a `Vec<u8>` only
        // guarantees 1-byte alignment, so casting *that* buffer's pointer to this struct type and
        // reading through it would be misaligned-pointer UB (found by adversarial review: it
        // happened to work in testing because the allocator over-aligns most allocations this
        // size, but nothing guarantees that). `Vec<u64>` guarantees 8-byte alignment instead, so
        // this rounds the byte count up to a whole number of `u64` words -- the buffer holds the
        // fixed-size item array *and* PDH's own trailing variable-length string storage in one
        // contiguous region, per `PdhGetFormattedCounterArrayW`'s documented shape, so it must
        // stay a raw word buffer rather than becoming a typed `Vec<PDH_FMT_COUNTERVALUE_ITEM_W>`.
        let word_count = (buffer_size as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buffer: Vec<u64> = vec![0; word_count];
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut buffer_size,
                &mut item_count,
                Some(buffer.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W),
            )
        };
        if status != 0 {
            return None;
        }

        let items = unsafe {
            std::slice::from_raw_parts(
                buffer.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
                item_count as usize,
            )
        };

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if item.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA
                && item.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA
            {
                continue;
            }
            if item.szName.is_null() {
                continue;
            }
            let Ok(name) = (unsafe { item.szName.to_string() }) else {
                continue;
            };
            let value = unsafe { item.FmtValue.Anonymous.doubleValue };
            out.push((name, value));
        }
        Some(out)
    }

    pub struct PdhLoadSource {
        query: PDH_HQUERY,
        gpu_counter: PDH_HCOUNTER,
        disk_counter: PDH_HCOUNTER,
        target_luid: u64,
    }

    // Safety: `PdhLoadSource` is only ever driven from a single thread at a time -- never two
    // threads calling into it concurrently -- which is the "ownership transfer only" contract
    // `Send` promises. Worth being honest about what this does and doesn't rule out (flagged by
    // adversarial review): `new()` (`PdhOpenQueryW`/`PdhAddEnglishCounterW`) runs wherever
    // `default_load_source()` is called from -- today that's the UI thread, in
    // `nicti-pelt::app.rs` -- and the resulting value is then moved once into
    // `TelemetrySampler`'s background thread, where every later `query()` call
    // (`PdhCollectQueryData`/`PdhGetFormattedCounterArrayW`) runs. So the query handle is created
    // on one thread and used from a different one, sequentially, never concurrently. PDH is a
    // flat Win32 handle API (no COM apartment model), and nothing in its documentation ties a
    // query handle to its creating thread the way some older Win32 subsystems do -- but that's an
    // absence-of-evidence argument, not a confirmed one, and this exact pattern is unverified
    // against real hardware. See `docs/adr/0070-bottleneck-telemetry.md`'s reference-machine
    // checklist.
    unsafe impl Send for PdhLoadSource {}

    impl PdhLoadSource {
        /// Opens one PDH query with both wildcard counters. Returns `None` if PDH open/add fails
        /// (e.g. those counters don't exist on this Windows build) or no DXGI adapter is
        /// enumerable -- the caller (`default_load_source`) falls back to
        /// [`super::super::UnavailableLoadSource`].
        pub fn new() -> Option<Self> {
            let target_luid = query_adapter0_luid()?;

            unsafe {
                let mut query = PDH_HQUERY::default();
                if PdhOpenQueryW(windows::core::PCWSTR::null(), 0, &mut query) != 0 {
                    return None;
                }

                let mut gpu_counter = PDH_HCOUNTER::default();
                if PdhAddEnglishCounterW(
                    query,
                    w!("\\GPU Engine(*)\\Utilization Percentage"),
                    0,
                    &mut gpu_counter,
                ) != 0
                {
                    let _ = PdhCloseQuery(query);
                    return None;
                }

                let mut disk_counter = PDH_HCOUNTER::default();
                if PdhAddEnglishCounterW(
                    query,
                    w!("\\PhysicalDisk(*)\\% Idle Time"),
                    0,
                    &mut disk_counter,
                ) != 0
                {
                    let _ = PdhCloseQuery(query);
                    return None;
                }

                Some(PdhLoadSource {
                    query,
                    gpu_counter,
                    disk_counter,
                    target_luid,
                })
            }
        }

        fn read_gpu_busy_percent(&self) -> Option<f32> {
            let readings = unsafe { collect_formatted_array(self.gpu_counter) }?;
            let parsed: Vec<(super::EngineInstance, f64)> = readings
                .into_iter()
                .filter_map(|(name, value)| parse_engine_instance(&name).map(|inst| (inst, value)))
                .collect();
            // Not just "did anything parse" -- PDH can report instances for another adapter
            // (e.g. an integrated GPU) with none for `self.target_luid` at all, in which case
            // `aggregate_engines`'s per-target sum would be empty and fold to a fabricated 0.0
            // rather than the honest "n/a" (caught by CodeRabbit's review of this PR).
            if !parsed.iter().any(|(inst, _)| inst.luid == self.target_luid) {
                return None;
            }
            Some(aggregate_engines(&parsed, self.target_luid))
        }

        /// Busiest physical disk (100 - idle%), excluding the `_Total` instance -- that average
        /// hides one saturated drive behind several idle ones. `None` if PDH reported no
        /// per-disk instances at all.
        fn read_disk_busy_percent(&self) -> Option<f32> {
            let readings = unsafe { collect_formatted_array(self.disk_counter) }?;
            let mut max_busy: Option<f32> = None;
            for (name, idle_percent) in readings {
                if name.eq_ignore_ascii_case("_Total") {
                    continue;
                }
                let busy = (100.0 - idle_percent).clamp(0.0, 100.0) as f32;
                max_busy = Some(max_busy.map_or(busy, |m: f32| m.max(busy)));
            }
            max_busy
        }
    }

    impl LoadSource for PdhLoadSource {
        fn query(&mut self) -> LoadReading {
            let collected = unsafe { PdhCollectQueryData(self.query) } == 0;
            if !collected {
                return LoadReading::default();
            }
            LoadReading {
                gpu_busy_percent: self.read_gpu_busy_percent(),
                disk_busy_percent: self.read_disk_busy_percent(),
            }
        }
    }

    impl Drop for PdhLoadSource {
        fn drop(&mut self) {
            unsafe {
                let _ = PdhCloseQuery(self.query);
            }
        }
    }
}

#[cfg(windows)]
pub use windows_impl::PdhLoadSource;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_engine_instance_valid_zero_values() {
        let input = "pid_4660_luid_0x00000000_0x0000c6bb_phys_0_eng_0_engtype_3D";
        let expected = EngineInstance {
            luid: combine_luid(0, 0x0000c6bb),
            key: EngineKey { phys: 0, eng: 0 },
            engtype: "3D".to_string(),
        };
        assert_eq!(parse_engine_instance(input), Some(expected));
    }

    #[test]
    fn test_parse_engine_instance_valid_nonzero_values() {
        let input = "pid_4661_luid_0x00000001_0x0000c6bc_phys_1_eng_1_engtype_VideoDecode";
        let expected = EngineInstance {
            luid: combine_luid(1, 0x0000c6bc),
            key: EngineKey { phys: 1, eng: 1 },
            engtype: "VideoDecode".to_string(),
        };
        assert_eq!(parse_engine_instance(input), Some(expected));
    }

    #[test]
    fn test_parse_engine_instance_empty_string() {
        assert_eq!(parse_engine_instance(""), None);
    }

    #[test]
    fn test_parse_engine_instance_missing_pid_prefix() {
        let input = "id_4660_luid_0x00000000_0x0000c6bb_phys_0_eng_0_engtype_3D";
        assert_eq!(parse_engine_instance(input), None);
    }

    #[test]
    fn test_parse_engine_instance_no_pid_digits() {
        let input = "pid_luid_0x00000000_0x0000c6bb_phys_0_eng_0_engtype_3D";
        assert_eq!(parse_engine_instance(input), None);
    }

    #[test]
    fn test_parse_engine_instance_missing_luid_segment() {
        let input = "pid_4660_luid_0x00000000_c6bb_phys_0_eng_0_engtype_3D";
        assert_eq!(parse_engine_instance(input), None);
    }

    #[test]
    fn test_parse_engine_instance_missing_hex_prefix() {
        let input = "pid_4660_luid_0x00000000_0000c6bb_phys_0_eng_0_engtype_3D";
        assert_eq!(parse_engine_instance(input), None);
    }

    #[test]
    fn test_parse_engine_instance_empty_engtype() {
        let input = "pid_4660_luid_0x00000000_0x0000c6bb_phys_0_eng_0_engtype_";
        assert_eq!(parse_engine_instance(input), None);
    }

    #[test]
    fn test_combine_luid_zero_values() {
        assert_eq!(combine_luid(0, 0), 0);
    }

    #[test]
    fn test_combine_luid_high_bits() {
        assert_eq!(combine_luid(1, 0), 1u64 << 32);
    }

    #[test]
    fn test_combine_luid_low_bits() {
        assert_eq!(combine_luid(0, 1), 1u64);
    }

    #[test]
    fn test_aggregate_engines_same_key_sum() {
        let readings = vec![
            (
                EngineInstance {
                    luid: 1,
                    key: EngineKey { phys: 0, eng: 0 },
                    engtype: "3D".to_string(),
                },
                40.0,
            ),
            (
                EngineInstance {
                    luid: 1,
                    key: EngineKey { phys: 0, eng: 0 },
                    engtype: "3D".to_string(),
                },
                50.0,
            ),
        ];
        assert_eq!(aggregate_engines(&readings, 1), 90.0);
    }

    #[test]
    fn test_aggregate_engines_different_keys_max() {
        let readings = vec![
            (
                EngineInstance {
                    luid: 1,
                    key: EngineKey { phys: 0, eng: 0 },
                    engtype: "3D".to_string(),
                },
                20.0,
            ),
            (
                EngineInstance {
                    luid: 1,
                    key: EngineKey { phys: 0, eng: 1 },
                    engtype: "3D".to_string(),
                },
                70.0,
            ),
        ];
        assert_eq!(aggregate_engines(&readings, 1), 70.0);
    }

    #[test]
    fn test_aggregate_engines_ignore_non_matching_luid() {
        let readings = vec![
            (
                EngineInstance {
                    luid: 2,
                    key: EngineKey { phys: 0, eng: 0 },
                    engtype: "3D".to_string(),
                },
                99.0,
            ),
            (
                EngineInstance {
                    luid: 1,
                    key: EngineKey { phys: 0, eng: 0 },
                    engtype: "3D".to_string(),
                },
                30.0,
            ),
        ];
        assert_eq!(aggregate_engines(&readings, 1), 30.0);
    }

    #[test]
    fn test_aggregate_engines_clamp_to_100() {
        let readings = vec![
            (
                EngineInstance {
                    luid: 1,
                    key: EngineKey { phys: 0, eng: 0 },
                    engtype: "3D".to_string(),
                },
                60.0,
            ),
            (
                EngineInstance {
                    luid: 1,
                    key: EngineKey { phys: 0, eng: 0 },
                    engtype: "3D".to_string(),
                },
                60.0,
            ),
            (
                EngineInstance {
                    luid: 1,
                    key: EngineKey { phys: 0, eng: 0 },
                    engtype: "3D".to_string(),
                },
                60.0,
            ),
        ];
        assert_eq!(aggregate_engines(&readings, 1), 100.0);
    }

    #[test]
    fn test_aggregate_engines_empty_readings() {
        let readings: Vec<(EngineInstance, f64)> = vec![];
        assert_eq!(aggregate_engines(&readings, 1), 0.0);
    }
}
