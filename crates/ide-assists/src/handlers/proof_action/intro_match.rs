use std::vec;

use crate::{AssistContext, AssistId, AssistKind, Assists};
use ide_db::syntax_helpers::vst_ext::vst_walk_expr;

use syntax::{
    ast::{self, vst::*},
    AstNode, T,
};

pub(crate) fn intro_match(acc: &mut Assists, ctx: &AssistContext<'_>) -> Option<()> {
    // trigger on `assert` keyword
    let assert_keyword = ctx.find_token_syntax_at_offset(T![assert])?;
    let assert_expr = ast::AssertExpr::cast(assert_keyword.parent()?)?;
    let assert_range = assert_keyword.text_range();
    let cursor_in_range = assert_range.contains_range(ctx.selection_trimmed());
    if !cursor_in_range {
        return None;
    }

    let assert: AssertExpr = AssertExpr::try_from(assert_expr.clone()).ok()?;
    let result = vst_rewriter_intro_match(ctx, assert.clone())?;
    let result = ctx.fmt(assert_expr.clone(),result.to_string())?;

    // register code change to `acc`
    acc.add(
        AssistId("intro_match", AssistKind::RefactorRewrite),
        "Add match pattern for failed assert on enum ",
        assert_expr.syntax().text_range(),
        |edit| {
            edit.replace(assert_expr.syntax().text_range(), result);
        },
    )
}

pub(crate) fn vst_rewriter_intro_match(
    ctx: &AssistContext<'_>,
    assert: AssertExpr,
) -> Option<MatchExpr> {
    let mut v = vec![];
    let cb = &mut |e: Expr| {
        if let Some(_) = ctx.type_of_expr_enum(&e) {
            v.push(e.clone());
        }
    };
    let exp_assert = Expr::AssertExpr(Box::new(assert.clone()));
    // walk over the assertion's predicate, to get expressions of `enum` type.
    vst_walk_expr(&exp_assert, cb);
    if v.len() == 0 {
        return None;
    }
    let enum_expr_inside_assertion = &v[0]; // select first 
    let en = ctx.type_of_expr_enum(enum_expr_inside_assertion)?;
    let mut match_arms: Vec<MatchArm> = vec![];
    for variant in &en.variant_list.variants {
        let vst_pat = Literal::new(format!("{}::{}(..)", en.name, variant.name));
        let vst_pat = LiteralPat::new(vst_pat);
        let arm = MatchArm::new(vst_pat.into(), assert.clone());
        match_arms.push(arm);
    }

    // now run verifier and only present failing variants
    // Try each variant --- for the rest(`_`), use "assume false"
    let match_arms: Option<Vec<MatchArm>>= match_arms.into_iter().map(|arm| {
        let this_fn = ctx.vst_find_node_at_offset::<Fn, ast::Fn>()?; 
        let wild_card = Literal::new(format!("_"));
        let wild_pat = LiteralPat::new(wild_card);
        let assume_false = ctx.vst_expr_from_text("assume(false)")?;
        let wild_arm = MatchArm::new(wild_pat.into(), assume_false);
        let simple_arms = vec![arm.clone(), wild_arm];
        let mut match_arm_lst = MatchArmList::new();
        match_arm_lst.arms = simple_arms;
        let simple_match_stmt = MatchExpr::new(enum_expr_inside_assertion.clone(), match_arm_lst);
        let modified_fn = ctx.replace_statement(&this_fn, assert.clone(), simple_match_stmt.clone())?;
        let verif_result = ctx.try_verus(&modified_fn)?;
        if verif_result.is_failing(&assert) {
            Some(arm.clone())
        } else {
            None
        }
    }).filter(|x| x.is_some()).collect();

    let mut match_arm_list = MatchArmList::new();
    match_arm_list.arms = match_arms?;
    let match_stmt = MatchExpr::new(enum_expr_inside_assertion.clone(), match_arm_list);

    
    Some(match_stmt)
}

#[cfg(test)]
mod tests {
    use crate::tests::check_assist;

    use super::*;

    // TEST1
    #[test]
    fn intro_match1() {
        check_assist(
            intro_match,
            r#"
enum Movement {
    Up(u32),
    Down(u32),
}

spec fn is_good_move(m: Movement) -> bool {
    match m {
        Movement::Up(v) => v > 100,
        Movement::Down(v) => v > 100,
    }
}

proof fn good_move(m: Movement)
{
    ass$0ert(is_good_move(m));
}
"#,
            r#"
enum Movement {
    Up(u32),
    Down(u32),
}

spec fn is_good_move(m: Movement) -> bool {
    match m {
        Movement::Up(v) => v > 100,
        Movement::Down(v) => v > 100,
    }
}

proof fn good_move(m: Movement)
{
    match m {
        Movement::Up(..) => assert(is_good_move(m)),
        Movement::Down(..) => assert(is_good_move(m)),
    };
}
"#,
        );
    }


    // TEST2
    #[test]
    fn intro_match2() {
        check_assist(
            intro_match,
            r#"
enum Movement {
    Up(u32),
    Down(u32),
}

spec fn is_good_move(m: Movement, a: int) -> bool {
    match m {
        Movement::Up(v) => v > a,
        Movement::Down(v) => v > 100,
    }
}

proof fn good_move(m: Movement)
{
    ass$0ert(is_good_move(m, 100));
}
"#,
            r#"
enum Movement {
    Up(u32),
    Down(u32),
}

spec fn is_good_move(m: Movement, a: int) -> bool {
    match m {
        Movement::Up(v) => v > a,
        Movement::Down(v) => v > 100,
    }
}

proof fn good_move(m: Movement)
{
    match m {
        Movement::Up(..) => assert(is_good_move(m, 100)),
        Movement::Down(..) => assert(is_good_move(m, 100)),
    };
}
"#,
        );
    }


//     #[test]
//     fn intro_match3() {
//         check_assist(
//             intro_match,
//             r#"
// verus!{
//     #[derive(PartialEq, Eq)] 
//     pub enum Message {
//         Quit(bool),
//         Move { x: i32, y: i32 },
//         Write(bool),
//     }
    
//     spec fn is_good_integer_3(x: int) -> bool 
//     {
//         x >= 0 && x != 5
//     }
    
//     spec fn is_good_message(msg:Message) -> bool {
//         match msg {
//             Message::Quit(b) => b,
//             Message::Move{x, y} => is_good_integer_3( (x as int)  - (y as int)),
//             Message::Write(b) => b,
//         }
//     }
    
//     proof fn test_expansion_multiple_call() {
//       let x = Message::Move{x: 5, y:6};
//       as$0sert(is_good_message(x));
//     }
// }
// "#,

// r#"
// verus!{
//     #[derive(PartialEq, Eq)] 
//     pub enum Message {
//         Quit(bool),
//         Move { x: i32, y: i32 },
//         Write(bool),
//     }
    
//     spec fn is_good_integer_3(x: int) -> bool 
//     {
//         x >= 0 && x != 5
//     }
    
//     spec fn is_good_message(msg:Message) -> bool {
//         match msg {
//             Message::Quit(b) => b,
//             Message::Move{x, y} => is_good_integer_3( (x as int)  - (y as int)),
//             Message::Write(b) => b,
//         }
//     }
    
//     proof fn test_expansion_multiple_call() {
//       let x = Message::Move{x: 5, y:6};
//       match x {
//         Message::Quit(..) => assert(is_good_message(x)),
//         Message::Move{..} => assert(is_good_message(x)),
//         Message::Write(..) => assert(is_good_message(x)),
//       };
//     }
// }
// "#
//         );
//     }
}
