//! The ELF tests' targets: the host's, and each cross target whose cross
//! compiler and QEMU are installed, whose programs run under QEMU.

use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use crate::{
    Listed, Mode, Options, Target, fail, make_jobs, parse_options, prepare_work_dir,
    print_inventory, print_summary, print_test_list, run_exact, run_jobs, selected_scripts,
};

struct TargetSpec {
    machine: &'static str,
    triple: &'static str,
    qemu: &'static str,
}

// The targets tested by default: the ones Codira builds Funis for. The mold
// targets that were removed from this tree (see NOTICE) are not tested.
// A target other than the host's is
// tested only if its cross compiler and QEMU are installed.
const TARGETS: &[TargetSpec] = &[
    TargetSpec { machine: "x86_64", triple: "x86_64-linux-gnu", qemu: "qemu-x86_64" },
    TargetSpec { machine: "aarch64", triple: "aarch64-linux-gnu", qemu: "qemu-aarch64" },
    TargetSpec { machine: "riscv64", triple: "riscv64-linux-gnu", qemu: "qemu-riscv64" },
];

/// A target, its programs run natively without a triple, under QEMU
/// with one.
fn target(machine: &str, triple: Option<&str>, cpu: Option<&str>) -> Target {
    let label = match cpu {
        Some(cpu) => format!("{machine}-{cpu}"),
        None => machine.to_owned(),
    };
    Target {
        label,
        arch: Some(machine.to_owned()),
        env: vec![
            ("MACHINE", Some(machine.to_owned())),
            ("TRIPLE", triple.map(str::to_owned)),
            ("CPU", cpu.map(str::to_owned)),
        ],
    }
}

fn canonical_machine(machine: &str) -> String {
    let machine = machine.trim();
    if machine == "amd64" {
        return "x86_64".to_owned();
    }
    if machine.len() == 4 && machine.starts_with('i') && machine.ends_with("86") {
        return "i686".to_owned();
    }
    if machine.starts_with("armeb") {
        return "armeb".to_owned();
    }
    if machine.starts_with("arm") {
        return "arm".to_owned();
    }
    match machine {
        "powerpc" => "ppc".to_owned(),
        "powerpc64" => "ppc64".to_owned(),
        "powerpc64le" => "ppc64le".to_owned(),
        _ => machine.to_owned(),
    }
}

fn machine_from_triple(triple: &str) -> String {
    canonical_machine(triple.split('-').next().unwrap_or(triple))
}

fn native_machine() -> String {
    if let Some(machine) = env::var_os("MACHINE").filter(|s| !s.is_empty()) {
        return canonical_machine(&machine.to_string_lossy());
    }

    if let Ok(output) = Command::new("cc").arg("-dumpmachine").output()
        && output.status.success()
    {
        let triple = String::from_utf8_lossy(&output.stdout);
        if !triple.trim().is_empty() {
            return machine_from_triple(&triple);
        }
    }
    canonical_machine(env::consts::ARCH)
}

fn command_exists(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|dir| dir.join(command).is_file()))
}

fn supports_power10() -> bool {
    if !command_exists("powerpc64le-linux-gnu-gcc") || !command_exists("qemu-ppc64le") {
        return false;
    }

    let compiler = Command::new("powerpc64le-linux-gnu-gcc")
        .args(["-mcpu=power10", "-E", "-x", "c", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let qemu = Command::new("qemu-ppc64le")
        .args(["-cpu", "help"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("power10_v2.0"));
    compiler && qemu
}

fn all_targets(native: &str) -> (Vec<Target>, Vec<&'static TargetSpec>) {
    let mut targets = Vec::new();
    let mut unavailable = Vec::new();

    for spec in TARGETS {
        if spec.machine == native {
            targets.push(target(native, None, None));
        } else if command_exists(spec.qemu) && command_exists(&format!("{}-gcc", spec.triple)) {
            targets.push(target(spec.machine, Some(spec.triple), None));
        } else {
            unavailable.push(spec);
        }
    }

    if native == "ppc64le"
        && fs::read_to_string("/proc/cpuinfo").is_ok_and(|s| s.contains("POWER10"))
    {
        targets.push(target(native, None, Some("power10")));
    } else if supports_power10() {
        targets.push(target("ppc64le", Some("powerpc64le-linux-gnu"), Some("power10")));
    }

    (targets, unavailable)
}

fn selected_targets(options: &Options) -> (Vec<Target>, Vec<&'static TargetSpec>) {
    let native = native_machine();
    match &options.mode {
        Mode::Native => (vec![target(&native, None, None)], Vec::new()),
        Mode::Default | Mode::All => all_targets(&native),
        Mode::Triple(triple) => {
            let machine = machine_from_triple(triple);
            (vec![target(&machine, Some(triple), options.cpu.as_deref())], Vec::new())
        }
    }
}

/// Runs the ELF test scripts in `cases_dir` with the linker at `funis`.
/// Command line arguments are substring patterns selecting a subset of
/// tests. `--list --format terse` and `--exact` speak the test harness
/// protocol cargo-nextest uses, with one test per script and target; a
/// cross target without its toolchain has its tests listed as ignored.
pub fn run(cases_dir: &Path, funis: &Path) -> ExitCode {
    let options = parse_options();
    let work_dir = prepare_work_dir(funis).unwrap_or_else(|err| fail(err));
    let (targets, unavailable) = selected_targets(&options);
    if options.terse || options.exact {
        let scripts = selected_scripts(cases_dir, &[]).unwrap_or_else(|err| fail(err));
        let missing = unavailable.iter().map(|spec| Listed {
            target: target(spec.machine, Some(spec.triple), None),
            whole: false,
            unavailable: Some(format!(
                "needs the cross compiler {}-gcc and {}, which are not installed",
                spec.triple, spec.qemu
            )),
        });
        let listed: Vec<_> = targets.into_iter().map(Listed::available).chain(missing).collect();
        if options.list {
            return print_test_list(&scripts, &listed, options.ignored);
        }
        return run_exact(&options, &scripts, &work_dir, &listed, |_| unreachable!());
    }
    let scripts = selected_scripts(cases_dir, &options.patterns).unwrap_or_else(|err| fail(err));
    let unavailable: Vec<_> = unavailable.iter().map(|spec| spec.machine.to_owned()).collect();
    if options.list {
        return print_inventory(&scripts, &work_dir, targets, &unavailable);
    }
    if !unavailable.is_empty() {
        eprintln!("skipping targets without both compiler and QEMU: {}", unavailable.join(", "));
    }
    let jobs = make_jobs(&scripts, &work_dir, targets, true).unwrap_or_else(|err| fail(err));
    print_summary(&run_jobs(&work_dir, jobs, &options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_machine_names() {
        assert_eq!(canonical_machine("amd64"), "x86_64");
        assert_eq!(canonical_machine("i386"), "i686");
        assert_eq!(canonical_machine("armv7l"), "arm");
        assert_eq!(canonical_machine("aarch64"), "aarch64");
        assert_eq!(canonical_machine("powerpc64le"), "ppc64le");
    }
}
