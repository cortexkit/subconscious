#![forbid(unsafe_code)]

use std::{
    fs,
    path::{Path, PathBuf},
};
use syn::{visit::Visit, Expr, ExprCall, FnArg, ImplItemFn, ItemImpl, Pat, Type};

#[derive(Debug, PartialEq, Eq)]
enum ViolationKind {
    OutsideTypedHelper,
    UntypedSource,
    SpellingCountChanged,
}

#[derive(Debug)]
struct Violation {
    file: PathBuf,
    kind: ViolationKind,
    message: String,
}

#[derive(Default)]
struct Inventory {
    file: PathBuf,
    in_call_error: bool,
    in_typed_helper: bool,
    constructions: usize,
    violations: Vec<Violation>,
}

impl Inventory {
    fn report(&mut self, kind: ViolationKind, message: impl Into<String>) {
        self.violations.push(Violation {
            file: self.file.clone(),
            kind,
            message: message.into(),
        });
    }
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
                if !self.in_typed_helper {
                    self.report(
                        ViolationKind::OutsideTypedHelper,
                        "OutcomeUnknown must use the cause-attaching helper",
                    );
                }
                let boxes_source = node.args.len() == 1
                    && matches!(node.args.first(), Some(Expr::Call(boxed))
                        if matches!(boxed.func.as_ref(), Expr::Path(path)
                            if path.path.segments.len() == 2
                            && path.path.segments[0].ident == "Box"
                            && path.path.segments[1].ident == "new")
                        && boxed.args.len() == 1
                        && matches!(boxed.args.first(), Some(Expr::Path(path))
                            if path.path.is_ident("source")));
                if !boxes_source {
                    self.report(
                        ViolationKind::UntypedSource,
                        "OutcomeUnknown must box its typed source as Box::new(source), not an untyped replacement",
                    );
                }
            }
        }
        syn::visit::visit_expr_call(self, node);
    }
}

fn inventory_source(file: &Path, source: &str, expected_spellings: usize) -> Inventory {
    let mut inventory = Inventory {
        file: file.to_path_buf(),
        ..Inventory::default()
    };
    // syn leaves macro bodies opaque. Inventory every spelling too, so
    // adding a constructor inside select!, matches!, or another macro
    // cannot bypass the typed-constructor check by escaping the AST walk.
    let compact: String = source.chars().filter(|ch| !ch.is_whitespace()).collect();
    let spellings = compact.matches("OutcomeUnknown(").count();
    if spellings != expected_spellings {
        inventory.report(
            ViolationKind::SpellingCountChanged,
            format!(
                "OutcomeUnknown source inventory changed: expected {expected_spellings} spellings, found {spellings}; review every new site"
            ),
        );
    }
    let parsed = syn::parse_file(source).unwrap();
    inventory.visit_file(&parsed);
    inventory
}

fn scan(directory: &Path, inventory: &mut Inventory) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan(&path, inventory);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path).unwrap();
            let expected = if path.file_name().unwrap() == "consumer.rs" {
                18
            } else {
                0
            };
            let report = inventory_source(&path, &source, expected);
            inventory.constructions += report.constructions;
            inventory.violations.extend(report.violations);
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
    assert!(
        inventory.violations.is_empty(),
        "OutcomeUnknown inventory violations: {:#?}",
        inventory.violations
    );
    assert_eq!(
        inventory.constructions, 1,
        "all construction must pass through one typed helper"
    );
}

#[test]
fn outcome_unknown_inventory_positive_control_reports_planted_violations() {
    let cases = [
        (
            "outside_helper.rs",
            r#"fn bypass() { CallError::OutcomeUnknown(Box::new(SimpleError("lost"))); }"#,
            1,
            vec![
                ViolationKind::OutsideTypedHelper,
                ViolationKind::UntypedSource,
            ],
        ),
        (
            "self_outside_helper.rs",
            r#"impl CallError {
                fn bypass(source: OutcomeUnknownSource) -> Self {
                    Self::OutcomeUnknown(Box::new(source))
                }
            }"#,
            1,
            vec![ViolationKind::OutsideTypedHelper],
        ),
        (
            "macro_body.rs",
            r#"fn bypass() {
                opaque!(CallError::OutcomeUnknown(Box::new(SimpleError("lost"))));
            }"#,
            0,
            vec![ViolationKind::SpellingCountChanged],
        ),
        (
            "untyped_helper.rs",
            r#"impl CallError {
                fn outcome_unknown_source(source: OutcomeUnknownSource) -> Self {
                    Self::OutcomeUnknown(Box::new(SimpleError("lost")))
                }
            }"#,
            1,
            vec![ViolationKind::UntypedSource],
        ),
    ];
    for (file, source, expected_spellings, expected_kinds) in cases {
        let report = inventory_source(Path::new(file), source, expected_spellings);
        assert_eq!(
            report
                .violations
                .iter()
                .map(|v| &v.kind)
                .collect::<Vec<_>>(),
            expected_kinds.iter().collect::<Vec<_>>(),
            "planted violations in {file} must be reported: {:#?}",
            report.violations
        );
        for violation in &report.violations {
            assert_eq!(violation.file, Path::new(file));
            assert!(!violation.message.is_empty());
        }
    }

    let correct_helper = r#"impl CallError {
        fn outcome_unknown_source(source: OutcomeUnknownSource) -> Self {
            Self::OutcomeUnknown(Box::new(source))
        }
    }"#;
    let report = inventory_source(Path::new("correct_helper.rs"), correct_helper, 1);
    assert_eq!(report.constructions, 1);
    assert!(report.violations.is_empty(), "{:#?}", report.violations);
}
