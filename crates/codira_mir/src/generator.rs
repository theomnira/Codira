//! Copyright (c) 2026 Omnira CJSC
//!
//! Generators: parametric function/struct templates.
//!
//! Structural analog of `kgen.generator` / `kgen.struct.generator`
//! (`modular/KGEN/docs/MojoCompilerWalkthrough.md` §"Generators: Function
//! and Struct") -- see `spec/EIDOS_ARCHITECTURE.md` §3. Elaboration
//! (in the separate `codira_comptime` crate) consumes a [`Generator`] plus
//! concrete parameter values and produces a monomorphized [`Body`].

use la_arena::{Arena, Idx};
use smol_str::SmolStr;

use crate::op::Body;

/// One named parameter of a generator (`T` or `N` in `SIMD[T, N: usize]`).
///
/// Whether this is a *type* parameter (its use sites expect a type) or a
/// *value* parameter (its use sites expect a concrete constant, like a
/// const-generic array length) is not decided here -- the grammar doesn't
/// distinguish the two either (see `codira_hir::item_tree::GenericParamData`'s
/// doc comment, added alongside this crate). It's a property of how the
/// parameter is *used* in the body, resolved during elaboration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GeneratorParam {
    pub name: SmolStr,
}

/// A parametric function template (KGEN analog: `kgen.generator`).
///
/// After elaboration with concrete parameter values, this becomes a
/// concrete [`Body`] (KGEN analog: `kgen.func`) -- elaboration doesn't
/// change the `Body` type, it just means every `param.ref` op in the body
/// has been folded away by the interpreter (see `codira_comptime`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Generator {
    pub name: SmolStr,
    pub params: Vec<GeneratorParam>,
    pub body: Body,
}

/// A parametric struct template (KGEN analog: `kgen.struct.generator`).
///
/// Field *types* are deliberately not modeled yet (only field names) --
/// doing so honestly requires a type-expression IR shared with
/// `codira_hir::type_ref`, which is out of scope for this session (see
/// `spec/EIDOS_ARCHITECTURE.md` §8, non-goals). This exists now so
/// `GeneratorId`/`GeneratorStore` have a real second case to be generic
/// over, matching KGEN's `GeneratorOpInterface` covering both function and
/// struct generators uniformly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StructGenerator {
    pub name: SmolStr,
    pub params: Vec<GeneratorParam>,
    pub field_names: Vec<SmolStr>,
}

pub type GeneratorId = Idx<Generator>;
pub type StructGeneratorId = Idx<StructGenerator>;

/// Owns every [`Generator`]/[`StructGenerator`] produced while lowering a
/// compilation unit. One store is shared by every generator reference
/// (`param.ref`-style lookups go through here), the same role KGEN's
/// module-level symbol table plays for `kgen.generator` symbols.
#[derive(Debug, Default)]
pub struct GeneratorStore {
    generators: Arena<Generator>,
    struct_generators: Arena<StructGenerator>,
}

impl GeneratorStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_generator(&mut self, generator: Generator) -> GeneratorId {
        self.generators.alloc(generator)
    }

    pub fn add_struct_generator(&mut self, generator: StructGenerator) -> StructGeneratorId {
        self.struct_generators.alloc(generator)
    }

    pub fn generator(&self, id: GeneratorId) -> &Generator {
        &self.generators[id]
    }

    /// Resolves a `core.call` symbol reference (`OpKind::Call`'s callee
    /// name) to its generator, the way KGEN resolves `#kgen.genref`
    /// against the module symbol table. Linear scan: generator counts per
    /// compilation unit are small, and keeping the store index-free means
    /// `add_generator` can never observe a stale name index.
    pub fn generator_by_name(&self, name: &str) -> Option<(GeneratorId, &Generator)> {
        self.generators.iter().find(|(_, g)| g.name == name)
    }

    pub fn iter_generators(&self) -> impl Iterator<Item = (GeneratorId, &Generator)> {
        self.generators.iter()
    }

    pub fn struct_generator(&self, id: StructGeneratorId) -> &StructGenerator {
        &self.struct_generators[id]
    }
}
