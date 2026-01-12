example for detecting fend computations:

```rust
use fend_core::{lexer, parser, ast::Expr};
fn contains_computation(expr: &Expr) -> bool {
    match expr {
        // No computation - just a literal value
        Expr::Literal(_) => false,

        // Parens are just grouping, check inside
        Expr::Parens(inner) => contains_computation(inner),

        // Unary operations involve computation
        Expr::UnaryMinus(_) | Expr::UnaryPlus(_) | Expr::UnaryDiv(_) | Expr::Factorial(_) => true,

        // Binary operations are computation
        Expr::Bop(_, _, _) => true,

        // Function application or multiplication involves computation
        Expr::Apply(_, _) | Expr::ApplyFunctionCall(_, _) | Expr::ApplyMul(_, _) => true,

        // Unit conversion is computation
        Expr::As(_, _) => true,

        // Lambda creation is computation
        Expr::Fn(_, _) => true,

        // Member access might be computation
        Expr::Of(_, _) => true,

        // Variable assignment is computation
        Expr::Assign(_, _) => true,

        // Multiple statements - check if any involves computation
        Expr::Statements(a, b) => contains_computation(a) || contains_computation(b),

        // Equality check is computation
        Expr::Equality(_, _, _) => true,

        // Identifier - check if it resolves to a unit/builtin (not just a number literal)
        // You can't determine this without context, so treat as "possibly computation"
        Expr::Ident(_) => true,
    }
}
fn should_show_result(input: &str, context: &mut fend_core::Context) -> bool {
    // Lex and parse
    let tokens: Vec<_> = fend_core::lexer::lex(input, context, &fend_core::interrupt::Never)
        .collect();
    let tokens = tokens.into_iter().collect::<Result<Vec<_>, _>>().ok()?;

    let parsed = parser::parse_tokens(&tokens).ok()?;

    // Check if computation is involved
    contains_computation(&parsed)
}
```
