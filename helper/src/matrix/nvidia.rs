//! Driver -> max CUDA runtime lookup.
//!
//! For pip-installed frameworks the system CUDA toolkit is irrelevant; wheels
//! bundle their own CUDA runtime, and NVIDIA drivers are backwards compatible.
//! The only question that matters is whether the installed *driver* is new
//! enough for the CUDA runtime a given wheel was built against. This module
//! answers that from the bundled [`crate::matrix::NVIDIA_JSON`] table, which
//! records the minimum driver NVIDIA has published for each CUDA release.

use std::cmp::Ordering;

use serde::Deserialize;

use crate::core::platform::Os;

#[derive(Debug, Deserialize)]
struct NvidiaFile {
    drivers: Vec<DriverRow>,
}

#[derive(Debug, Deserialize)]
struct DriverRow {
    cuda: String,
    min_driver_linux: String,
    min_driver_windows: String,
}

/// A CUDA runtime version, e.g. "12.4".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CudaVersion {
    pub major: u32,
    pub minor: u32,
}

impl CudaVersion {
    fn parse(s: &str) -> Option<CudaVersion> {
        let (major, minor) = s.split_once('.')?;
        Some(CudaVersion {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        })
    }
}

impl std::fmt::Display for CudaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// What a driver version supports, per the bundled table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverSupport {
    /// The newest CUDA runtime the table confirms this driver supports.
    pub max_known: CudaVersion,
    /// The driver version that floor came from, for messages that cite it.
    pub min_driver_for_max: String,
    /// True when the driver is at or above the newest row in the table. The
    /// driver almost certainly supports CUDA releases newer than PyPilot's
    /// bundled snapshot knows about; `max_known` is a floor, not a ceiling.
    pub exceeds_known_table: bool,
}

/// Parsed driver->CUDA table, held for the lifetime of one check.
pub struct DriverTable {
    rows: Vec<DriverRow>,
}

impl DriverTable {
    pub fn load(json: &str) -> Option<DriverTable> {
        let file: NvidiaFile = serde_json::from_str(json).ok()?;
        Some(DriverTable {
            rows: file.rows_sorted(),
        })
    }

    /// What does `driver_version` support, on `os`? `None` when the driver is
    /// older than every row the table knows, i.e. GPU/driver combination this
    /// old cannot run any CUDA build PyPilot has data for.
    pub fn support_for(&self, driver_version: &str, os: Os) -> Option<DriverSupport> {
        let driver = parse_dotted(driver_version)?;
        let mut best: Option<(&DriverRow, CudaVersion)> = None;

        for row in &self.rows {
            let min = match os {
                Os::Windows => &row.min_driver_windows,
                _ => &row.min_driver_linux,
            };
            let Some(min_parsed) = parse_dotted(min) else {
                continue;
            };
            if cmp_dotted(&driver, &min_parsed) != Ordering::Less {
                let cuda = CudaVersion::parse(&row.cuda)?;
                if best.map(|(_, c)| cuda > c).unwrap_or(true) {
                    best = Some((row, cuda));
                }
            }
        }

        let (row, max_known) = best?;
        let newest_row_min = match os {
            Os::Windows => &self.rows.last()?.min_driver_windows,
            _ => &self.rows.last()?.min_driver_linux,
        };
        let newest_min_parsed = parse_dotted(newest_row_min)?;

        Some(DriverSupport {
            max_known,
            min_driver_for_max: match os {
                Os::Windows => row.min_driver_windows.clone(),
                _ => row.min_driver_linux.clone(),
            },
            exceeds_known_table: cmp_dotted(&driver, &newest_min_parsed) != Ordering::Less,
        })
    }
}

impl NvidiaFile {
    /// Rows ordered oldest to newest CUDA, so `support_for` can take the last
    /// match as "newest supported".
    fn rows_sorted(mut self) -> Vec<DriverRow> {
        self.drivers.sort_by(|a, b| {
            let av = CudaVersion::parse(&a.cuda);
            let bv = CudaVersion::parse(&b.cuda);
            av.cmp(&bv)
        });
        self.drivers
    }
}

fn parse_dotted(s: &str) -> Option<Vec<u32>> {
    let parts: Vec<u32> = s.split('.').filter_map(|p| p.parse().ok()).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

fn cmp_dotted(a: &[u32], b: &[u32]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> DriverTable {
        DriverTable::load(crate::matrix::NVIDIA_JSON).expect("bundled table parses")
    }

    #[test]
    fn exact_ga_driver_matches_its_cuda_release() {
        let t = table();
        let s = t.support_for("535.54.03", Os::Linux).unwrap();
        assert_eq!(
            s.max_known,
            CudaVersion {
                major: 12,
                minor: 2
            }
        );
        assert!(!s.exceeds_known_table);
    }

    #[test]
    fn driver_between_two_rows_gets_the_lower_cuda() {
        let t = table();
        // Between 12.1 (530.30.02) and 12.2 (535.54.03) on Linux.
        let s = t.support_for("532.00.00", Os::Linux).unwrap();
        assert_eq!(
            s.max_known,
            CudaVersion {
                major: 12,
                minor: 1
            }
        );
    }

    #[test]
    fn windows_and_linux_floors_differ_for_the_same_cuda() {
        let t = table();
        // 536.25 is the Windows floor for 12.2; it is below the Linux floor's
        // numeric value but must still resolve on Windows.
        let s = t.support_for("536.25", Os::Windows).unwrap();
        assert_eq!(
            s.max_known,
            CudaVersion {
                major: 12,
                minor: 2
            }
        );
    }

    #[test]
    fn driver_older_than_everything_known_is_none() {
        let t = table();
        assert!(t.support_for("300.00", Os::Linux).is_none());
    }

    #[test]
    fn driver_newer_than_the_table_gets_a_soft_note() {
        let t = table();
        let s = t.support_for("999.00", Os::Linux).unwrap();
        assert!(s.exceeds_known_table);
        // Still gets the newest known floor as a confirmed-supported answer.
        assert_eq!(
            s.max_known,
            CudaVersion {
                major: 12,
                minor: 9
            }
        );
    }
}
