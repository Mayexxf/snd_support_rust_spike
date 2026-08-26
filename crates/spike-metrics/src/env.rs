//! What machine produced these numbers.
//!
//! This module exists because of how phase 0 is actually run: first on a VM to
//! prove the harness works, then on the target itself. Two runs, two very
//! different meanings, and a report that does not say which is which invites
//! exactly one mistake — reading VM numbers as an answer about the target.
//!
//! So the report leads with the machine, and states plainly whether its numbers
//! transfer.
//!
//! The target is an Intel N100 (Alder Lake-N, four cores, AVX2), not the Celeron
//! N3150 this was first aimed at. The change inverts one assumption written all
//! over the old text: the baseline numbers come from a Xeon E5-2696 v2, which
//! has AVX and no AVX2, so the target is now the machine with the wider vector
//! unit, not the one short of it.

/// Everything worth knowing about the machine under measurement.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub cpu_brand: Option<String>,
    pub arch: &'static str,
    pub cores: usize,
    /// `None` on architectures where the question does not apply.
    pub has_avx: Option<bool>,
    pub has_avx2: Option<bool>,
    pub has_sse42: Option<bool>,
    /// Hypervisor vendor string, when the CPU says it is running under one.
    pub hypervisor: Option<String>,
    /// True when the hypervisor named above runs *under* this machine rather
    /// than over it — the Hyper-V root partition. Windows 11 enters it whenever
    /// VBS is on, which is the default, so the vendor string alone proves
    /// nothing about whether these numbers came from real hardware.
    pub root_partition: bool,
    pub os: String,
}

/// Why a run's numbers may not answer the question that was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caveat {
    Virtualised(String),
    ForeignArch(&'static str),
    /// Not the target machine. Not an error — runs elsewhere are still worth
    /// making, they just mean nothing until divided by a run on the target over
    /// the same snapshot.
    NotTargetCpu { here: String },
    FewerCores { here: usize, target: usize },
}

/// Cores on the reference target, an Intel N100.
pub const TARGET_CORES: usize = 4;

/// How the target CPU names itself, as a substring of its CPUID brand string.
/// Matched rather than compared whole: the brand carries clock and padding that
/// vary between steppings, and none of that changes which machine this is.
pub const TARGET_CPU_MARK: &str = "N100";

/// First build number of Windows 11. `ProductName` in the registry still reads
/// "Windows 10 Pro" on 11 — Microsoft never updated the value — so the build is
/// the only thing that tells the two apart.
#[cfg(windows)]
const WINDOWS_11_FIRST_BUILD: u32 = 22000;

impl Machine {
    pub fn detect() -> Self {
        let hv = hypervisor();
        Self {
            cpu_brand: cpu_brand(),
            arch: std::env::consts::ARCH,
            cores: std::thread::available_parallelism().map_or(0, |n| n.get()),
            has_avx: feature("avx"),
            has_avx2: feature("avx2"),
            has_sse42: feature("sse4.2"),
            root_partition: hv.as_ref().is_some_and(|h| h.root_partition),
            hypervisor: hv.map(|h| h.vendor),
            os: os_description(),
        }
    }

    /// Reasons this run does not stand in for a run on the target machine.
    ///
    /// An empty list does not certify the machine *is* the target — it only
    /// says nothing detectable disqualifies it.
    pub fn caveats(&self) -> Vec<Caveat> {
        let mut out = Vec::new();
        // The bare hypervisor bit stopped meaning "virtual machine". Windows 11
        // turns VBS on by default, which puts the OS in the Hyper-V root
        // partition, and the bit is then set on ordinary hardware — measured on
        // the target, which is a desktop with no Hyper-V role installed.
        //
        // Only positive proof of the root partition clears the flag, never the
        // absence of proof: the two mistakes are not symmetrical. Calling the
        // target a VM is noise. Calling a VM the target is the single error this
        // module was written to prevent.
        if let Some(vendor) = &self.hypervisor {
            if !self.root_partition {
                out.push(Caveat::Virtualised(vendor.clone()));
            }
        }
        if self.arch != "x86_64" {
            out.push(Caveat::ForeignArch(self.arch));
        }
        if let Some(brand) = &self.cpu_brand {
            if !brand.contains(TARGET_CPU_MARK) {
                out.push(Caveat::NotTargetCpu { here: brand.clone() });
            }
        }
        if self.cores > 0 && self.cores < TARGET_CORES {
            out.push(Caveat::FewerCores { here: self.cores, target: TARGET_CORES });
        }
        out
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("=== Машина ===\n");
        s.push_str(&format!(
            "  процессор               {}\n",
            self.cpu_brand.as_deref().unwrap_or("неизвестен")
        ));
        s.push_str(&format!(
            "  архитектура             {}, ядер {}\n",
            self.arch, self.cores
        ));
        s.push_str(&format!(
            "  наборы инструкций       SSE4.2 {}, AVX {}, AVX2 {}\n",
            yes_no(self.has_sse42),
            yes_no(self.has_avx),
            yes_no(self.has_avx2)
        ));
        s.push_str(&format!("  система                 {}\n", self.os));
        if let Some(v) = &self.hypervisor {
            // Which side of the hypervisor this machine is on decides whether
            // the rest of the report means anything, so it is printed, not left
            // for the reader to infer from the vendor string.
            let side = if self.root_partition {
                " (корневой раздел — железо настоящее, гипервизор от VBS)"
            } else {
                " (гость)"
            };
            s.push_str(&format!("  гипервизор              {v}{side}\n"));
        }

        let caveats = self.caveats();
        if caveats.is_empty() {
            s.push_str("\n  Ничто не мешает считать эти цифры показательными для целевой машины.\n");
        } else {
            s.push_str("\n  ⚠ ЦИФРЫ НЕ ПЕРЕНОСЯТСЯ НА ЦЕЛЕВУЮ МАШИНУ:\n");
            for c in caveats {
                s.push_str(&format!("    · {}\n", c.explain()));
            }
            s.push_str("    Этот прогон проверяет, что стенд работает, а не что решение проходит.\n");
        }
        s
    }
}

impl Caveat {
    pub fn explain(&self) -> String {
        match self {
            Caveat::Virtualised(vendor) => format!(
                "виртуальная машина ({vendor}). Захват экрана и работа с GPU здесь \
                 ведут себя иначе, чем на живом железе"
            ),
            Caveat::ForeignArch(arch) => format!(
                "архитектура {arch}, а целевая — x86_64. Если это ARM-хост, \
                 x86-код идёт через эмуляцию и замер скорости бессмыслен"
            ),
            Caveat::NotTargetCpu { here } => format!(
                "процессор {here}, а целевой — Intel N100. Числа отсюда значат \
                 что-то только после деления на прогон по тому же снимку на цели"
            ),
            Caveat::FewerCores { here, target } => format!(
                "ядер {here}, а на целевой машине {target}"
            ),
        }
    }
}

fn yes_no(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "есть",
        Some(false) => "нет",
        None => "н/д",
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn feature(name: &str) -> Option<bool> {
    Some(match name {
        "avx" => is_x86_feature_detected!("avx"),
        "avx2" => is_x86_feature_detected!("avx2"),
        "sse4.2" => is_x86_feature_detected!("sse4.2"),
        _ => return None,
    })
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn feature(_name: &str) -> Option<bool> {
    None
}

/// One CPUID leaf.
///
/// The `unsafe` here is version-dependent: newer compilers consider `__cpuid`
/// safe on x86_64 (it needs no target feature beyond the baseline) and warn that
/// the block is redundant, while the declared MSRV still requires it. Rather
/// than guess the exact release that changed, the requirement is satisfied and
/// the warning suppressed in one place — scoped to this three-line function, so
/// it cannot hide a redundant `unsafe` anywhere else.
#[cfg(target_arch = "x86_64")]
#[allow(unused_unsafe)]
fn cpuid(leaf: u32) -> std::arch::x86_64::CpuidResult {
    // SAFETY: CPUID is unconditionally available on x86_64, the leaves this
    // module reads are architecturally defined, and the instruction has no side
    // effects and touches no memory.
    unsafe { std::arch::x86_64::__cpuid(leaf) }
}

#[cfg(target_arch = "x86_64")]
fn cpu_brand() -> Option<String> {
    let max_ext = cpuid(0x8000_0000).eax;
    if max_ext < 0x8000_0004 {
        return None;
    }

    let mut bytes = Vec::with_capacity(48);
    for leaf in 0x8000_0002u32..=0x8000_0004 {
        let r = cpuid(leaf);
        for reg in [r.eax, r.ebx, r.ecx, r.edx] {
            bytes.extend_from_slice(&reg.to_le_bytes());
        }
    }
    let s = String::from_utf8_lossy(&bytes)
        .trim_end_matches('\0')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!s.is_empty()).then_some(s)
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_brand() -> Option<String> {
    None
}

/// A hypervisor, and which side of it this code is running on.
struct Hypervisor {
    vendor: String,
    root_partition: bool,
}

#[cfg(target_arch = "x86_64")]
fn hypervisor() -> Option<Hypervisor> {
    // ECX bit 31 of leaf 1 is the "hypervisor present" bit. It used to be enough
    // on its own — real silicon left it clear. It does not any more: Windows 11
    // turns VBS on by default, the OS then runs in the Hyper-V root partition,
    // and the bit is set on a plain desktop. Measured on the target: bit set,
    // vendor "Microsoft Hv", no Hyper-V role installed, LarkBox X mainboard.
    if cpuid(1).ecx & (1 << 31) == 0 {
        return None;
    }

    // Only meaningful once the hypervisor bit is set: leaf 0x40000000 is the
    // hypervisor vendor range, and on bare metal it returns whatever it likes.
    let r = cpuid(0x4000_0000);
    let mut bytes = Vec::with_capacity(12);
    for reg in [r.ebx, r.ecx, r.edx] {
        bytes.extend_from_slice(&reg.to_le_bytes());
    }
    let vendor = String::from_utf8_lossy(&bytes)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_owned();

    let root_partition = vendor == HYPER_V && r.eax >= 0x4000_0003 && {
        // Hyper-V publishes the partition's privileges in leaf 0x40000003.
        // Creating partitions and managing CPUs are the root's jobs and a guest
        // is not granted them. Read only under Hyper-V, because the leaf means
        // something else entirely under other vendors.
        //
        // Measured on the target: EBX = 0x002bb9ff, both bits set.
        const CREATE_PARTITIONS: u32 = 1 << 0;
        const CPU_MANAGEMENT: u32 = 1 << 12;
        const ROOT_ONLY: u32 = CREATE_PARTITIONS | CPU_MANAGEMENT;
        cpuid(0x4000_0003).ebx & ROOT_ONLY == ROOT_ONLY
    };

    Some(Hypervisor {
        vendor: if vendor.is_empty() { "неизвестный".to_owned() } else { vendor },
        root_partition,
    })
}

/// Leaf 0x40000000's vendor string for Hyper-V, and for the Windows 11 VBS
/// hypervisor, which is the same thing wearing a different hat.
#[cfg(target_arch = "x86_64")]
const HYPER_V: &str = "Microsoft Hv";

#[cfg(not(target_arch = "x86_64"))]
fn hypervisor() -> Option<Hypervisor> {
    None
}

#[cfg(windows)]
fn os_description() -> String {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    // Read the registry rather than call GetVersionEx: without an explicit
    // compatibility manifest Windows lies to that API and reports 6.2, which
    // would make every report claim Windows 8.
    let fallback = || format!("{} (версия не прочитана)", std::env::consts::OS);
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
    else {
        return fallback();
    };

    let product: String = key.get_value("ProductName").unwrap_or_default();
    let display: String = key.get_value("DisplayVersion").unwrap_or_default();
    let build: String = key.get_value("CurrentBuild").unwrap_or_default();
    windows_name(&product, &display, &build)
}

/// Assemble the OS name from the three registry values that describe it.
///
/// Split out from the registry read because the interesting part is not reading
/// the key — it is knowing that `ProductName` lies. On Windows 11 it still reads
/// "Windows 10 Pro"; measured on the target, which reports exactly that at build
/// 22631. Left uncorrected, every report from a Windows 11 machine names the
/// wrong OS, and these reports exist to be compared across machines.
#[cfg(windows)]
fn windows_name(product: &str, display: &str, build: &str) -> String {
    let mut s = if product.is_empty() { "Windows".to_owned() } else { product.to_owned() };
    if build.parse::<u32>().is_ok_and(|b| b >= WINDOWS_11_FIRST_BUILD) {
        s = s.replace("Windows 10", "Windows 11");
    }
    if !display.is_empty() {
        s.push_str(&format!(" {display}"));
    }
    if !build.is_empty() {
        s.push_str(&format!(" (сборка {build})"));
    }
    s
}

#[cfg(not(windows))]
fn os_description() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::FAMILY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_fills_in_what_it_can() {
        let m = Machine::detect();
        assert!(m.cores >= 1);
        assert!(!m.arch.is_empty());
        assert!(!m.os.is_empty());
        // Rendering must never panic, whatever the host turns out to be — this
        // same code runs on the dev Mac, a VM and the target Celeron.
        let text = m.render();
        assert!(text.contains("Машина"));
    }

    #[test]
    fn a_machine_that_is_not_the_target_says_which_one_it_is() {
        // The baseline numbers come from this one, so the caveat has to name it
        // rather than just deny it.
        let m = Machine {
            cpu_brand: Some("Intel(R) Xeon(R) CPU E5-2696 v2 @ 2.50GHz".to_owned()),
            arch: "x86_64",
            cores: 4,
            ..Default::default()
        };
        let caveats = m.caveats();
        assert!(
            caveats.iter().any(|c| matches!(c, Caveat::NotTargetCpu { .. })),
            "{caveats:?}"
        );
        assert!(m.render().contains("E5-2696"));
    }

    #[test]
    fn an_n100_shaped_machine_is_not_flagged() {
        let m = Machine {
            cpu_brand: Some("Intel(R) N100".to_owned()),
            arch: "x86_64",
            cores: 4,
            has_avx: Some(true),
            has_avx2: Some(true),
            has_sse42: Some(true),
            // What the target actually reports: VBS is on, so the hypervisor bit
            // is set, and the partition privileges say this side is the root.
            hypervisor: Some("Microsoft Hv".to_owned()),
            root_partition: true,
            os: "Windows 11 Pro 23H2 (сборка 22631)".to_owned(),
        };
        assert!(m.caveats().is_empty(), "{:?}", m.caveats());
        assert!(m.render().contains("показательными"));
    }

    #[test]
    fn the_root_partition_is_not_a_virtual_machine() {
        // Same vendor string, opposite meaning. Before this distinction existed
        // every run on the target printed "ЦИФРЫ НЕ ПЕРЕНОСЯТСЯ" about itself.
        let root = Machine {
            arch: "x86_64",
            cores: 4,
            hypervisor: Some("Microsoft Hv".to_owned()),
            root_partition: true,
            ..Default::default()
        };
        assert!(
            !root.caveats().iter().any(|c| matches!(c, Caveat::Virtualised(_))),
            "{:?}",
            root.caveats()
        );
        assert!(root.render().contains("железо настоящее"));

        let guest = Machine { root_partition: false, ..root };
        assert!(guest.caveats().iter().any(|c| matches!(c, Caveat::Virtualised(_))));
        assert!(guest.render().contains("(гость)"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_eleven_is_not_reported_as_windows_ten() {
        // Exactly what the target's registry holds. ProductName was never
        // updated for 11, so only the build number separates them.
        assert_eq!(
            windows_name("Windows 10 Pro", "23H2", "22631"),
            "Windows 11 Pro 23H2 (сборка 22631)"
        );
        // And a real Windows 10 must not be renamed: the baseline machine is
        // build 19045, and its reports are read next to the target's.
        assert_eq!(
            windows_name("Windows 10 Pro", "22H2", "19045"),
            "Windows 10 Pro 22H2 (сборка 19045)"
        );
        // A build number that will not parse must not silently rename anything.
        assert_eq!(windows_name("Windows 10 Pro", "", ""), "Windows 10 Pro");
    }

    #[test]
    fn a_vm_says_so_loudly() {
        let m = Machine {
            arch: "x86_64",
            cores: 4,
            has_avx: Some(false),
            hypervisor: Some("prl hyperv".to_owned()),
            ..Default::default()
        };
        let caveats = m.caveats();
        assert_eq!(caveats.len(), 1);
        assert!(matches!(caveats[0], Caveat::Virtualised(_)));
        assert!(m.render().contains("НЕ ПЕРЕНОСЯТСЯ"));
    }

    #[test]
    fn an_arm_host_is_flagged_separately_from_virtualisation() {
        // The realistic first run: a Windows-on-ARM VM on an Apple Silicon Mac.
        // Both caveats must fire, because they invalidate the numbers for
        // different reasons.
        let m = Machine {
            arch: "aarch64",
            cores: 8,
            has_avx: None,
            hypervisor: Some("Microsoft Hv".to_owned()),
            ..Default::default()
        };
        let caveats = m.caveats();
        assert!(caveats.iter().any(|c| matches!(c, Caveat::Virtualised(_))));
        assert!(caveats.iter().any(|c| matches!(c, Caveat::ForeignArch("aarch64"))));
    }
}
