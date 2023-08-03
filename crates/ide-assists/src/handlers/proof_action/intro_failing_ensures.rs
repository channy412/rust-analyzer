use crate::{AssistContext, AssistId, AssistKind, Assists};
use crate::verus_error::*;

use ide_db::syntax_helpers::vst_ext;
use syntax::{
    ast::{self,  vst::*},
     AstNode, 
};

pub(crate) fn intro_failing_ensures(acc: &mut Assists, ctx: &AssistContext<'_>) -> Option<()> {
    // setup basic variables
    let func: ast::Fn = ctx.find_node_at_offset::<ast::Fn>()?;
    let body: ast::BlockExpr = func.body()?;
    let ensures: ast::EnsuresClause = func.ensures_clause()?;

    // trigger on "ensures"
    // check if cursor is on "ensures" keyword
    let ensures_keyword = ensures.ensures_token()?;
    let cursor_in_range = ensures_keyword.text_range().contains_range(ctx.selection_trimmed());
    if !cursor_in_range {
        return None;
    }

    let v_body = BlockExpr::try_from(body.clone()).ok()?;
    let result = vst_rewriter_intro_failing_ensures(ctx, v_body.clone())?;

    acc.add(
        AssistId("intro_failing_ensures", AssistKind::RefactorRewrite),
        "Insert failing ensures clauses to the end",
        body.syntax().text_range(),
        |edit| {
            edit.replace(body.syntax().text_range(), result);
        },
    )
}

pub(crate) fn vst_rewriter_intro_failing_ensures(
    ctx: &AssistContext<'_>,
    mut blk: BlockExpr,
) -> Option<String> {
    let post_fails = filter_post_failuires(&ctx.verus_errors);
    let failed_exprs: Option<Vec<Expr>> = post_fails.into_iter().map(|p| ctx.expr_from_post_failure(p)).collect(); 
    let asserts_failed_exprs = failed_exprs?.into_iter().map(|e| {
        AssertExpr::new(e).into()
    }).collect::<Vec<Stmt>>();
    
    let foo = ctx.vst_find_node_at_offset::<Fn, ast::Fn>()?;
    if foo.ret_type.is_some() {
        // need to map in-place for each tail expression
        // when the function has a returning expression `e`
        // `e` into
        // ```
        // let ret = e;
        // assert(failing_stuff);
        // ret
        // ```
        let pat = foo.ret_type?.pat?.clone();
        let tail = foo.body?.stmt_list.tail_expr?;
        let cb = &mut |e: &mut Expr| {
            let mut new_binding = LetExpr::new(e.clone());
            new_binding.pat = Some(pat.clone());
            let new_let_stmt: Stmt = new_binding.into();
            let mut stmt_list = StmtList::new();
            let mut stmts = asserts_failed_exprs.clone();
            stmts.insert(0, new_let_stmt);
            stmt_list.statements = stmts;
            stmt_list.tail_expr = Some(Box::new(Literal::new(pat.to_string()).into()));
            let new_block_expr = BlockExpr::new(stmt_list);
            Ok(new_block_expr.into())
        };
        let new_tail = vst_ext::vst_map_each_tail_expr(*tail.clone(), cb).ok()?;
        blk.stmt_list.tail_expr = Some(Box::new(new_tail));
        Some(blk.to_string())
    } else {
        // just append the assertions
        let mut stmt_list = blk.stmt_list.clone();
        stmt_list.statements.extend(asserts_failed_exprs);
        blk.stmt_list = stmt_list;
        Some(blk.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::check_assist;

    use super::*;

    #[test]
    fn intro_failing_ensures_easy() {
        check_assist(
            intro_failing_ensures,
            r#"
proof fn my_proof_fun(x: int, y: int)
    requires
        x < 100,
        y < 100,
    ens$0ures
        x + y < 200,
        x + y < 100,
{
    assert(x + y < 600);
}
"#,
            r#"
proof fn my_proof_fun(x: int, y: int)
    requires
        x < 100,
        y < 100,
    ensures
        x + y < 200,
        x + y < 400,
{
    assert(x + y < 600);

    assert(x + y < 100); 
}
"#,
        );
    }

    #[test]
    fn intro_ensure_ret_arg() {
        check_assist(
            intro_failing_ensures,
            r#"
proof fn my_proof_fun(x: int, y: int) -> (sum: int)
    requires
        x < 100,
        y < 100,
    ens$0ures
        sum < 100,
        sum < 200,
        sum < 300,
{
    x + y
}
"#,
            r#"
proof fn my_proof_fun(x: int, y: int) -> (sum: int)
    requires
        x < 100,
        y < 100,
    ensures
        sum < 100,
        sum < 200,
        sum < 300,
{
    let sum = x + y; 
    assert(sum < 100); 
    sum
}
"#,
        );
    }


    #[test]
    fn intro_ensure_multiple_ret_arg() {
        check_assist(
            intro_failing_ensures,
            r#"
proof fn my_proof_fun(x: int, y: int) -> (sum: int)
    requires
        x < 100,
        y < 100,
    ens$0ures
        sum < 100,
        sum < 200,
        sum < 300,
{
    if x > 0 {
        x + y + 1
    } else {
        x + y
    }
}
"#,
            r#"
proof fn my_proof_fun(x: int, y: int) -> (sum: int)
    requires
        x < 100,
        y < 100,
    ensures
        sum < 100,
        sum < 200,
        sum < 300,
{
    if x > 0 {
        let sum = x + y + 1;
        assert(sum < 100);
        sum
    } else {
        let sum = x + y;
        assert(sum < 100);
        sum
    }
}
"#,
        );
    }


    #[test]
    fn intro_ensure_fibo() {
        check_assist(
            intro_failing_ensures,
            r#"
proof fn lemma_fibo_is_monotonic(i: nat, j: nat)
    requires
        i <= j,
    e$0nsures
        fibo(i) <= fibo(j),
    decreases j - i
{
    if i < 2 && j < 2 {
    } else if i == j {
    } else if i == j - 1 {
        reveal_with_fuel(fibo, 2);
        lemma_fibo_is_monotonic(i, (j - 1) as nat);
    } else {
        lemma_fibo_is_monotonic(i, (j - 1) as nat);
        lemma_fibo_is_monotonic(i, (j - 2) as nat);
    }
}
"#,
            r#"
proof fn lemma_fibo_is_monotonic(i: nat, j: nat)
    requires
        i <= j,
    ensures
        fibo(i) <= fibo(j),
    decreases j - i
{
    let _ = if i < 2 && j < 2 {
    } else if i == j {
    } else if i == j - 1 {
        reveal_with_fuel(fibo, 2);
        lemma_fibo_is_monotonic(i, (j - 1) as nat);
    } else {
        lemma_fibo_is_monotonic(i, (j - 1) as nat);
        lemma_fibo_is_monotonic(i, (j - 2) as nat);
    }; 
    assert(fibo(i) <= fibo(j)); 
    ()
}
"#,
        );
    }
}