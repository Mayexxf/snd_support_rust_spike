//! What machine produced these numbers.
//!
//! This module exists because of how phase 0 is actually run: first on a VM to
//! prove the harness works, then once on the target Celeron. Two runs, two very
//! different meanings, and a report that does not say which is which invites
//! exactly one mistake — reading VM numbers as an answer about the target.
//!
//! So the report leads with the machine, and states plainly whether its numbers
//! transfer.

/// Everything worth knowing about the machine under measurement.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub cpu_brand: Option<String>,
    pub arch: &'static str,
    pub cores: usize,
    /// `None` on architectures where the question does not apply.
    pub has_avx: Option<bool>,
    pub has_sse42: Option<bool>,
    /// Hypervisor vendor string, when the CPU says it is running under one.
    pub hypervisor: Option<String>,
    pub os: String,
}

/// Why a run's numbers may not answer the question that was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caveat {
    Virtualised(String),
    ForeignArch(&'static str),
    /// The target CPU (Braswell) has SSE4.2 and no AVX. A machine with AVX will
    /// flatter a software encoder that the target cannot run the same way.
    HasAvx,
    FewerCores { here: usize, target: usize },
}

/// Cores on the reference target, an Intel Celeron N3150.
pub const TARGET_CORES: usize = 4;

impl Machine {
    pub fn detect() -> Self {
        Self {
            cpu_brand: cpu_brand(),
            arch: std::env::consts::ARCH,
            cores: std::thread::available_parallelism().map_or(0, |n| n.get()),
            has_avx: feature("avx"),
            has_sse42: feature("sse4.2"),
            hypervisor: hypervisor(),
            os: os_description(),
        }
    }

    /// Reasons this run does not stand in for a run on the target machine.
    ///
    /// An empty list does not certify the machine *is* a Celeron N3150 — it only
    /// says nothing detectable disqualifies it.
    pub fn caveats(&self) -> Vec<Caveat> {
        let mut out = Vec::new();
        if let Some(vendor) = &self.hypervisor {
            out.push(Caveat::Virtualised(vendor.clone()));
        }
        if self.arch != "x86_64" {
            out.push(Caveat::ForeignArch(self.arch));
        }
        if self.has_avx == Some(true) {
            out.push(Caveat::HasAvx);
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
            "  наборы инструкций       SSE4.2 {}, AVX {}\n",
            yes_no(self.has_sse42),
            yes_no(self.has_avx)
        ));
        s.push_str(&format!("  система                 {}\n", self.os));
        if let Some(v) = &self.hypervisor {
            s.push_str(&format!("  гипервизор              {v}\n"));
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
            Caveat::HasAvx => "у процессора есть AVX, а у целевого Braswell его нет. \
                 Программный кодер здесь получит фору, которой на целевой машине не будет"
                .to_owned(),
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

#[cfg(target_arch = "x86_64")]
fn hypervisor() -> Option<String> {
    // ECX bit 31 of leaf 1 is the "hypervisor present" bit. Real silicon leaves
    // it clear; every mainstream hypervisor sets it, and none of them has a
    // reason to lie in the direction that matters here.
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

    Some(if vendor.is_empty() { "неизвестный".to_owned() } else { vendor })
}

#[cfg(not(target_arch = "x86_64"))]
fn hypervisor() -> Option<String> {
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

    let mut s = if product.is_empty() { "Windows".to_owned() } else { product };
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
    fn a_machine_with_avx_is_flagged() {
        let m = Machine { has_avx: Some(true), arch: "x86_64", cores: 4, ..Default::default() };
        assert!(m.caveats().contains(&Caveat::HasAvx));
    }

    #[test]
    fn a_braswell_shaped_machine_is_not_flagged() {
        let m = Machine {
            cpu_brand: Some("Intel(R) Celeron(R) CPU N3150 @ 1.60GHz".to_owned()),
            arch: "x86_64",
            cores: 4,
            has_avx: Some(false),
            has_sse42: Some(true),
            hypervisor: None,
            os: "Windows 10".to_owned(),
        };
        assert!(m.caveats().is_empty(), "{:?}", m.caveats());
        assert!(m.render().contains("показательными"));
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
