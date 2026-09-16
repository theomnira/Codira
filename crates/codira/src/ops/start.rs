//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::path::PathBuf;

use anyhow::anyhow;
use codira_runtime::Runtime;

use crate::ExitStatus;

#[derive(clap::Args)]
pub struct Args {
    /// The library to use
    library: PathBuf,

    /// The function entry point to call on startup
    #[clap(default_value_t = String::from("main"))]
    entry: String,

    /// Run under the HERACLES self-healing supervisor.
    ///
    /// Installs the process-wide hardware-fault handler and routes any
    /// trap raised by the entry point to `codira_healing`'s engine, which
    /// ranks and applies a recovery strategy instead of letting the fault
    /// terminate the process. See `spec/HERACLES_Codira_Implementation.md`.
    #[clap(long)]
    heal: bool,
}

/// Maps a hardware fault to the healing engine's fault taxonomy.
///
/// The two vocabularies are deliberately separate: [`trap::FaultKind`] is
/// what the CPU raised, [`FaultClass`] is what a `@heal` contract selects
/// a strategy for. They line up closely for the trap-borne faults, but the
/// contract language also has classes no CPU raises (`Timeout`,
/// `ConnectionRefused`, `MemoryLeak`), which is why the mapping is
/// explicit rather than a cast.
///
/// `IllegalInstruction` is deliberately **not** mapped: it means the
/// process is executing something that is not code, so the correct
/// response is to let it terminate rather than hand it to a recovery
/// strategy. Returning `None` routes it to the default handler.
fn classify_fault(
    kind: codira_healing::trap::FaultKind,
) -> Option<codira_healing::contract::FaultClass> {
    use codira_healing::{contract::FaultClass, trap::FaultKind};
    Some(match kind {
        FaultKind::AccessViolation => FaultClass::AccessViolation,
        FaultKind::IntegerDivideByZero
        | FaultKind::FloatDivideByZero
        | FaultKind::FloatInvalidOperation => FaultClass::ArithmeticFault,
        FaultKind::IllegalInstruction => return None,
    })
}

/// Starts the runtime with the specified library and invokes function `entry`.
pub fn start(args: Args) -> anyhow::Result<ExitStatus> {
    // Arm fault interception *before* the runtime loads anything, so a
    // fault during library initialization is caught too. Installing the
    // handler without `--heal` would change process-wide fault behaviour
    // for a user who did not ask for it.
    if args.heal {
        codira_healing::trap::install();
        log::info!("HERACLES self-healing supervisor armed");
    }

    let builder = Runtime::builder(args.library);

    // Safety: we assume that the passed in library is safe
    let runtime = unsafe { builder.finish() }?;

    let fn_definition = runtime
        .get_function_definition(&args.entry)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Failed to obtain entry point '{}'", args.entry),
            )
        })?;

    // The entry point's invocation, factored out so it can run either
    // directly or under the fault-interception guard below.
    let invoke = || -> anyhow::Result<ExitStatus> {
        let return_type = &fn_definition.prototype.signature.return_type;

        /// Dispatches on the entry point's return type, invoking and
        /// printing the first arm whose type matches.
        ///
        /// Every marshallable primitive is listed rather than just the
        /// three that used to be here: an entry point returning `i32` --
        /// the single most natural signature for `main` in a systems
        /// language, and what `codira new` would lead anyone to write --
        /// was rejected outright with "only native Codira return types are
        /// supported", which read as a type-system limitation when it was
        /// only a missing branch.
        macro_rules! dispatch_on_return_type {
            ($($ty:ty),+ $(,)?) => {
                $(
                    if return_type.equals::<$ty>() {
                        let result: $ty = runtime
                            .invoke(&args.entry, ())
                            .map_err(|e| anyhow!("{}", e))?;

                        println!("{result}");
                        return Ok(ExitStatus::Success);
                    }
                )+
            };
        }

        dispatch_on_return_type!(
            bool, i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64,
        );

        if return_type.equals::<()>() {
            #[allow(clippy::unit_arg)]
            runtime
                .invoke(&args.entry, ())
                .map(|_: ()| ExitStatus::Success)
                .map_err(|e| anyhow!("{}", e))?;
            return Ok(ExitStatus::Success);
        }

        // A struct, array or other composite return type genuinely cannot
        // be printed here -- there is no marshalling for it across the
        // runtime boundary -- so this stays an error, but one that names
        // what *is* accepted instead of implying nothing else exists.
        Err(anyhow!(
            "entry point '{}' returns `{}`, which cannot be marshalled across the runtime \
             boundary. An entry point must return a primitive (bool, an integer or \
             floating-point type) or nothing at all.",
            args.entry,
            return_type.name()
        ))
    };

    if !args.heal {
        return invoke();
    }

    // Under `--heal`, a hardware fault anywhere in the entry point -- at
    // any call depth -- returns here as an `Err` instead of terminating
    // the process. See `codira_healing::trap::protected` for exactly what
    // "catching" means: the fault really happened, and what is recovered
    // is control flow, not the abandoned work.
    let engine = codira_healing::engine::HealingEngine::new();

    // Site identity. The engine ranks strategies per site, so the id must
    // be stable across runs for Thompson sampling to accumulate evidence;
    // the entry point's name is the only stable handle available here.
    // A real per-call-site id arrives with `@heal` contract lowering.
    let site_id = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        args.entry.hash(&mut hasher);
        hasher.finish()
    };

    match codira_healing::trap::protected(invoke) {
        Ok(result) => result,
        Err(fault) => {
            let Some(class) = classify_fault(fault.kind) else {
                return Err(anyhow!(
                    "fatal fault at {:#x}: {:?} is not recoverable",
                    fault.instruction_pointer,
                    fault.kind
                ));
            };
            log::warn!(
                "fault {:?} at {:#x}; consulting the healing engine",
                fault.kind,
                fault.instruction_pointer
            );
            match engine.handle_fault(site_id, class) {
                codira_healing::engine::HealingResult::Recovered(_) => {
                    log::info!("recovered from {class:?}");
                    Ok(ExitStatus::Success)
                }
                // No contract was registered for this site, so the engine
                // has no strategy to apply. Reporting that plainly is more
                // useful than a bare crash, and it is the honest state
                // until `@heal` contract lowering registers real
                // contracts from source.
                outcome => Err(anyhow!(
                    "unrecovered fault {:?} at {:#x} (engine: {:?}); \
                     no `@heal` contract is registered for entry point '{}'",
                    fault.kind,
                    fault.instruction_pointer,
                    outcome,
                    args.entry
                )),
            }
        }
    }
}
