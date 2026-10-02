#![forbid(unsafe_code)]

use std::{fs, path::Path};
use syn::{visit::Visit, Expr, ExprCall, FnArg, ImplItemFn, ItemImpl, Pat, Type};

#[derive(Default)]
struct Inventory {
    in_call_error: bool,
    in_typed_helper: bool,
    constructions: usize,
}

impl<'ast> Visit<'ast> for Inventory {
    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        let previous = self.in_call_error;
        self.in_call_error = matches!(node.self_ty.as_ref(), Type::Path(ty)
            if ty.path.is_ident("CallError"));
        syn::visit::visit_item_impl(self, node);
        self.in_call_error = previous;
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        let previous = self.in_typed_helper;
        self.in_typed_helper = self.in_call_error
            && node.sig.ident == "outcome_unknown_source"
            && node.sig.inputs.len() == 1
            && matches!(node.sig.inputs.first(), Some(FnArg::Typed(arg))
                if matches!(arg.ty.as_ref(), Type::Path(ty) if ty.path.is_ident("OutcomeUnknownSource"))
                && matches!(arg.pat.as_ref(), Pat::Ident(pat) if pat.ident == "source"));
        syn::visit::visit_impl_item_fn(self, node);
        self.in_typed_helper = previous;
    }

    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let Expr::Path(function) = node.func.as_ref() {
            let segments: Vec<_> = function.path.segments.iter().collect();
            let is_error_constructor = segments.last().is_some_and(|s| s.ident == "OutcomeUnknown")
                && (segments.iter().any(|s| s.ident == "CallError")
                    || (self.in_call_error && segments.iter().any(|s| s.ident == "Self")));
            if is_error_constructor {
                self.constructions += 1;
                assert!(
                    self.in_typed_helper,
                    "OutcomeUnknown must use the cause-attaching helper"
                );
                assert_eq!(node.args.len(), 1);
                let Some(Expr::Call(boxed)) = node.args.first() else {
                    panic!("OutcomeUnknown source must be Box::new(source)");
                };
                assert!(matches!(boxed.func.as_ref(), Expr::Path(path)
                    if path.path.segments.len() == 2
                    && path.path.segments[0].ident == "Box"
                    && path.path.segments[1].ident == "new"));
                assert_eq!(boxed.args.len(), 1);
                assert!(
                    matches!(boxed.args.first(), Some(Expr::Path(path)) if path.path.is_ident("source")),
                    "OutcomeUnknown must box its typed source, not an untyped replacement"
                );
            }
        }
        syn::visit::visit_expr_call(self, node);
    }
}

fn scan(directory: &Path, inventory: &mut Inventory) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan(&path, inventory);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path).unwrap();
            // syn leaves macro bodies opaque. Inventory every spelling too, so
            // adding a constructor inside select!, matches!, or another macro
            // cannot bypass the typed-constructor check by escaping the AST walk.
            let compact: String = source.chars().filter(|ch| !ch.is_whitespace()).collect();
            let expected = if path.file_name().unwrap() == "consumer.rs" {
                18
            } else {
                0
            };
            assert_eq!(
                compact.matches("OutcomeUnknown(").count(),
                expected,
                "OutcomeUnknown source inventory changed in {}: review every new site",
                path.display()
            );
            let parsed = syn::parse_file(&source).unwrap();
            inventory.visit_file(&parsed);
        }
    }
}

#[test]
fn every_outcome_unknown_construction_boxes_a_typed_cause() {
    // Parse source rather than searching lines so matches and enum declarations
    // are not mistaken for constructors, and multiline constructors are covered.
    let mut inventory = Inventory::default();
    scan(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut inventory,
    );
    assert_eq!(
        inventory.constructions, 1,
        "all construction must pass through one typed helper"
    );
}
