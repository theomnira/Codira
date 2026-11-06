//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Compiles a module group's `@target(gpu)` functions to PTX for NVIDIA GPUs
//!   through LLVM's NVPTX backend.
//!
//! # The kernel subset
//!
//! A `@target(gpu)` function is compiled twice: as an ordinary function in
//! the host library, and -- here -- for the GPU, where it runs once per
//! thread of a launch. On the GPU there is no Codira runtime, so the same
//! rule as for WebAssembly applies: only code that needs nothing from it.
//!
//! * Parameters are integers, floats and booleans. Buffers are passed as
//!   `usize` device addresses and read and written with the `load_<t>` /
//!   `store_<t>` intrinsics, as host code reads caller-owned memory.
//! * A function returning nothing is a **kernel**: an entry point the host
//!   launches (`ptx_kernel`). One returning a value is a device function
//!   kernels can call.
//! * A GPU function may call other `@target(gpu)` functions and intrinsics
//!   (including `gpu_thread_id_x` and friends, see `ir::intrinsic_ops::gpu`),
//!   nothing else: no GC allocation, no dispatch table, no `extern` functions,
//!   no inline assembly.
//!
//! Code outside the subset is refused with the reason, at build time.
//!
//! # How
//!
//! The group is lowered with the ordinary HIR -> LLVM generator, against an
//! NVPTX target machine and its data layout. Everything that is not a GPU
//! function is then deleted; anything a GPU function still needs after that
//! is what the subset forbids, and is reported by name. What remains goes
//! through LLVM's optimizer and the NVPTX backend, which prints PTX: text the
//! CUDA driver JIT-compiles for whatever GPU it finds (`codira_gpu`).

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use codira_hir::{Function, HasSource, HirDatabase, TyKind};
use codira_syntax::ast::{self, AttributeOwner};
use codira_target::spec::Target;
use inkwell::{
    context::Context,
    module::Linkage,
    targets::{CodeModel, FileType, RelocMode, TargetTriple},
    values::{BasicValue, FunctionValue},
};

use crate::{
    code_gen::{optimize_module, CodeGenContext},
    ir::{file::gen_file_ir, file_group::gen_file_group_ir, ty::HirTypeCache},
    CodeGenDatabase, ModuleGroupId,
};

/// LLVM's `ptx_kernel` calling convention: what makes a function an entry
/// point the CUDA driver can launch.
const PTX_KERNEL_CALLING_CONVENTION: u32 = 71;

/// The globals a native runtime fills in when it loads a `.codiralib`, and
/// which therefore stay empty on a GPU.
const RUNTIME_GLOBALS: [&str; 3] = [
    "dispatchTable",
    "allocatorHandle",
    crate::ir::type_table::TypeTable::NAME,
];

/// Whether `function` carries `@target(gpu)`.
pub fn is_gpu_function(db: &dyn HirDatabase, function: Function) -> bool {
    let source = function.source(db);
    let Some(attributes) = source.value.attribute_list() else {
        return false;
    };
    attributes.attributes().any(|attribute| {
        let named_target = attribute.path().and_then(|p| p.segment()).is_some_and(
            |s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "target"),
        );
        named_target
            && attribute.arg_list().is_some_and(|args| {
                args.args().next().is_some_and(|arg| {
                    codira_syntax::AstNode::syntax(&arg)
                        .text()
                        .to_string()
                        .trim()
                        == "gpu"
                })
            })
    })
}

/// The PTX for a module group's GPU functions, and which of them are
/// kernels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuKernels {
    /// PTX assembly text, ready for `cuModuleLoadData`.
    pub ptx: String,
    /// The kernels (entry points) in it, by name.
    pub kernels: Vec<String>,
}

/// Salsa query behind `CodeGenDatabase::gpu_kernels`: the target's default
/// GPU architecture.
pub(crate) fn gpu_kernels_query(
    db: &dyn CodeGenDatabase,
    group: ModuleGroupId,
) -> std::sync::Arc<Result<Option<GpuKernels>, String>> {
    std::sync::Arc::new(compile_gpu_kernels(db, group, None))
}

/// Compiles the `@target(gpu)` functions of `group` to PTX for `cpu` (an
/// `sm_*` name; `None` for the target's default). `Ok(None)` when the group
/// has none.
pub fn compile_gpu_kernels(
    db: &dyn CodeGenDatabase,
    group: ModuleGroupId,
    cpu: Option<&str>,
) -> Result<Option<GpuKernels>, String> {
    let partition = db.module_partition();
    let module_group = &partition[group];
    let gpu_functions: Vec<Function> = module_group
        .iter()
        .flat_map(|module| module.all_functions(db))
        .filter(|&f| is_gpu_function(db, f))
        .collect();
    if gpu_functions.is_empty() {
        return Ok(None);
    }

    let mut target = Target::gpu_target();
    if let Some(cpu) = cpu {
        cpu.clone_into(&mut target.options.cpu);
    }
    let refuse = |reason: String| {
        format!(
            "cannot compile the @target(gpu) functions of `{}` for {}: {reason}",
            module_group.name, target.llvm_target
        )
    };

    let mut kernels = Vec::new();
    for &function in &gpu_functions {
        let signature = function
            .ty(db)
            .callable_sig(db)
            .expect("a function has a signature");
        let scalar = |ty: &codira_hir::Ty| {
            matches!(
                ty.interned(),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool
            )
        };
        if !signature.params().iter().all(scalar) {
            return Err(refuse(format!(
                "`{}` takes a parameter that is not an integer, float or bool; pass buffers \
                 as `usize` device addresses",
                function.name(db)
            )));
        }
        if signature.ret().is_empty() {
            kernels.push(function.name(db).to_string());
        } else if !scalar(signature.ret()) {
            return Err(refuse(format!(
                "`{}` returns a value that is not an integer, float or bool",
                function.name(db)
            )));
        }
    }

    crate::db::initialize_target_backends_once();
    let triple = TargetTriple::create(&target.llvm_target);
    let backend = inkwell::targets::Target::from_triple(&triple)
        .map_err(|e| refuse(format!("this compiler has no NVPTX backend ({e})")))?;
    let machine = backend
        .create_target_machine(
            &triple,
            &target.options.cpu,
            &target.options.features,
            db.optimization_level(),
            RelocMode::Default,
            CodeModel::Default,
        )
        .ok_or_else(|| refuse(format!("LLVM has no `{}` processor", target.options.cpu)))?;

    let context = Context::create();
    let code_gen = CodeGenContext {
        context: &context,
        db,
        rust_types: RefCell::new(HashMap::default()),
        hir_types: HirTypeCache::new(&context, db, machine.get_target_data()),
        optimization_level: db.optimization_level(),
        eidos: db.eidos_options().optimizer(&target),
        codegen_db: db,
        // C and C++ in the package are host code; a GPU function that calls
        // them is refused below like any other host call.
        native: db.native_inputs(),
        // Fuel counts against a counter the host runtime owns; a kernel has
        // no such counter, so it is never instrumented.
        fuel: false,
        // PTX is not a library a certificate describes.
        keep_function_symbols: false,
        // Likewise for the hardware-safety modes: they guard host memory.
        hardware_safety: crate::HardwareSafety::NONE,
        // A kernel has no GC heap, so nothing to move off it.
        frame_allocation: false,
        safety_refusals: Rc::default(),
        target_machine: Rc::new(machine),
    };

    let group_ir = gen_file_group_ir(&code_gen, module_group);
    let file_ir = gen_file_ir(&code_gen, &group_ir, module_group);
    let module = file_ir.llvm_module;
    module
        .link_in_module(group_ir.llvm_module)
        .map_err(|e| refuse(e.to_string()))?;

    // Delete every host function nothing on the GPU side calls, until none
    // is left to delete. Whatever still stands is called by a GPU function.
    let gpu_names: Vec<String> = gpu_functions
        .iter()
        .map(|f| f.name(db).to_string())
        .collect();
    let is_gpu = |f: &FunctionValue<'_>| {
        gpu_names
            .iter()
            .any(|name| f.get_name().to_bytes() == name.as_bytes())
    };
    loop {
        let unused: Vec<FunctionValue<'_>> = module
            .get_functions()
            .filter(|f| {
                !is_gpu(f)
                    && f.as_global_value()
                        .as_pointer_value()
                        .get_first_use()
                        .is_none()
            })
            .collect();
        if unused.is_empty() {
            break;
        }
        for function in unused {
            // SAFETY: the function has no uses left in the module.
            unsafe { function.delete() };
        }
    }
    let host_calls: Vec<String> = module
        .get_functions()
        .filter(|f| !is_gpu(f) && f.count_basic_blocks() > 0)
        .map(|f| format!("`{}`", f.get_name().to_string_lossy()))
        .collect();
    if !host_calls.is_empty() {
        return Err(refuse(format!(
            "a GPU function calls {}, which is not @target(gpu)",
            host_calls.join(", ")
        )));
    }
    let undefined: Vec<String> = module
        .get_functions()
        .filter(|f| f.count_basic_blocks() == 0 && !f.get_name().to_bytes().starts_with(b"llvm."))
        .map(|f| format!("`{}`", f.get_name().to_string_lossy()))
        .collect();
    if !undefined.is_empty() {
        return Err(refuse(format!(
            "a GPU function needs {}, which nothing on the GPU defines",
            undefined.join(", ")
        )));
    }
    let runtime_globals: Vec<String> = module
        .get_globals()
        .filter(|g| {
            g.as_pointer_value().get_first_use().is_some()
                && RUNTIME_GLOBALS
                    .iter()
                    .any(|name| g.get_name().to_bytes() == name.as_bytes())
        })
        .map(|g| format!("`{}`", g.get_name().to_string_lossy()))
        .collect();
    if !runtime_globals.is_empty() {
        return Err(refuse(format!(
            "a GPU function uses the Codira runtime ({}), which does not exist on the GPU: \
             no garbage-collected values, no calls through the dispatch table",
            runtime_globals.join(", ")
        )));
    }

    for function in module.get_functions() {
        if !is_gpu(&function) {
            continue;
        }
        let name = function.get_name().to_string_lossy().into_owned();
        if kernels.contains(&name) {
            function.set_call_conventions(PTX_KERNEL_CALLING_CONVENTION);
            function.set_linkage(Linkage::External);
        } else {
            function.set_linkage(Linkage::Internal);
        }
    }
    module.set_triple(&code_gen.target_machine.get_triple());
    module.set_data_layout(&code_gen.target_machine.get_target_data().get_data_layout());
    optimize_module(
        &module,
        &code_gen.target_machine,
        code_gen.optimization_level,
    );
    module
        .verify()
        .map_err(|e| refuse(format!("LLVM rejected the kernel module: {e}")))?;

    let ptx = code_gen
        .target_machine
        .write_to_memory_buffer(&module, FileType::Assembly)
        .map_err(|e| refuse(e.to_string()))?;
    let ptx = String::from_utf8_lossy(ptx.as_slice())
        .trim_end_matches('\0')
        .to_owned();
    Ok(Some(GpuKernels { ptx, kernels }))
}

#[cfg(test)]
mod tests {
    use codira_gpu::{Gpu, GpuError, KernelArg};

    use super::GpuKernels;
    use crate::{mock::MockDatabase, CodeGenDatabase};

    fn kernels(source: &str) -> Result<Option<GpuKernels>, String> {
        let (db, file_id) = MockDatabase::with_single_file(source);
        let group = db
            .module_partition()
            .group_for_file(file_id)
            .expect("a module group for the file");
        (*db.gpu_kernels(group)).clone()
    }

    const VECTOR_ADD: &str = r#"
extern "codira-intrinsic" {
    func gpu_thread_id_x() -> u32;
    func gpu_block_id_x() -> u32;
    func gpu_block_dim_x() -> u32;
    func load_f32(addr: usize) -> f32;
    func store_f32(addr: usize, value: f32);
}

@target(gpu)
func element(i: u32) -> usize {
    (i as usize) * 4
}

@target(gpu)
public func vadd(a: usize, b: usize, c: usize, n: u32) {
    let i = gpu_block_id_x() * gpu_block_dim_x() + gpu_thread_id_x()
    if i < n {
        let at = element(i)
        store_f32(c + at, load_f32(a + at) + load_f32(b + at))
    }
}

public func host_only(x: i64) -> i64 { x * 2 }
"#;

    #[test]
    fn a_kernel_compiles_to_ptx() {
        let compiled = kernels(VECTOR_ADD)
            .expect("the kernel is in the subset")
            .expect("the group has GPU functions");
        assert_eq!(compiled.kernels, vec!["vadd".to_owned()]);
        let ptx = &compiled.ptx;
        assert!(ptx.contains(".target sm_75"), "{ptx}");
        assert!(ptx.contains(".entry vadd"), "{ptx}");
        // The thread-geometry intrinsics became PTX special registers.
        assert!(ptx.contains("%tid.x") && ptx.contains("%ctaid.x"), "{ptx}");
        // Host code and the helper's own symbol do not survive as entries.
        assert!(!ptx.contains("host_only"), "{ptx}");
        assert!(!ptx.contains(".entry element"), "{ptx}");
    }

    #[test]
    fn a_group_without_gpu_functions_has_no_kernels() {
        assert_eq!(kernels("public func f() -> i32 { 1 }"), Ok(None));
    }

    #[test]
    fn a_kernel_calling_host_code_is_refused() {
        let error = kernels(
            "func host(x: u32) -> u32 { x + 1 }\n\n@target(gpu)\npublic func k(x: u32) {\n    \
             host(x)\n}\n",
        )
        .unwrap_err();
        assert!(
            error.contains("`host`, which is not @target(gpu)"),
            "{error}"
        );
    }

    #[test]
    fn a_kernel_that_allocates_is_refused() {
        let error = kernels(
            "class Box {\n    var v: i64\n}\n\n@target(gpu)\npublic func k(x: i64) {\n    \
             let b = Box { v: x }\n}\n",
        )
        .unwrap_err();
        assert!(error.contains("does not exist on the GPU"), "{error}");
    }

    #[test]
    fn vector_add_runs_on_the_gpu() {
        let compiled = kernels(VECTOR_ADD).unwrap().unwrap();
        let gpu = match Gpu::open() {
            Ok(gpu) => gpu,
            Err(reason @ (GpuError::NoDriver(_) | GpuError::NoDevice)) => {
                eprintln!("skipped: {reason}");
                return;
            }
            Err(other) => panic!("{other}"),
        };
        let module = gpu
            .load_ptx(&compiled.ptx)
            .expect("the driver accepts the PTX");
        let vadd = module.kernel("vadd").expect("the kernel is in the module");

        let n = 100_000u32;
        let a: Vec<f32> = (0..n).map(|i| i as f32).collect();
        let b: Vec<f32> = (0..n).map(|i| (i as f32) * 0.5).collect();
        let a_dev = gpu.upload(&a).unwrap();
        let b_dev = gpu.upload(&b).unwrap();
        // One element past `n`, to see the bounds check hold.
        let sentinel = vec![-1.0f32; n as usize + 1];
        let c_dev = gpu.upload(&sentinel).unwrap();

        let block = 256;
        let grid = n.div_ceil(block) + 1;
        // SAFETY: the arguments match `vadd(a, b, c: usize, n: u32)`, and each
        // buffer holds at least `n` f32s.
        unsafe {
            gpu.launch(
                &vadd,
                grid,
                block,
                &[a_dev.arg(), b_dev.arg(), c_dev.arg(), KernelArg::U32(n)],
            )
        }
        .expect("the kernel runs");

        let mut c = vec![0.0f32; n as usize + 1];
        c_dev.copy_to(&mut c).unwrap();
        for i in 0..n as usize {
            assert_eq!(c[i], a[i] + b[i], "element {i} on {}", gpu.name());
        }
        assert_eq!(c[n as usize], -1.0, "the kernel wrote past n");
        eprintln!("vector add of {n} elements ran on {}", gpu.name());
    }
}
