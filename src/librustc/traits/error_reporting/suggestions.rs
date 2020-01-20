use super::{
    ArgKind, EvaluationResult, Obligation, ObligationCause, ObligationCauseCode,
    PredicateObligation,
};

use crate::infer::InferCtxt;
use crate::traits::object_safety::object_safety_violations;
use crate::ty::TypeckTables;
use crate::ty::{self, AdtKind, DefIdTree, ToPredicate, Ty, TyCtxt, TypeFoldable};

use rustc_errors::{
    error_code, pluralize, struct_span_err, Applicability, DiagnosticBuilder, Style,
};
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::Visitor;
use rustc_hir::Node;
use rustc_span::source_map::SourceMap;
use rustc_span::symbol::{kw, sym};
use rustc_span::{MultiSpan, Span, DUMMY_SP};
use std::fmt;

use rustc_error_codes::*;

impl<'a, 'tcx> InferCtxt<'a, 'tcx> {
    crate fn suggest_restricting_param_bound(
        &self,
        mut err: &mut DiagnosticBuilder<'_>,
        trait_ref: &ty::PolyTraitRef<'_>,
        body_id: hir::HirId,
    ) {
        let self_ty = trait_ref.self_ty();
        let (param_ty, projection) = match &self_ty.kind {
            ty::Param(_) => (true, None),
            ty::Projection(projection) => (false, Some(projection)),
            _ => return,
        };

        let suggest_restriction =
            |generics: &hir::Generics<'_>, msg, err: &mut DiagnosticBuilder<'_>| {
                let span = generics.where_clause.span_for_predicates_or_empty_place();
                if !span.from_expansion() && span.desugaring_kind().is_none() {
                    err.span_suggestion(
                        generics.where_clause.span_for_predicates_or_empty_place().shrink_to_hi(),
                        &format!("consider further restricting {}", msg),
                        format!(
                            "{} {} ",
                            if !generics.where_clause.predicates.is_empty() {
                                ","
                            } else {
                                " where"
                            },
                            trait_ref.to_predicate(),
                        ),
                        Applicability::MachineApplicable,
                    );
                }
            };

        // FIXME: Add check for trait bound that is already present, particularly `?Sized` so we
        //        don't suggest `T: Sized + ?Sized`.
        let mut hir_id = body_id;
        while let Some(node) = self.tcx.hir().find(hir_id) {
            match node {
                hir::Node::TraitItem(hir::TraitItem {
                    generics,
                    kind: hir::TraitItemKind::Method(..),
                    ..
                }) if param_ty && self_ty == self.tcx.types.self_param => {
                    // Restricting `Self` for a single method.
                    suggest_restriction(&generics, "`Self`", err);
                    return;
                }

                hir::Node::Item(hir::Item { kind: hir::ItemKind::Fn(_, generics, _), .. })
                | hir::Node::TraitItem(hir::TraitItem {
                    generics,
                    kind: hir::TraitItemKind::Method(..),
                    ..
                })
                | hir::Node::ImplItem(hir::ImplItem {
                    generics,
                    kind: hir::ImplItemKind::Method(..),
                    ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::Trait(_, _, generics, _, _),
                    ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::Impl { generics, .. }, ..
                }) if projection.is_some() => {
                    // Missing associated type bound.
                    suggest_restriction(&generics, "the associated type", err);
                    return;
                }

                hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::Struct(_, generics),
                    span,
                    ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::Enum(_, generics), span, ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::Union(_, generics),
                    span,
                    ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::Trait(_, _, generics, ..),
                    span,
                    ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::Impl { generics, .. },
                    span,
                    ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::Fn(_, generics, _),
                    span,
                    ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::TyAlias(_, generics),
                    span,
                    ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::TraitAlias(generics, _),
                    span,
                    ..
                })
                | hir::Node::Item(hir::Item {
                    kind: hir::ItemKind::OpaqueTy(hir::OpaqueTy { generics, .. }),
                    span,
                    ..
                })
                | hir::Node::TraitItem(hir::TraitItem { generics, span, .. })
                | hir::Node::ImplItem(hir::ImplItem { generics, span, .. })
                    if param_ty =>
                {
                    // Missing generic type parameter bound.
                    let param_name = self_ty.to_string();
                    let constraint = trait_ref.print_only_trait_path().to_string();
                    if suggest_constraining_type_param(
                        generics,
                        &mut err,
                        &param_name,
                        &constraint,
                        self.tcx.sess.source_map(),
                        *span,
                    ) {
                        return;
                    }
                }

                hir::Node::Crate => return,

                _ => {}
            }

            hir_id = self.tcx.hir().get_parent_item(hir_id);
        }
    }

    /// When encountering an assignment of an unsized trait, like `let x = ""[..];`, provide a
    /// suggestion to borrow the initializer in order to use have a slice instead.
    crate fn suggest_borrow_on_unsized_slice(
        &self,
        code: &ObligationCauseCode<'tcx>,
        err: &mut DiagnosticBuilder<'tcx>,
    ) {
        if let &ObligationCauseCode::VariableType(hir_id) = code {
            let parent_node = self.tcx.hir().get_parent_node(hir_id);
            if let Some(Node::Local(ref local)) = self.tcx.hir().find(parent_node) {
                if let Some(ref expr) = local.init {
                    if let hir::ExprKind::Index(_, _) = expr.kind {
                        if let Ok(snippet) = self.tcx.sess.source_map().span_to_snippet(expr.span) {
                            err.span_suggestion(
                                expr.span,
                                "consider borrowing here",
                                format!("&{}", snippet),
                                Applicability::MachineApplicable,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Given a closure's `DefId`, return the given name of the closure.
    ///
    /// This doesn't account for reassignments, but it's only used for suggestions.
    crate fn get_closure_name(
        &self,
        def_id: DefId,
        err: &mut DiagnosticBuilder<'_>,
        msg: &str,
    ) -> Option<String> {
        let get_name =
            |err: &mut DiagnosticBuilder<'_>, kind: &hir::PatKind<'_>| -> Option<String> {
                // Get the local name of this closure. This can be inaccurate because
                // of the possibility of reassignment, but this should be good enough.
                match &kind {
                    hir::PatKind::Binding(hir::BindingAnnotation::Unannotated, _, name, None) => {
                        Some(format!("{}", name))
                    }
                    _ => {
                        err.note(&msg);
                        None
                    }
                }
            };

        let hir = self.tcx.hir();
        let hir_id = hir.as_local_hir_id(def_id)?;
        let parent_node = hir.get_parent_node(hir_id);
        match hir.find(parent_node) {
            Some(hir::Node::Stmt(hir::Stmt { kind: hir::StmtKind::Local(local), .. })) => {
                get_name(err, &local.pat.kind)
            }
            // Different to previous arm because one is `&hir::Local` and the other
            // is `P<hir::Local>`.
            Some(hir::Node::Local(local)) => get_name(err, &local.pat.kind),
            _ => return None,
        }
    }

    /// We tried to apply the bound to an `fn` or closure. Check whether calling it would
    /// evaluate to a type that *would* satisfy the trait binding. If it would, suggest calling
    /// it: `bar(foo)` → `bar(foo())`. This case is *very* likely to be hit if `foo` is `async`.
    crate fn suggest_fn_call(
        &self,
        obligation: &PredicateObligation<'tcx>,
        err: &mut DiagnosticBuilder<'_>,
        trait_ref: &ty::Binder<ty::TraitRef<'tcx>>,
        points_at_arg: bool,
    ) {
        let self_ty = trait_ref.self_ty();
        let (def_id, output_ty, callable) = match self_ty.kind {
            ty::Closure(def_id, substs) => {
                (def_id, self.closure_sig(def_id, substs).output(), "closure")
            }
            ty::FnDef(def_id, _) => (def_id, self_ty.fn_sig(self.tcx).output(), "function"),
            _ => return,
        };
        let msg = format!("use parentheses to call the {}", callable);

        let obligation = self.mk_obligation_for_def_id(
            trait_ref.def_id(),
            output_ty.skip_binder(),
            obligation.cause.clone(),
            obligation.param_env,
        );

        match self.evaluate_obligation(&obligation) {
            Ok(EvaluationResult::EvaluatedToOk)
            | Ok(EvaluationResult::EvaluatedToOkModuloRegions)
            | Ok(EvaluationResult::EvaluatedToAmbig) => {}
            _ => return,
        }
        let hir = self.tcx.hir();
        // Get the name of the callable and the arguments to be used in the suggestion.
        let snippet = match hir.get_if_local(def_id) {
            Some(hir::Node::Expr(hir::Expr {
                kind: hir::ExprKind::Closure(_, decl, _, span, ..),
                ..
            })) => {
                err.span_label(*span, "consider calling this closure");
                let name = match self.get_closure_name(def_id, err, &msg) {
                    Some(name) => name,
                    None => return,
                };
                let args = decl.inputs.iter().map(|_| "_").collect::<Vec<_>>().join(", ");
                format!("{}({})", name, args)
            }
            Some(hir::Node::Item(hir::Item {
                ident,
                kind: hir::ItemKind::Fn(.., body_id),
                ..
            })) => {
                err.span_label(ident.span, "consider calling this function");
                let body = hir.body(*body_id);
                let args = body
                    .params
                    .iter()
                    .map(|arg| match &arg.pat.kind {
                        hir::PatKind::Binding(_, _, ident, None)
                        // FIXME: provide a better suggestion when encountering `SelfLower`, it
                        // should suggest a method call.
                        if ident.name != kw::SelfLower => ident.to_string(),
                        _ => "_".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}({})", ident, args)
            }
            _ => return,
        };
        if points_at_arg {
            // When the obligation error has been ensured to have been caused by
            // an argument, the `obligation.cause.span` points at the expression
            // of the argument, so we can provide a suggestion. This is signaled
            // by `points_at_arg`. Otherwise, we give a more general note.
            err.span_suggestion(
                obligation.cause.span,
                &msg,
                snippet,
                Applicability::HasPlaceholders,
            );
        } else {
            err.help(&format!("{}: `{}`", msg, snippet));
        }
    }

    crate fn suggest_add_reference_to_arg(
        &self,
        obligation: &PredicateObligation<'tcx>,
        err: &mut DiagnosticBuilder<'tcx>,
        trait_ref: &ty::Binder<ty::TraitRef<'tcx>>,
        points_at_arg: bool,
        has_custom_message: bool,
    ) -> bool {
        if !points_at_arg {
            return false;
        }

        let span = obligation.cause.span;
        let param_env = obligation.param_env;
        let trait_ref = trait_ref.skip_binder();

        if let ObligationCauseCode::ImplDerivedObligation(obligation) = &obligation.cause.code {
            // Try to apply the original trait binding obligation by borrowing.
            let self_ty = trait_ref.self_ty();
            let found = self_ty.to_string();
            let new_self_ty = self.tcx.mk_imm_ref(self.tcx.lifetimes.re_static, self_ty);
            let substs = self.tcx.mk_substs_trait(new_self_ty, &[]);
            let new_trait_ref = ty::TraitRef::new(obligation.parent_trait_ref.def_id(), substs);
            let new_obligation =
                Obligation::new(ObligationCause::dummy(), param_env, new_trait_ref.to_predicate());
            if self.predicate_must_hold_modulo_regions(&new_obligation) {
                if let Ok(snippet) = self.tcx.sess.source_map().span_to_snippet(span) {
                    // We have a very specific type of error, where just borrowing this argument
                    // might solve the problem. In cases like this, the important part is the
                    // original type obligation, not the last one that failed, which is arbitrary.
                    // Because of this, we modify the error to refer to the original obligation and
                    // return early in the caller.
                    let msg = format!(
                        "the trait bound `{}: {}` is not satisfied",
                        found,
                        obligation.parent_trait_ref.skip_binder().print_only_trait_path(),
                    );
                    if has_custom_message {
                        err.note(&msg);
                    } else {
                        err.message = vec![(msg, Style::NoStyle)];
                    }
                    if snippet.starts_with('&') {
                        // This is already a literal borrow and the obligation is failing
                        // somewhere else in the obligation chain. Do not suggest non-sense.
                        return false;
                    }
                    err.span_label(
                        span,
                        &format!(
                            "expected an implementor of trait `{}`",
                            obligation.parent_trait_ref.skip_binder().print_only_trait_path(),
                        ),
                    );
                    err.span_suggestion(
                        span,
                        "consider borrowing here",
                        format!("&{}", snippet),
                        Applicability::MaybeIncorrect,
                    );
                    return true;
                }
            }
        }
        false
    }

    /// Whenever references are used by mistake, like `for (i, e) in &vec.iter().enumerate()`,
    /// suggest removing these references until we reach a type that implements the trait.
    crate fn suggest_remove_reference(
        &self,
        obligation: &PredicateObligation<'tcx>,
        err: &mut DiagnosticBuilder<'tcx>,
        trait_ref: &ty::Binder<ty::TraitRef<'tcx>>,
    ) {
        let trait_ref = trait_ref.skip_binder();
        let span = obligation.cause.span;

        if let Ok(snippet) = self.tcx.sess.source_map().span_to_snippet(span) {
            let refs_number =
                snippet.chars().filter(|c| !c.is_whitespace()).take_while(|c| *c == '&').count();
            if let Some('\'') =
                snippet.chars().filter(|c| !c.is_whitespace()).skip(refs_number).next()
            {
                // Do not suggest removal of borrow from type arguments.
                return;
            }

            let mut trait_type = trait_ref.self_ty();

            for refs_remaining in 0..refs_number {
                if let ty::Ref(_, t_type, _) = trait_type.kind {
                    trait_type = t_type;

                    let new_obligation = self.mk_obligation_for_def_id(
                        trait_ref.def_id,
                        trait_type,
                        ObligationCause::dummy(),
                        obligation.param_env,
                    );

                    if self.predicate_may_hold(&new_obligation) {
                        let sp = self
                            .tcx
                            .sess
                            .source_map()
                            .span_take_while(span, |c| c.is_whitespace() || *c == '&');

                        let remove_refs = refs_remaining + 1;
                        let format_str =
                            format!("consider removing {} leading `&`-references", remove_refs);

                        err.span_suggestion_short(
                            sp,
                            &format_str,
                            String::new(),
                            Applicability::MachineApplicable,
                        );
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// Check if the trait bound is implemented for a different mutability and note it in the
    /// final error.
    crate fn suggest_change_mut(
        &self,
        obligation: &PredicateObligation<'tcx>,
        err: &mut DiagnosticBuilder<'tcx>,
        trait_ref: &ty::Binder<ty::TraitRef<'tcx>>,
        points_at_arg: bool,
    ) {
        let span = obligation.cause.span;
        if let Ok(snippet) = self.tcx.sess.source_map().span_to_snippet(span) {
            let refs_number =
                snippet.chars().filter(|c| !c.is_whitespace()).take_while(|c| *c == '&').count();
            if let Some('\'') =
                snippet.chars().filter(|c| !c.is_whitespace()).skip(refs_number).next()
            {
                // Do not suggest removal of borrow from type arguments.
                return;
            }
            let trait_ref = self.resolve_vars_if_possible(trait_ref);
            if trait_ref.has_infer_types() {
                // Do not ICE while trying to find if a reborrow would succeed on a trait with
                // unresolved bindings.
                return;
            }

            if let ty::Ref(region, t_type, mutability) = trait_ref.skip_binder().self_ty().kind {
                let trait_type = match mutability {
                    hir::Mutability::Mut => self.tcx.mk_imm_ref(region, t_type),
                    hir::Mutability::Not => self.tcx.mk_mut_ref(region, t_type),
                };

                let new_obligation = self.mk_obligation_for_def_id(
                    trait_ref.skip_binder().def_id,
                    trait_type,
                    ObligationCause::dummy(),
                    obligation.param_env,
                );

                if self.evaluate_obligation_no_overflow(&new_obligation).must_apply_modulo_regions()
                {
                    let sp = self
                        .tcx
                        .sess
                        .source_map()
                        .span_take_while(span, |c| c.is_whitespace() || *c == '&');
                    if points_at_arg && mutability == hir::Mutability::Not && refs_number > 0 {
                        err.span_suggestion(
                            sp,
                            "consider changing this borrow's mutability",
                            "&mut ".to_string(),
                            Applicability::MachineApplicable,
                        );
                    } else {
                        err.note(&format!(
                            "`{}` is implemented for `{:?}`, but not for `{:?}`",
                            trait_ref.print_only_trait_path(),
                            trait_type,
                            trait_ref.skip_binder().self_ty(),
                        ));
                    }
                }
            }
        }
    }

    crate fn suggest_semicolon_removal(
        &self,
        obligation: &PredicateObligation<'tcx>,
        err: &mut DiagnosticBuilder<'tcx>,
        span: Span,
        trait_ref: &ty::Binder<ty::TraitRef<'tcx>>,
    ) {
        let hir = self.tcx.hir();
        let parent_node = hir.get_parent_node(obligation.cause.body_id);
        let node = hir.find(parent_node);
        if let Some(hir::Node::Item(hir::Item {
            kind: hir::ItemKind::Fn(sig, _, body_id), ..
        })) = node
        {
            let body = hir.body(*body_id);
            if let hir::ExprKind::Block(blk, _) = &body.value.kind {
                if sig.decl.output.span().overlaps(span)
                    && blk.expr.is_none()
                    && "()" == &trait_ref.self_ty().to_string()
                {
                    // FIXME(estebank): When encountering a method with a trait
                    // bound not satisfied in the return type with a body that has
                    // no return, suggest removal of semicolon on last statement.
                    // Once that is added, close #54771.
                    if let Some(ref stmt) = blk.stmts.last() {
                        let sp = self.tcx.sess.source_map().end_point(stmt.span);
                        err.span_label(sp, "consider removing this semicolon");
                    }
                }
            }
        }
    }

    /// If all conditions are met to identify a returned `dyn Trait`, suggest using `impl Trait` if
    /// applicable and signal that the error has been expanded appropriately and needs to be
    /// emitted.
    crate fn suggest_impl_trait(
        &self,
        err: &mut DiagnosticBuilder<'tcx>,
        span: Span,
        obligation: &PredicateObligation<'tcx>,
        trait_ref: &ty::Binder<ty::TraitRef<'tcx>>,
    ) -> bool {
        match obligation.cause.code.peel_derives() {
            // Only suggest `impl Trait` if the return type is unsized because it is `dyn Trait`.
            ObligationCauseCode::SizedReturnType => {}
            _ => return false,
        }

        let hir = self.tcx.hir();
        let parent_node = hir.get_parent_node(obligation.cause.body_id);
        let node = hir.find(parent_node);
        let (sig, body_id) = if let Some(hir::Node::Item(hir::Item {
            kind: hir::ItemKind::Fn(sig, _, body_id),
            ..
        })) = node
        {
            (sig, body_id)
        } else {
            return false;
        };
        let body = hir.body(*body_id);
        let trait_ref = self.resolve_vars_if_possible(trait_ref);
        let ty = trait_ref.skip_binder().self_ty();
        let is_object_safe = match ty.kind {
            ty::Dynamic(predicates, _) => {
                // If the `dyn Trait` is not object safe, do not suggest `Box<dyn Trait>`.
                predicates
                    .principal_def_id()
                    .map_or(true, |def_id| object_safety_violations(self.tcx, def_id).is_empty())
            }
            // We only want to suggest `impl Trait` to `dyn Trait`s.
            // For example, `fn foo() -> str` needs to be filtered out.
            _ => return false,
        };

        let ret_ty = if let hir::FunctionRetTy::Return(ret_ty) = sig.decl.output {
            ret_ty
        } else {
            return false;
        };

        // Use `TypeVisitor` instead of the output type directly to find the span of `ty` for
        // cases like `fn foo() -> (dyn Trait, i32) {}`.
        // Recursively look for `TraitObject` types and if there's only one, use that span to
        // suggest `impl Trait`.

        // Visit to make sure there's a single `return` type to suggest `impl Trait`,
        // otherwise suggest using `Box<dyn Trait>` or an enum.
        let mut visitor = ReturnsVisitor(vec![]);
        visitor.visit_body(&body);

        let tables = self.in_progress_tables.map(|t| t.borrow()).unwrap();

        let mut ret_types = visitor.0.iter().filter_map(|expr| tables.node_type_opt(expr.hir_id));
        let (last_ty, all_returns_have_same_type) =
            ret_types.clone().fold((None, true), |(last_ty, mut same), returned_ty| {
                same &= last_ty.map_or(true, |ty| ty == returned_ty);
                (Some(returned_ty), same)
            });
        let all_returns_conform_to_trait =
            if let Some(ty_ret_ty) = tables.node_type_opt(ret_ty.hir_id) {
                match ty_ret_ty.kind {
                    ty::Dynamic(predicates, _) => {
                        let cause = ObligationCause::misc(ret_ty.span, ret_ty.hir_id);
                        let param_env = ty::ParamEnv::empty();
                        ret_types.all(|returned_ty| {
                            predicates.iter().all(|predicate| {
                                let pred = predicate.with_self_ty(self.tcx, returned_ty);
                                let obl = Obligation::new(cause.clone(), param_env, pred);
                                self.predicate_may_hold(&obl)
                            })
                        })
                    }
                    _ => true,
                }
            } else {
                true
            };

        let (snippet, last_ty) =
            if let (true, hir::TyKind::TraitObject(..), Ok(snippet), true, Some(last_ty)) = (
                // Verify that we're dealing with a return `dyn Trait`
                ret_ty.span.overlaps(span),
                &ret_ty.kind,
                self.tcx.sess.source_map().span_to_snippet(ret_ty.span),
                // If any of the return types does not conform to the trait, then we can't
                // suggest `impl Trait` nor trait objects, it is a type mismatch error.
                all_returns_conform_to_trait,
                last_ty,
            ) {
                (snippet, last_ty)
            } else {
                return false;
            };
        err.code(error_code!(E0746));
        err.set_primary_message("return type cannot have an unboxed trait object");
        err.children.clear();
        let impl_trait_msg = "for information on `impl Trait`, see \
            <https://doc.rust-lang.org/book/ch10-02-traits.html\
            #returning-types-that-implement-traits>";
        let trait_obj_msg = "for information on trait objects, see \
            <https://doc.rust-lang.org/book/ch17-02-trait-objects.html\
            #using-trait-objects-that-allow-for-values-of-different-types>";
        let has_dyn = snippet.split_whitespace().next().map_or(false, |s| s == "dyn");
        let trait_obj = if has_dyn { &snippet[4..] } else { &snippet[..] };
        if all_returns_have_same_type {
            // Suggest `-> impl Trait`.
            err.span_suggestion(
                ret_ty.span,
                &format!(
                    "return `impl {1}` instead, as all return paths are of type `{}`, \
                        which implements `{1}`",
                    last_ty, trait_obj,
                ),
                format!("impl {}", trait_obj),
                Applicability::MachineApplicable,
            );
            err.note(impl_trait_msg);
        } else {
            if is_object_safe {
                // Suggest `-> Box<dyn Trait>` and `Box::new(returned_value)`.
                // Get all the return values and collect their span and suggestion.
                let mut suggestions = visitor
                    .0
                    .iter()
                    .map(|expr| {
                        (
                            expr.span,
                            format!(
                                "Box::new({})",
                                self.tcx.sess.source_map().span_to_snippet(expr.span).unwrap()
                            ),
                        )
                    })
                    .collect::<Vec<_>>();
                // Add the suggestion for the return type.
                suggestions.push((
                    ret_ty.span,
                    format!("Box<{}{}>", if has_dyn { "" } else { "dyn " }, snippet),
                ));
                err.multipart_suggestion(
                    "return a boxed trait object instead",
                    suggestions,
                    Applicability::MaybeIncorrect,
                );
            } else {
                // This is currently not possible to trigger because E0038 takes precedence, but
                // leave it in for completeness in case anything changes in an earlier stage.
                err.note(&format!(
                    "if trait `{}` was object safe, you could return a trait object",
                    trait_obj,
                ));
            }
            err.note(trait_obj_msg);
            err.note(&format!(
                "if all the returned values were of the same type you could use \
                    `impl {}` as the return type",
                trait_obj,
            ));
            err.note(impl_trait_msg);
            err.note("you can create a new `enum` with a variant for each returned type");
        }
        true
    }

    crate fn point_at_returns_when_relevant(
        &self,
        err: &mut DiagnosticBuilder<'tcx>,
        obligation: &PredicateObligation<'tcx>,
    ) {
        match obligation.cause.code.peel_derives() {
            ObligationCauseCode::SizedReturnType => {}
            _ => return,
        }

        let hir = self.tcx.hir();
        let parent_node = hir.get_parent_node(obligation.cause.body_id);
        let node = hir.find(parent_node);
        if let Some(hir::Node::Item(hir::Item { kind: hir::ItemKind::Fn(_, _, body_id), .. })) =
            node
        {
            let body = hir.body(*body_id);
            // Point at all the `return`s in the function as they have failed trait bounds.
            let mut visitor = ReturnsVisitor(vec![]);
            visitor.visit_body(&body);
            let tables = self.in_progress_tables.map(|t| t.borrow()).unwrap();
            for expr in &visitor.0 {
                if let Some(returned_ty) = tables.node_type_opt(expr.hir_id) {
                    let ty = self.resolve_vars_if_possible(&returned_ty);
                    err.span_label(expr.span, &format!("this returned value is of type `{}`", ty));
                }
            }
        }
    }

    /// Given some node representing a fn-like thing in the HIR map,
    /// returns a span and `ArgKind` information that describes the
    /// arguments it expects. This can be supplied to
    /// `report_arg_count_mismatch`.
    pub fn get_fn_like_arguments(&self, node: Node<'_>) -> (Span, Vec<ArgKind>) {
        match node {
            Node::Expr(&hir::Expr {
                kind: hir::ExprKind::Closure(_, ref _decl, id, span, _),
                ..
            }) => (
                self.tcx.sess.source_map().def_span(span),
                self.tcx
                    .hir()
                    .body(id)
                    .params
                    .iter()
                    .map(|arg| {
                        if let hir::Pat { kind: hir::PatKind::Tuple(ref args, _), span, .. } =
                            *arg.pat
                        {
                            ArgKind::Tuple(
                                Some(span),
                                args.iter()
                                    .map(|pat| {
                                        let snippet = self
                                            .tcx
                                            .sess
                                            .source_map()
                                            .span_to_snippet(pat.span)
                                            .unwrap();
                                        (snippet, "_".to_owned())
                                    })
                                    .collect::<Vec<_>>(),
                            )
                        } else {
                            let name =
                                self.tcx.sess.source_map().span_to_snippet(arg.pat.span).unwrap();
                            ArgKind::Arg(name, "_".to_owned())
                        }
                    })
                    .collect::<Vec<ArgKind>>(),
            ),
            Node::Item(&hir::Item { span, kind: hir::ItemKind::Fn(ref sig, ..), .. })
            | Node::ImplItem(&hir::ImplItem {
                span,
                kind: hir::ImplItemKind::Method(ref sig, _),
                ..
            })
            | Node::TraitItem(&hir::TraitItem {
                span,
                kind: hir::TraitItemKind::Method(ref sig, _),
                ..
            }) => (
                self.tcx.sess.source_map().def_span(span),
                sig.decl
                    .inputs
                    .iter()
                    .map(|arg| match arg.clone().kind {
                        hir::TyKind::Tup(ref tys) => ArgKind::Tuple(
                            Some(arg.span),
                            vec![("_".to_owned(), "_".to_owned()); tys.len()],
                        ),
                        _ => ArgKind::empty(),
                    })
                    .collect::<Vec<ArgKind>>(),
            ),
            Node::Ctor(ref variant_data) => {
                let span = variant_data
                    .ctor_hir_id()
                    .map(|hir_id| self.tcx.hir().span(hir_id))
                    .unwrap_or(DUMMY_SP);
                let span = self.tcx.sess.source_map().def_span(span);

                (span, vec![ArgKind::empty(); variant_data.fields().len()])
            }
            _ => panic!("non-FnLike node found: {:?}", node),
        }
    }

    /// Reports an error when the number of arguments needed by a
    /// trait match doesn't match the number that the expression
    /// provides.
    pub fn report_arg_count_mismatch(
        &self,
        span: Span,
        found_span: Option<Span>,
        expected_args: Vec<ArgKind>,
        found_args: Vec<ArgKind>,
        is_closure: bool,
    ) -> DiagnosticBuilder<'tcx> {
        let kind = if is_closure { "closure" } else { "function" };

        let args_str = |arguments: &[ArgKind], other: &[ArgKind]| {
            let arg_length = arguments.len();
            let distinct = match &other[..] {
                &[ArgKind::Tuple(..)] => true,
                _ => false,
            };
            match (arg_length, arguments.get(0)) {
                (1, Some(&ArgKind::Tuple(_, ref fields))) => {
                    format!("a single {}-tuple as argument", fields.len())
                }
                _ => format!(
                    "{} {}argument{}",
                    arg_length,
                    if distinct && arg_length > 1 { "distinct " } else { "" },
                    pluralize!(arg_length)
                ),
            }
        };

        let expected_str = args_str(&expected_args, &found_args);
        let found_str = args_str(&found_args, &expected_args);

        let mut err = struct_span_err!(
            self.tcx.sess,
            span,
            E0593,
            "{} is expected to take {}, but it takes {}",
            kind,
            expected_str,
            found_str,
        );

        err.span_label(span, format!("expected {} that takes {}", kind, expected_str));

        if let Some(found_span) = found_span {
            err.span_label(found_span, format!("takes {}", found_str));

            // move |_| { ... }
            // ^^^^^^^^-- def_span
            //
            // move |_| { ... }
            // ^^^^^-- prefix
            let prefix_span = self.tcx.sess.source_map().span_until_non_whitespace(found_span);
            // move |_| { ... }
            //      ^^^-- pipe_span
            let pipe_span =
                if let Some(span) = found_span.trim_start(prefix_span) { span } else { found_span };

            // Suggest to take and ignore the arguments with expected_args_length `_`s if
            // found arguments is empty (assume the user just wants to ignore args in this case).
            // For example, if `expected_args_length` is 2, suggest `|_, _|`.
            if found_args.is_empty() && is_closure {
                let underscores = vec!["_"; expected_args.len()].join(", ");
                err.span_suggestion(
                    pipe_span,
                    &format!(
                        "consider changing the closure to take and ignore the expected argument{}",
                        if expected_args.len() < 2 { "" } else { "s" }
                    ),
                    format!("|{}|", underscores),
                    Applicability::MachineApplicable,
                );
            }

            if let &[ArgKind::Tuple(_, ref fields)] = &found_args[..] {
                if fields.len() == expected_args.len() {
                    let sugg = fields
                        .iter()
                        .map(|(name, _)| name.to_owned())
                        .collect::<Vec<String>>()
                        .join(", ");
                    err.span_suggestion(
                        found_span,
                        "change the closure to take multiple arguments instead of a single tuple",
                        format!("|{}|", sugg),
                        Applicability::MachineApplicable,
                    );
                }
            }
            if let &[ArgKind::Tuple(_, ref fields)] = &expected_args[..] {
                if fields.len() == found_args.len() && is_closure {
                    let sugg = format!(
                        "|({}){}|",
                        found_args
                            .iter()
                            .map(|arg| match arg {
                                ArgKind::Arg(name, _) => name.to_owned(),
                                _ => "_".to_owned(),
                            })
                            .collect::<Vec<String>>()
                            .join(", "),
                        // add type annotations if available
                        if found_args.iter().any(|arg| match arg {
                            ArgKind::Arg(_, ty) => ty != "_",
                            _ => false,
                        }) {
                            format!(
                                ": ({})",
                                fields
                                    .iter()
                                    .map(|(_, ty)| ty.to_owned())
                                    .collect::<Vec<String>>()
                                    .join(", ")
                            )
                        } else {
                            String::new()
                        },
                    );
                    err.span_suggestion(
                        found_span,
                        "change the closure to accept a tuple instead of individual arguments",
                        sugg,
                        Applicability::MachineApplicable,
                    );
                }
            }
        }

        err
    }

    crate fn report_closure_arg_mismatch(
        &self,
        span: Span,
        found_span: Option<Span>,
        expected_ref: ty::PolyTraitRef<'tcx>,
        found: ty::PolyTraitRef<'tcx>,
    ) -> DiagnosticBuilder<'tcx> {
        crate fn build_fn_sig_string<'tcx>(
            tcx: TyCtxt<'tcx>,
            trait_ref: &ty::TraitRef<'tcx>,
        ) -> String {
            let inputs = trait_ref.substs.type_at(1);
            let sig = if let ty::Tuple(inputs) = inputs.kind {
                tcx.mk_fn_sig(
                    inputs.iter().map(|k| k.expect_ty()),
                    tcx.mk_ty_infer(ty::TyVar(ty::TyVid { index: 0 })),
                    false,
                    hir::Unsafety::Normal,
                    ::rustc_target::spec::abi::Abi::Rust,
                )
            } else {
                tcx.mk_fn_sig(
                    ::std::iter::once(inputs),
                    tcx.mk_ty_infer(ty::TyVar(ty::TyVid { index: 0 })),
                    false,
                    hir::Unsafety::Normal,
                    ::rustc_target::spec::abi::Abi::Rust,
                )
            };
            ty::Binder::bind(sig).to_string()
        }

        let argument_is_closure = expected_ref.skip_binder().substs.type_at(0).is_closure();
        let mut err = struct_span_err!(
            self.tcx.sess,
            span,
            E0631,
            "type mismatch in {} arguments",
            if argument_is_closure { "closure" } else { "function" }
        );

        let found_str = format!(
            "expected signature of `{}`",
            build_fn_sig_string(self.tcx, found.skip_binder())
        );
        err.span_label(span, found_str);

        let found_span = found_span.unwrap_or(span);
        let expected_str = format!(
            "found signature of `{}`",
            build_fn_sig_string(self.tcx, expected_ref.skip_binder())
        );
        err.span_label(found_span, expected_str);

        err
    }
}

impl<'a, 'tcx> InferCtxt<'a, 'tcx> {
    crate fn suggest_fully_qualified_path(
        &self,
        err: &mut DiagnosticBuilder<'_>,
        def_id: DefId,
        span: Span,
        trait_ref: DefId,
    ) {
        if let Some(assoc_item) = self.tcx.opt_associated_item(def_id) {
            if let ty::AssocKind::Const | ty::AssocKind::Type = assoc_item.kind {
                err.note(&format!(
                    "{}s cannot be accessed directly on a `trait`, they can only be \
                        accessed through a specific `impl`",
                    assoc_item.kind.suggestion_descr(),
                ));
                err.span_suggestion(
                    span,
                    "use the fully qualified path to an implementation",
                    format!("<Type as {}>::{}", self.tcx.def_path_str(trait_ref), assoc_item.ident),
                    Applicability::HasPlaceholders,
                );
            }
        }
    }

    /// Adds an async-await specific note to the diagnostic when the future does not implement
    /// an auto trait because of a captured type.
    ///
    /// ```ignore (diagnostic)
    /// note: future does not implement `Qux` as this value is used across an await
    ///   --> $DIR/issue-64130-3-other.rs:17:5
    ///    |
    /// LL |     let x = Foo;
    ///    |         - has type `Foo`
    /// LL |     baz().await;
    ///    |     ^^^^^^^^^^^ await occurs here, with `x` maybe used later
    /// LL | }
    ///    | - `x` is later dropped here
    /// ```
    ///
    /// When the diagnostic does not implement `Send` or `Sync` specifically, then the diagnostic
    /// is "replaced" with a different message and a more specific error.
    ///
    /// ```ignore (diagnostic)
    /// error: future cannot be sent between threads safely
    ///   --> $DIR/issue-64130-2-send.rs:21:5
    ///    |
    /// LL | fn is_send<T: Send>(t: T) { }
    ///    |    -------    ---- required by this bound in `is_send`
    /// ...
    /// LL |     is_send(bar());
    ///    |     ^^^^^^^ future returned by `bar` is not send
    ///    |
    ///    = help: within `impl std::future::Future`, the trait `std::marker::Send` is not
    ///            implemented for `Foo`
    /// note: future is not send as this value is used across an await
    ///   --> $DIR/issue-64130-2-send.rs:15:5
    ///    |
    /// LL |     let x = Foo;
    ///    |         - has type `Foo`
    /// LL |     baz().await;
    ///    |     ^^^^^^^^^^^ await occurs here, with `x` maybe used later
    /// LL | }
    ///    | - `x` is later dropped here
    /// ```
    ///
    /// Returns `true` if an async-await specific note was added to the diagnostic.
    crate fn maybe_note_obligation_cause_for_async_await(
        &self,
        err: &mut DiagnosticBuilder<'_>,
        obligation: &PredicateObligation<'tcx>,
    ) -> bool {
        debug!(
            "maybe_note_obligation_cause_for_async_await: obligation.predicate={:?} \
                obligation.cause.span={:?}",
            obligation.predicate, obligation.cause.span
        );
        let source_map = self.tcx.sess.source_map();

        // Attempt to detect an async-await error by looking at the obligation causes, looking
        // for a generator to be present.
        //
        // When a future does not implement a trait because of a captured type in one of the
        // generators somewhere in the call stack, then the result is a chain of obligations.
        //
        // Given a `async fn` A that calls a `async fn` B which captures a non-send type and that
        // future is passed as an argument to a function C which requires a `Send` type, then the
        // chain looks something like this:
        //
        // - `BuiltinDerivedObligation` with a generator witness (B)
        // - `BuiltinDerivedObligation` with a generator (B)
        // - `BuiltinDerivedObligation` with `std::future::GenFuture` (B)
        // - `BuiltinDerivedObligation` with `impl std::future::Future` (B)
        // - `BuiltinDerivedObligation` with `impl std::future::Future` (B)
        // - `BuiltinDerivedObligation` with a generator witness (A)
        // - `BuiltinDerivedObligation` with a generator (A)
        // - `BuiltinDerivedObligation` with `std::future::GenFuture` (A)
        // - `BuiltinDerivedObligation` with `impl std::future::Future` (A)
        // - `BuiltinDerivedObligation` with `impl std::future::Future` (A)
        // - `BindingObligation` with `impl_send (Send requirement)
        //
        // The first obligation in the chain is the most useful and has the generator that captured
        // the type. The last generator has information about where the bound was introduced. At
        // least one generator should be present for this diagnostic to be modified.
        let (mut trait_ref, mut target_ty) = match obligation.predicate {
            ty::Predicate::Trait(p) => {
                (Some(p.skip_binder().trait_ref), Some(p.skip_binder().self_ty()))
            }
            _ => (None, None),
        };
        let mut generator = None;
        let mut last_generator = None;
        let mut next_code = Some(&obligation.cause.code);
        while let Some(code) = next_code {
            debug!("maybe_note_obligation_cause_for_async_await: code={:?}", code);
            match code {
                ObligationCauseCode::BuiltinDerivedObligation(derived_obligation)
                | ObligationCauseCode::ImplDerivedObligation(derived_obligation) => {
                    let ty = derived_obligation.parent_trait_ref.self_ty();
                    debug!(
                        "maybe_note_obligation_cause_for_async_await: \
                            parent_trait_ref={:?} self_ty.kind={:?}",
                        derived_obligation.parent_trait_ref, ty.kind
                    );

                    match ty.kind {
                        ty::Generator(did, ..) => {
                            generator = generator.or(Some(did));
                            last_generator = Some(did);
                        }
                        ty::GeneratorWitness(..) => {}
                        _ if generator.is_none() => {
                            trait_ref = Some(*derived_obligation.parent_trait_ref.skip_binder());
                            target_ty = Some(ty);
                        }
                        _ => {}
                    }

                    next_code = Some(derived_obligation.parent_code.as_ref());
                }
                _ => break,
            }
        }

        // Only continue if a generator was found.
        debug!(
            "maybe_note_obligation_cause_for_async_await: generator={:?} trait_ref={:?} \
                target_ty={:?}",
            generator, trait_ref, target_ty
        );
        let (generator_did, trait_ref, target_ty) = match (generator, trait_ref, target_ty) {
            (Some(generator_did), Some(trait_ref), Some(target_ty)) => {
                (generator_did, trait_ref, target_ty)
            }
            _ => return false,
        };

        let span = self.tcx.def_span(generator_did);

        // Do not ICE on closure typeck (#66868).
        if self.tcx.hir().as_local_hir_id(generator_did).is_none() {
            return false;
        }

        // Get the tables from the infcx if the generator is the function we are
        // currently type-checking; otherwise, get them by performing a query.
        // This is needed to avoid cycles.
        let in_progress_tables = self.in_progress_tables.map(|t| t.borrow());
        let generator_did_root = self.tcx.closure_base_def_id(generator_did);
        debug!(
            "maybe_note_obligation_cause_for_async_await: generator_did={:?} \
             generator_did_root={:?} in_progress_tables.local_id_root={:?} span={:?}",
            generator_did,
            generator_did_root,
            in_progress_tables.as_ref().map(|t| t.local_id_root),
            span
        );
        let query_tables;
        let tables: &TypeckTables<'tcx> = match &in_progress_tables {
            Some(t) if t.local_id_root == Some(generator_did_root) => t,
            _ => {
                query_tables = self.tcx.typeck_tables_of(generator_did);
                &query_tables
            }
        };

        // Look for a type inside the generator interior that matches the target type to get
        // a span.
        let target_ty_erased = self.tcx.erase_regions(&target_ty);
        let target_span = tables
            .generator_interior_types
            .iter()
            .find(|ty::GeneratorInteriorTypeCause { ty, .. }| {
                // Careful: the regions for types that appear in the
                // generator interior are not generally known, so we
                // want to erase them when comparing (and anyway,
                // `Send` and other bounds are generally unaffected by
                // the choice of region).  When erasing regions, we
                // also have to erase late-bound regions. This is
                // because the types that appear in the generator
                // interior generally contain "bound regions" to
                // represent regions that are part of the suspended
                // generator frame. Bound regions are preserved by
                // `erase_regions` and so we must also call
                // `erase_late_bound_regions`.
                let ty_erased = self.tcx.erase_late_bound_regions(&ty::Binder::bind(*ty));
                let ty_erased = self.tcx.erase_regions(&ty_erased);
                let eq = ty::TyS::same_type(ty_erased, target_ty_erased);
                debug!(
                    "maybe_note_obligation_cause_for_async_await: ty_erased={:?} \
                        target_ty_erased={:?} eq={:?}",
                    ty_erased, target_ty_erased, eq
                );
                eq
            })
            .map(|ty::GeneratorInteriorTypeCause { span, scope_span, expr, .. }| {
                (span, source_map.span_to_snippet(*span), scope_span, expr)
            });

        debug!(
            "maybe_note_obligation_cause_for_async_await: target_ty={:?} \
                generator_interior_types={:?} target_span={:?}",
            target_ty, tables.generator_interior_types, target_span
        );
        if let Some((target_span, Ok(snippet), scope_span, expr)) = target_span {
            self.note_obligation_cause_for_async_await(
                err,
                *target_span,
                scope_span,
                *expr,
                snippet,
                generator_did,
                last_generator,
                trait_ref,
                target_ty,
                tables,
                obligation,
                next_code,
            );
            true
        } else {
            false
        }
    }

    /// Unconditionally adds the diagnostic note described in
    /// `maybe_note_obligation_cause_for_async_await`'s documentation comment.
    crate fn note_obligation_cause_for_async_await(
        &self,
        err: &mut DiagnosticBuilder<'_>,
        target_span: Span,
        scope_span: &Option<Span>,
        expr: Option<hir::HirId>,
        snippet: String,
        first_generator: DefId,
        last_generator: Option<DefId>,
        trait_ref: ty::TraitRef<'_>,
        target_ty: Ty<'tcx>,
        tables: &ty::TypeckTables<'_>,
        obligation: &PredicateObligation<'tcx>,
        next_code: Option<&ObligationCauseCode<'tcx>>,
    ) {
        let source_map = self.tcx.sess.source_map();

        let is_async_fn = self
            .tcx
            .parent(first_generator)
            .map(|parent_did| self.tcx.asyncness(parent_did))
            .map(|parent_asyncness| parent_asyncness == hir::IsAsync::Async)
            .unwrap_or(false);
        let is_async_move = self
            .tcx
            .hir()
            .as_local_hir_id(first_generator)
            .and_then(|hir_id| self.tcx.hir().maybe_body_owned_by(hir_id))
            .map(|body_id| self.tcx.hir().body(body_id))
            .and_then(|body| body.generator_kind())
            .map(|generator_kind| match generator_kind {
                hir::GeneratorKind::Async(..) => true,
                _ => false,
            })
            .unwrap_or(false);
        let await_or_yield = if is_async_fn || is_async_move { "await" } else { "yield" };

        // Special case the primary error message when send or sync is the trait that was
        // not implemented.
        let is_send = self.tcx.is_diagnostic_item(sym::send_trait, trait_ref.def_id);
        let is_sync = self.tcx.is_diagnostic_item(sym::sync_trait, trait_ref.def_id);
        let hir = self.tcx.hir();
        let trait_explanation = if is_send || is_sync {
            let (trait_name, trait_verb) =
                if is_send { ("`Send`", "sent") } else { ("`Sync`", "shared") };

            err.clear_code();
            err.set_primary_message(format!(
                "future cannot be {} between threads safely",
                trait_verb
            ));

            let original_span = err.span.primary_span().unwrap();
            let mut span = MultiSpan::from_span(original_span);

            let message = if let Some(name) = last_generator
                .and_then(|generator_did| self.tcx.parent(generator_did))
                .and_then(|parent_did| hir.as_local_hir_id(parent_did))
                .and_then(|parent_hir_id| hir.opt_name(parent_hir_id))
            {
                format!("future returned by `{}` is not {}", name, trait_name)
            } else {
                format!("future is not {}", trait_name)
            };

            span.push_span_label(original_span, message);
            err.set_span(span);

            format!("is not {}", trait_name)
        } else {
            format!("does not implement `{}`", trait_ref.print_only_trait_path())
        };

        // Look at the last interior type to get a span for the `.await`.
        let await_span = tables.generator_interior_types.iter().map(|t| t.span).last().unwrap();
        let mut span = MultiSpan::from_span(await_span);
        span.push_span_label(
            await_span,
            format!("{} occurs here, with `{}` maybe used later", await_or_yield, snippet),
        );

        span.push_span_label(target_span, format!("has type `{}`", target_ty));

        // If available, use the scope span to annotate the drop location.
        if let Some(scope_span) = scope_span {
            span.push_span_label(
                source_map.end_point(*scope_span),
                format!("`{}` is later dropped here", snippet),
            );
        }

        err.span_note(
            span,
            &format!(
                "future {} as this value is used across an {}",
                trait_explanation, await_or_yield,
            ),
        );

        if let Some(expr_id) = expr {
            let expr = hir.expect_expr(expr_id);
            let is_ref = tables.expr_adjustments(expr).iter().any(|adj| adj.is_region_borrow());
            let parent = hir.get_parent_node(expr_id);
            if let Some(hir::Node::Expr(e)) = hir.find(parent) {
                let method_span = hir.span(parent);
                if tables.is_method_call(e) && is_ref {
                    err.span_help(
                        method_span,
                        "consider moving this method call into a `let` \
                        binding to create a shorter lived borrow",
                    );
                }
            }
        }

        // Add a note for the item obligation that remains - normally a note pointing to the
        // bound that introduced the obligation (e.g. `T: Send`).
        debug!("note_obligation_cause_for_async_await: next_code={:?}", next_code);
        self.note_obligation_cause_code(
            err,
            &obligation.predicate,
            next_code.unwrap(),
            &mut Vec::new(),
        );
    }

    crate fn note_obligation_cause_code<T>(
        &self,
        err: &mut DiagnosticBuilder<'_>,
        predicate: &T,
        cause_code: &ObligationCauseCode<'tcx>,
        obligated_types: &mut Vec<&ty::TyS<'tcx>>,
    ) where
        T: fmt::Display,
    {
        let tcx = self.tcx;
        match *cause_code {
            ObligationCauseCode::ExprAssignable
            | ObligationCauseCode::MatchExpressionArm { .. }
            | ObligationCauseCode::Pattern { .. }
            | ObligationCauseCode::IfExpression { .. }
            | ObligationCauseCode::IfExpressionWithNoElse
            | ObligationCauseCode::MainFunctionType
            | ObligationCauseCode::StartFunctionType
            | ObligationCauseCode::IntrinsicType
            | ObligationCauseCode::MethodReceiver
            | ObligationCauseCode::ReturnNoExpression
            | ObligationCauseCode::MiscObligation => {}
            ObligationCauseCode::SliceOrArrayElem => {
                err.note("slice and array elements must have `Sized` type");
            }
            ObligationCauseCode::TupleElem => {
                err.note("only the last element of a tuple may have a dynamically sized type");
            }
            ObligationCauseCode::ProjectionWf(data) => {
                err.note(&format!("required so that the projection `{}` is well-formed", data,));
            }
            ObligationCauseCode::ReferenceOutlivesReferent(ref_ty) => {
                err.note(&format!(
                    "required so that reference `{}` does not outlive its referent",
                    ref_ty,
                ));
            }
            ObligationCauseCode::ObjectTypeBound(object_ty, region) => {
                err.note(&format!(
                    "required so that the lifetime bound of `{}` for `{}` is satisfied",
                    region, object_ty,
                ));
            }
            ObligationCauseCode::ItemObligation(item_def_id) => {
                let item_name = tcx.def_path_str(item_def_id);
                let msg = format!("required by `{}`", item_name);

                if let Some(sp) = tcx.hir().span_if_local(item_def_id) {
                    let sp = tcx.sess.source_map().def_span(sp);
                    err.span_label(sp, &msg);
                } else {
                    err.note(&msg);
                }
            }
            ObligationCauseCode::BindingObligation(item_def_id, span) => {
                let item_name = tcx.def_path_str(item_def_id);
                let msg = format!("required by this bound in `{}`", item_name);
                if let Some(ident) = tcx.opt_item_name(item_def_id) {
                    err.span_label(ident.span, "");
                }
                if span != DUMMY_SP {
                    err.span_label(span, &msg);
                } else {
                    err.note(&msg);
                }
            }
            ObligationCauseCode::ObjectCastObligation(object_ty) => {
                err.note(&format!(
                    "required for the cast to the object type `{}`",
                    self.ty_to_string(object_ty)
                ));
            }
            ObligationCauseCode::Coercion { source: _, target } => {
                err.note(&format!("required by cast to type `{}`", self.ty_to_string(target)));
            }
            ObligationCauseCode::RepeatVec(suggest_const_in_array_repeat_expressions) => {
                err.note(
                    "the `Copy` trait is required because the repeated element will be copied",
                );
                if suggest_const_in_array_repeat_expressions {
                    err.note(
                        "this array initializer can be evaluated at compile-time, for more \
                         information, see issue \
                         https://github.com/rust-lang/rust/issues/49147",
                    );
                    if tcx.sess.opts.unstable_features.is_nightly_build() {
                        err.help(
                            "add `#![feature(const_in_array_repeat_expressions)]` to the \
                             crate attributes to enable",
                        );
                    }
                }
            }
            ObligationCauseCode::VariableType(_) => {
                err.note("all local variables must have a statically known size");
                if !self.tcx.features().unsized_locals {
                    err.help("unsized locals are gated as an unstable feature");
                }
            }
            ObligationCauseCode::SizedArgumentType => {
                err.note("all function arguments must have a statically known size");
                if !self.tcx.features().unsized_locals {
                    err.help("unsized locals are gated as an unstable feature");
                }
            }
            ObligationCauseCode::SizedReturnType => {
                err.note("the return type of a function must have a statically known size");
            }
            ObligationCauseCode::SizedYieldType => {
                err.note("the yield type of a generator must have a statically known size");
            }
            ObligationCauseCode::AssignmentLhsSized => {
                err.note("the left-hand-side of an assignment must have a statically known size");
            }
            ObligationCauseCode::TupleInitializerSized => {
                err.note("tuples must have a statically known size to be initialized");
            }
            ObligationCauseCode::StructInitializerSized => {
                err.note("structs must have a statically known size to be initialized");
            }
            ObligationCauseCode::FieldSized { adt_kind: ref item, last } => match *item {
                AdtKind::Struct => {
                    if last {
                        err.note(
                            "the last field of a packed struct may only have a \
                             dynamically sized type if it does not need drop to be run",
                        );
                    } else {
                        err.note(
                            "only the last field of a struct may have a dynamically sized type",
                        );
                    }
                }
                AdtKind::Union => {
                    err.note("no field of a union may have a dynamically sized type");
                }
                AdtKind::Enum => {
                    err.note("no field of an enum variant may have a dynamically sized type");
                }
            },
            ObligationCauseCode::ConstSized => {
                err.note("constant expressions must have a statically known size");
            }
            ObligationCauseCode::ConstPatternStructural => {
                err.note("constants used for pattern-matching must derive `PartialEq` and `Eq`");
            }
            ObligationCauseCode::SharedStatic => {
                err.note("shared static variables must have a type that implements `Sync`");
            }
            ObligationCauseCode::BuiltinDerivedObligation(ref data) => {
                let parent_trait_ref = self.resolve_vars_if_possible(&data.parent_trait_ref);
                let ty = parent_trait_ref.skip_binder().self_ty();
                err.note(&format!("required because it appears within the type `{}`", ty));
                obligated_types.push(ty);

                let parent_predicate = parent_trait_ref.to_predicate();
                if !self.is_recursive_obligation(obligated_types, &data.parent_code) {
                    self.note_obligation_cause_code(
                        err,
                        &parent_predicate,
                        &data.parent_code,
                        obligated_types,
                    );
                }
            }
            ObligationCauseCode::ImplDerivedObligation(ref data) => {
                let parent_trait_ref = self.resolve_vars_if_possible(&data.parent_trait_ref);
                err.note(&format!(
                    "required because of the requirements on the impl of `{}` for `{}`",
                    parent_trait_ref.print_only_trait_path(),
                    parent_trait_ref.skip_binder().self_ty()
                ));
                let parent_predicate = parent_trait_ref.to_predicate();
                self.note_obligation_cause_code(
                    err,
                    &parent_predicate,
                    &data.parent_code,
                    obligated_types,
                );
            }
            ObligationCauseCode::CompareImplMethodObligation { .. } => {
                err.note(&format!(
                    "the requirement `{}` appears on the impl method \
                     but not on the corresponding trait method",
                    predicate
                ));
            }
            ObligationCauseCode::CompareImplTypeObligation { .. } => {
                err.note(&format!(
                    "the requirement `{}` appears on the associated impl type \
                     but not on the corresponding associated trait type",
                    predicate
                ));
            }
            ObligationCauseCode::ReturnType
            | ObligationCauseCode::ReturnValue(_)
            | ObligationCauseCode::BlockTailExpression(_) => (),
            ObligationCauseCode::TrivialBound => {
                err.help("see issue #48214");
                if tcx.sess.opts.unstable_features.is_nightly_build() {
                    err.help("add `#![feature(trivial_bounds)]` to the crate attributes to enable");
                }
            }
            ObligationCauseCode::AssocTypeBound(ref data) => {
                err.span_label(data.original, "associated type defined here");
                if let Some(sp) = data.impl_span {
                    err.span_label(sp, "in this `impl` item");
                }
                for sp in &data.bounds {
                    err.span_label(*sp, "restricted in this bound");
                }
            }
        }
    }

    crate fn suggest_new_overflow_limit(&self, err: &mut DiagnosticBuilder<'_>) {
        let current_limit = self.tcx.sess.recursion_limit.get();
        let suggested_limit = current_limit * 2;
        err.help(&format!(
            "consider adding a `#![recursion_limit=\"{}\"]` attribute to your crate",
            suggested_limit
        ));
    }
}

/// Suggest restricting a type param with a new bound.
pub fn suggest_constraining_type_param(
    generics: &hir::Generics<'_>,
    err: &mut DiagnosticBuilder<'_>,
    param_name: &str,
    constraint: &str,
    source_map: &SourceMap,
    span: Span,
) -> bool {
    let restrict_msg = "consider further restricting this bound";
    if let Some(param) =
        generics.params.iter().filter(|p| p.name.ident().as_str() == param_name).next()
    {
        if param_name.starts_with("impl ") {
            // `impl Trait` in argument:
            // `fn foo(x: impl Trait) {}` → `fn foo(t: impl Trait + Trait2) {}`
            err.span_suggestion(
                param.span,
                restrict_msg,
                // `impl CurrentTrait + MissingTrait`
                format!("{} + {}", param_name, constraint),
                Applicability::MachineApplicable,
            );
        } else if generics.where_clause.predicates.is_empty() && param.bounds.is_empty() {
            // If there are no bounds whatsoever, suggest adding a constraint
            // to the type parameter:
            // `fn foo<T>(t: T) {}` → `fn foo<T: Trait>(t: T) {}`
            err.span_suggestion(
                param.span,
                "consider restricting this bound",
                format!("{}: {}", param_name, constraint),
                Applicability::MachineApplicable,
            );
        } else if !generics.where_clause.predicates.is_empty() {
            // There is a `where` clause, so suggest expanding it:
            // `fn foo<T>(t: T) where T: Debug {}` →
            // `fn foo<T>(t: T) where T: Debug, T: Trait {}`
            err.span_suggestion(
                generics.where_clause.span().unwrap().shrink_to_hi(),
                &format!("consider further restricting type parameter `{}`", param_name),
                format!(", {}: {}", param_name, constraint),
                Applicability::MachineApplicable,
            );
        } else {
            // If there is no `where` clause lean towards constraining to the
            // type parameter:
            // `fn foo<X: Bar, T>(t: T, x: X) {}` → `fn foo<T: Trait>(t: T) {}`
            // `fn foo<T: Bar>(t: T) {}` → `fn foo<T: Bar + Trait>(t: T) {}`
            let sp = param.span.with_hi(span.hi());
            let span = source_map.span_through_char(sp, ':');
            if sp != param.span && sp != span {
                // Only suggest if we have high certainty that the span
                // covers the colon in `foo<T: Trait>`.
                err.span_suggestion(
                    span,
                    restrict_msg,
                    format!("{}: {} + ", param_name, constraint),
                    Applicability::MachineApplicable,
                );
            } else {
                err.span_label(
                    param.span,
                    &format!("consider adding a `where {}: {}` bound", param_name, constraint),
                );
            }
        }
        return true;
    }
    false
}

/// Collect all the returned expressions within the input expression.
/// Used to point at the return spans when we want to suggest some change to them.
struct ReturnsVisitor<'v>(Vec<&'v hir::Expr<'v>>);

impl<'v> Visitor<'v> for ReturnsVisitor<'v> {
    type Map = rustc::hir::map::Map<'v>;

    fn nested_visit_map(&mut self) -> hir::intravisit::NestedVisitorMap<'_, Self::Map> {
        hir::intravisit::NestedVisitorMap::None
    }

    fn visit_expr(&mut self, ex: &'v hir::Expr<'v>) {
        if let hir::ExprKind::Ret(Some(ex)) = ex.kind {
            self.0.push(ex);
        }
        hir::intravisit::walk_expr(self, ex);
    }

    fn visit_body(&mut self, body: &'v hir::Body<'v>) {
        if body.generator_kind().is_none() {
            if let hir::ExprKind::Block(block, None) = body.value.kind {
                if let Some(expr) = block.expr {
                    self.0.push(expr);
                }
            }
        }
        hir::intravisit::walk_body(self, body);
    }
}