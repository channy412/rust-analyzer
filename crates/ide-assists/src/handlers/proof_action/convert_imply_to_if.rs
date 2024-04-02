// use ide_db::syntax_helpers::node_ext::is_pattern_cond;
use crate::{
    assist_context::{AssistContext, Assists},
    // utils::invert_boolean_expression,
    AssistId,
    AssistKind,
};
use syntax::{
    ast::{self, vst::*, AstNode, LogicOp},
    T,
};

pub(crate) fn imply_to_if(acc: &mut Assists, ctx: &AssistContext<'_>) -> Option<()> {
    // trigger on "assert"
    let _ = ctx.at_this_token(T![assert])?;

    // retrieve the assertion of interest
    let expr: ast::AssertExpr = ctx.find_node_at_offset()?;

    // lift CST into TOST node
    let assert: AssertExpr = AssertExpr::try_from(expr.clone()).ok()?;

    // modify TOST node
    let result = vst_rewriter_imply_to_if(assert.clone())?; 

    // pretty-print
    let result = ctx.fmt(expr.clone(),result.to_string())?;

    acc.add(
        AssistId("imply_to_if", AssistKind::RefactorRewrite),
        "Change implication into if and assert",
        expr.syntax().text_range(),
        |edit| {
            edit.replace(expr.syntax().text_range(), result);
        },
    )
}

pub(crate) fn vst_rewriter_imply_to_if(assert: AssertExpr) -> Option<IfExpr> {
    // if assertion's expression's top level is not implication, return None
    let ifstmt = match *assert.expr {
        Expr::BinExpr(b) => {
            if b.op != BinaryOp::LogicOp(LogicOp::Imply) {
                dbg!("not an implication");
                return None;
            }
            let rhs_as_assertion = AssertExpr::new(*b.rhs.clone());
            let mut blockexpr = BlockExpr::new(StmtList::new());
            blockexpr.stmt_list.statements.push(rhs_as_assertion.into());
            IfExpr::new(*b.lhs, blockexpr)
        }
        _ => {dbg!("not a binexpr"); return None;},
    };
    Some(ifstmt)    
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tests::check_assist;

    #[test]
    fn test_imply_to_if() {
        check_assist(
            imply_to_if,
"
fn test_imply_to_if(b: bool) -> (ret: u32) 
    ensures 
      b ==> ret == 2 && !b ==> ret == 1,
{
    let mut ret: u32 = 1;
    if b {
        ret = ret + 1;
    }  
    ass$0ert(b ==> ret == 2);
    ret
}  
",
"
fn test_imply_to_if(b: bool) -> (ret: u32) 
    ensures 
      b ==> ret == 2 && !b ==> ret == 1,
{
    let mut ret: u32 = 1;
    if b {
        ret = ret + 1;
    }  
    if b {
        assert(ret == 2);
    };
    ret
}  
",

        )
    }
}
