//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_hir_input::WithFixture;
use rustc_hash::FxHashSet;

use crate::{
    db::DefDatabase, ids::ItemDefinitionId, mock::MockDatabase, package_defs::PackageDefs,
    DiagnosticSink, Function, HirDatabase, Module, Package, Struct, TypeAlias,
};

#[test]
fn use_alias() {
    insta::assert_snapshot!(resolve(
        r#"
    //- /foo.code
    public struct Ok;

    //- /bar.code
    public import root.foo.Ok as ReallyOk

    public struct Ok;

    //- /baz.code
    import root.bar.ReallyOk
    "#),
    @"
    mod mod
    +-- mod bar
    |   +-- struct Ok
    |   '-- use struct package::foo::Ok
    +-- mod baz
    |   '-- use struct package::foo::Ok
    '-- mod foo
        '-- struct Ok
    ");
}

#[test]
fn use_duplicate_name() {
    insta::assert_snapshot!(resolve(
        r#"
    //- /foo.code
    public struct Ok;

    //- /bar.code
    import root.foo.Ok

    public struct Ok;
    "#),
    @"
    mod mod
    +-- mod bar
    |   +-- ERROR: 7..18: a second item with the same name imported. Try to use an alias.
    |   +-- ERROR: 20..37: the name `Ok` is defined multiple times
    |   '-- struct Ok
    '-- mod foo
        '-- struct Ok
    ");
}

#[test]
fn use_cyclic_wildcard() {
    insta::assert_snapshot!(resolve(
        r#"
    //- /foo.code
    public import super.baz.*

    public struct Ok;

    //- /baz.code
    public import super.foo.{self, *}
    "#),
    @"
    mod mod
    +-- mod baz
    |   +-- use struct package::foo::Ok
    |   '-- use mod package::foo
    '-- mod foo
        +-- struct Ok
        '-- use mod package::foo
    ");
}

#[test]
fn use_wildcard() {
    insta::assert_snapshot!(resolve(
        r#"
    //- /foo.code
    public struct Foo;

    //- /foo/bar.code
    public import super.Foo
    public struct FooBar;

    //- /bar.code
    import root.foo.bar.*   // Should reference two structs (Foo and FooBar)
    "#),
    @"
    mod mod
    +-- mod bar
    |   +-- use struct package::foo::Foo
    |   '-- use struct package::foo::bar::FooBar
    '-- mod foo
        +-- struct Foo
        '-- mod bar
            +-- struct FooBar
            '-- use struct package::foo::Foo
    ");
}

#[test]
fn use_self() {
    insta::assert_snapshot!(resolve(
        r#"
    //- /foo.code
    public struct Ok;

    //- /bar.code
    import super.foo.{self}
    import foo.Ok
    "#),
    @"
    mod mod
    +-- mod bar
    |   +-- use struct package::foo::Ok
    |   '-- use mod package::foo
    '-- mod foo
        '-- struct Ok
    ");
}

#[test]
fn use_cyclic() {
    insta::assert_snapshot!(resolve(
        r#"
    //- /foo.code
    import super.baz.Cyclic

    public struct Ok;

    //- /bar.code
    import super.foo.{Cyclic, Ok}

    //- /baz.code
    import super.bar.{Cyclic, Ok}
    "#),
    @"
    mod mod
    +-- mod bar
    |   +-- ERROR: 18..24: unresolved import
    |   '-- use struct package::foo::Ok
    +-- mod baz
    |   +-- ERROR: 18..24: unresolved import
    |   '-- ERROR: 26..28: unresolved import
    '-- mod foo
        +-- ERROR: 7..23: unresolved import
        '-- struct Ok
    ");
}

#[test]
fn use_unresolved() {
    insta::assert_snapshot!(resolve(
        r#"
    //- /foo.code
    public struct Foo;

    //- /mod.code
    import foo.Foo   // works
    import foo.Bar   // doesnt work (Bar does not exist)
    import baz.Baz   // doesnt work (baz does not exist)
    "#),
    @"
    mod mod
    +-- ERROR: 33..40: unresolved import
    +-- ERROR: 86..93: unresolved import
    +-- use struct package::foo::Foo
    '-- mod foo
        '-- struct Foo
    ");
}

#[test]
fn use_() {
    insta::assert_snapshot!(resolve(
        r#"
    //- /bar.code
    import root.Foo
    public struct Bar(public Foo);

    //- /mod.code
    public import foo.Foo // Re-export a child's definition

    struct Baz;

    //- /foo.code
    import root.{bar.Bar, Baz}

    public struct Foo {
        baz: Baz, // Can use private definitions from any of its ancestors
    }

    public func foo_from_bar(bar: Bar) -> Foo {
        bar.0
    }
    "#),
    @"
    mod mod
    +-- struct Baz
    +-- use struct package::foo::Foo
    +-- mod bar
    |   +-- struct Bar
    |   '-- use struct package::foo::Foo
    '-- mod foo
        +-- fn foo_from_bar
        +-- struct Foo
        +-- use struct package::Baz
        '-- use struct package::bar::Bar
    ");
}

fn resolve(content: &str) -> String {
    let db = MockDatabase::with_files(content);

    itertools::Itertools::intersperse(
        Package::all(&db).iter().map(|package| {
            let package_defs = db.package_defs(package.id);
            tree_for_module(&db, &package_defs, package.root_module(&db)).to_string()
        }),
        "\n".to_owned(),
    )
    .collect()
}

fn tree_for_module(
    db: &dyn HirDatabase,
    package_defs: &PackageDefs,
    module: Module,
) -> text_trees::StringTreeNode {
    // Construct a tree node
    let mut node = text_trees::StringTreeNode::new(format!(
        "mod {}",
        module
            .name(db)
            .map_or_else(|| "mod".to_owned(), |name| name.clone())
    ));

    // Add module level diagnostics
    let mut diag_sink = DiagnosticSink::new(|diag| {
        node.push(format!(
            "ERROR: {:?}: {}",
            diag.highlight_range(),
            diag.message()
        ));
    });
    module.diagnostics(db, &mut diag_sink);
    drop(diag_sink);

    // Iterate over all declarations and add them as nodes
    let scope = &package_defs[module.id.local_id];
    let local_declarations = scope.declarations().collect::<FxHashSet<_>>();
    let used_declarations = scope
        .entries()
        .filter_map(|entry| entry.1.take_types().map(|(def, _)| def))
        .collect::<Vec<_>>();
    for def in local_declarations.iter().chain(
        used_declarations
            .iter()
            .filter(|decl| !local_declarations.contains(*decl)),
    ) {
        let is_local = local_declarations.contains(def);
        match def {
            ItemDefinitionId::ModuleId(m) => {
                if m.package == module.id.package
                    && !module
                        .children(db)
                        .into_iter()
                        .any(|child_id| child_id.id == *m)
                {
                    let module: Module = (*m).into();
                    node.push(format!(
                        "use mod {}",
                        fully_qualified_module_path(db, module)
                    ));
                }
            }
            ItemDefinitionId::FunctionId(f) => {
                let func: Function = (*f).into();
                let name = func.name(db);
                if is_local {
                    node.push(format!("fn {name}"));
                } else {
                    let fully_qualified_name = format!(
                        "{}::{}",
                        fully_qualified_module_path(db, func.module(db)),
                        name
                    );
                    node.push(format!("use fn {fully_qualified_name}"));
                }
            }
            ItemDefinitionId::StructId(s) => {
                let strukt: Struct = (*s).into();
                let name = strukt.name(db);
                if is_local {
                    node.push(format!("struct {name}"));
                } else {
                    let fully_qualified_name = format!(
                        "{}::{}",
                        fully_qualified_module_path(db, strukt.module(db)),
                        name
                    );
                    node.push(format!("use struct {fully_qualified_name}"));
                }
            }
            ItemDefinitionId::TypeAliasId(alias) => {
                let alias: TypeAlias = (*alias).into();
                let name = alias.name(db);
                if is_local {
                    node.push(format!("type {name}"));
                } else {
                    let fully_qualified_name = format!(
                        "{}::{}",
                        fully_qualified_module_path(db, alias.module(db)),
                        name
                    );
                    node.push(format!("use type {fully_qualified_name}"));
                }
            }
            ItemDefinitionId::PrimitiveType(_) => {}
        }
    }

    // Iterate over all children of this module
    for child_module in module.children(db) {
        node.push_node(tree_for_module(db, package_defs, child_module));
    }

    node
}

/// Returns a fully qualified path of a module e.g. `package::foo::bar::baz`
fn fully_qualified_module_path(db: &dyn HirDatabase, module: Module) -> String {
    itertools::Itertools::intersperse(
        module
            .path_to_root(db)
            .into_iter()
            .map(|m| {
                m.name(db)
                    .map_or_else(|| "package".to_owned(), |name| name.clone())
            })
            .rev(),
        "::".to_string(),
    )
    .collect::<String>()
}
