use super::{*, items::ITEM_RECOVERY_SET};


pub(crate)  fn assume(p: &mut Parser<'_>, m: Marker) {
    p.expect(T![assume]);
    p.expect(T!['(']);
    expressions::expr(p);
    p.expect(T![')']);
    m.complete(p, ASSUME_EXPR);
}

// AssertExpr =
//   'assert' '(' Expr ')' 'by'? ( '(' Name ')' )?  RequiresClause? BlockExpr?
pub(crate)  fn assert(p: &mut Parser<'_>, m: Marker) {
    p.expect(T![assert]);
    
    if p.at(T!['(']) {
        // parse expression here
        p.expect(T!['(']);
        expressions::expr(p);
        p.expect(T![')']);
    } else {
        // TODO: make this a separate kind AssertForall
        // assert forall|x: int, y: int| f1(x) + f1(y) == x + y + 2 by {
        //     reveal(f1);
        // }
        p.error("TODO: make this a separate kind AssertForall");
        expressions::expr(p);
        if p.at(T![implies]) {
            p.bump(T![implies]);
            expressions::expr(p);
        }
        // p.error("expected function arguments");
    }
    
    // parse optional `by`
    // bit_vector, nonlinear_artih ...
    if p.at(T![by]) {
        p.expect(T![by]);
        if p.at(T!['(']) {
            p.expect(T!['(']);
            // p.bump_any();
            name_r(p, ITEM_RECOVERY_SET);
            p.expect(T![')']);
        }
    }

    // parse optional 'requires`
    if p.at(T![requires]) {
        requires(p);
    }

    if p.at(T![;]) {
        // test fn_decl
        // trait T { fn foo(); }
        // dbg!("getting ;, but ignoring");
        // p.bump(T![;]);
    } else {
        dbg!("proof block");
        // parse optional 'proof block'
        expressions::block_expr(p);
    }

    m.complete(p, ASSERT_EXPR);
}

pub(crate)  fn requires(p: &mut Parser<'_>) -> CompletedMarker {
    dbg!("requires");
    let m = p.start();
    p.expect(T![requires]);

    while !p.at(EOF) && !p.at(T![recommends]) && !p.at(T![ensures]) && !p.at(T![decreases]) && !p.at(T!['{'])  && !p.at(T![;])  {
        cond_comma(p);
        if p.at(T![recommends]) || p.at(T![ensures]) || p.at(T![decreases]) || p.at(T!['{']) {
            break;
        }
    }
    m.complete(p, REQUIRES_CLAUSE)
}


pub(crate)  fn recommends(p: &mut Parser<'_>) -> CompletedMarker {
    dbg!("recommends");
    let m = p.start();
    p.expect(T![recommends]);
    while !p.at(EOF) && !p.at(T![ensures]) && !p.at(T![decreases]) && !p.at(T!['{'])  && !p.at(T![;])  {
        cond_comma(p);
        if p.at(T![recommends]) || p.at(T![ensures]) || p.at(T![decreases]) || p.at(T!['{']) {
            break;
        }
    }
    m.complete(p, RECOMMENDS_CLAUSE)
}


pub(crate)  fn ensures(p: &mut Parser<'_>) -> CompletedMarker {
    dbg!("ensures");
    let m = p.start();
    p.expect(T![ensures]);

    while !p.at(EOF)  && !p.at(T![decreases]) && !p.at(T!['{'])  && !p.at(T![;])  {
        cond_comma(p);
        if p.at(T![recommends]) || p.at(T![ensures]) || p.at(T![decreases]) || p.at(T!['{']) {
            break;
        }
    }
    m.complete(p, ENSURES_CLAUSE)
}

pub(crate) fn decreases(p: &mut Parser<'_>) -> CompletedMarker {
    dbg!("decreases");
    let m = p.start();
    p.expect(T![decreases]);
    while !p.at(EOF) && !p.at(T!['{'])  && !p.at(T![;]) {
        pat_comma(p);
        if p.at(T![recommends]) || p.at(T![ensures]) || p.at(T![decreases]) || p.at(T!['{']) {
            break;
        }
    }
    m.complete(p, DECREASES_CLAUSE)
}


fn cond_comma(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    expressions::expr(p);
    p.expect(T![,]);
    m.complete(p, COND_AND_COMMA)
}

fn pat_comma(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    patterns::pattern(p); 
    p.expect(T![,]);
    m.complete(p, PAT_AND_COMMA)
}
