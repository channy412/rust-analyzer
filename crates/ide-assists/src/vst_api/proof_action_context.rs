#![allow(dead_code)]

use crate::{AssistContext, verus_error::*};
use syntax::{
    ast::{self, vst},
    AstNode, SyntaxKind,
};

impl<'a> AssistContext<'a> {
    /// Get VST node from the current cursor position
    /// This is a wrapper around `find_node_at_offset` that returns a VST node
    /// REVIEW: to remove type annotation, consider auto-generating all sorts of this function
    pub(crate) fn vst_find_node_at_offset<VSTT, CSTT>(&self) -> Option<VSTT>
    where
        VSTT: TryFrom<CSTT>,
        CSTT: AstNode,
    {
        let cst_node: CSTT = self.find_node_at_offset()?;
        VSTT::try_from(cst_node).ok()
    }

    pub(crate) fn verus_errors(&self) -> Vec<VerusError> {
        self.verus_errors.clone()
    }

    // note that `pre` uses `pre.callsite` instead of `pre.failing_pre`.
    // technically, the failing pre condition is not the error of that function.
    // it is error of the callsite
    pub(crate) fn verus_errors_inside_fn(&self, func: &vst::Fn) -> Option<Vec<VerusError>> {
        let surrounding_fn: &ast::Fn = func.cst.as_ref()?;
        let surrounding_range = surrounding_fn.syntax().text_range();
        let filtered_verus_errs = self
            .verus_errors()
            .into_iter()
            .filter(|verr| match verr {
                VerusError::Pre(pre) => surrounding_range.contains_range(pre.callsite),
                VerusError::Post(post) => surrounding_range.contains_range(post.failing_post),
                VerusError::Assert(assert) => surrounding_range.contains_range(assert.range),
            })
            .collect();
        Some(filtered_verus_errs)
    }

    pub(crate) fn pre_failures_by_calling_this_fn(&self, func: &vst::Fn) -> Option<Vec<PreFailure>> {
        let surrounding_fn: &ast::Fn = func.cst.as_ref()?;
        let surrounding_range: text_edit::TextRange = surrounding_fn.syntax().text_range();
        let filtered_verus_errs: Vec<VerusError> = self
            .verus_errors()
            .into_iter()
            .filter(|verr| match verr {
                VerusError::Pre(pre) => surrounding_range.contains_range(pre.failing_pre),
                _ => false,
            })
            .collect();
        Some(filter_pre_failuires(&filtered_verus_errs))
    }

    pub(crate) fn pre_failures(&self) -> Vec<PreFailure> {
        filter_pre_failuires(&self.verus_errors)
    }
    pub(crate) fn post_failures(&self) -> Vec<PostFailure> {
        filter_post_failuires(&self.verus_errors)
    }

    pub(crate) fn expr_from_pre_failure(&self, pre: PreFailure) -> Option<vst::Expr> {
        self.find_node_at_given_range::<syntax::ast::Expr>(pre.failing_pre)?.try_into().ok()
    }
    pub(crate) fn expr_from_post_failure(&self, post: PostFailure) -> Option<vst::Expr> {
        self.find_node_at_given_range::<syntax::ast::Expr>(post.failing_post)?.try_into().ok()
    }

    pub(crate) fn at_this_token(&self, token: SyntaxKind) -> Option<()> {
        let assert_keyword = self.find_token_syntax_at_offset(token)?;
        let cursor_in_range = assert_keyword.text_range().contains_range(self.selection_trimmed());
        if !cursor_in_range {
            return None;
        }
        Some(())
    }

    // helper routine to replace statement
    pub(crate) fn replace_statement<ST0, ST1>(&self, func: &vst::Fn, old: ST0, new: ST1) -> Option<vst::Fn> 
    where
        ST0: Into<vst::Stmt> + std::clone::Clone,
        ST1: Into<vst::Stmt> + std::clone::Clone,
    {
        let old:vst::Stmt = old.into();
        let new:vst::Stmt = new.into();
        let stmts = func.body.as_ref()?.stmt_list.statements.clone(); 
        let mut func = func.clone();
        let filtered_stmts: Vec<vst::Stmt> = stmts.into_iter().map(|s| if s == old.clone() {new.clone()} else {s}).collect();
        func.body.as_mut()?.stmt_list.statements = filtered_stmts;  
        Some(func)
    }

    // helper routine to reduce a list of predicate into &&-ed predicate
    pub(crate) fn reduce_exprs(&self, es: Vec<vst::Expr>) -> Option<vst::Expr> {
        es.into_iter().reduce(|acc, e| {
            vst::Expr::BinExpr(Box::new(vst::BinExpr::new(
                acc,
                vst::BinaryOp::LogicOp(ast::LogicOp::And),
                e,
            )))
        })
    }
}
