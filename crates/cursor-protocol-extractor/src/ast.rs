//! Small oxc AST accessors shared by the bundle scanner.

use oxc::ast::ast::{
    Argument, ArrayExpression, AssignmentTarget, BindingPattern, CallExpression, Expression,
    FunctionBody, ObjectPropertyKind, PropertyKey, Statement,
};
use oxc::syntax::operator::UnaryOperator;

#[must_use]
pub fn ident_name<'a>(expr: &Expression<'a>) -> Option<&'a str> {
    match expr.get_inner_expression() {
        Expression::Identifier(id) => Some(id.name.as_str()),
        _ => None,
    }
}

#[must_use]
pub fn string_lit<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr.get_inner_expression() {
        Expression::StringLiteral(lit) => lit.value.as_str(),
        _ => None,
    }
}

#[must_use]
pub fn number_lit(expr: &Expression<'_>) -> Option<f64> {
    match expr.get_inner_expression() {
        Expression::NumericLiteral(lit) => Some(lit.value),
        Expression::UnaryExpression(unary) if unary.operator.is_arithmetic() => {
            number_lit(&unary.argument).map(|value| {
                if unary.operator == UnaryOperator::UnaryNegation {
                    -value
                } else {
                    value
                }
            })
        }
        _ => None,
    }
}

/// JS numbers are doubles, so every integer read out of the AST goes through
/// here: out-of-range values are rejected instead of silently saturating. A
/// proto field id or enum number that does not fit cannot come from a Cursor
/// bundle, so dropping it surfaces as a diagnostic instead of a wrong schema.
#[must_use]
pub fn f64_to_i32(value: f64) -> Option<i32> {
    if value.is_finite() && (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value) {
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "the range check above makes the cast exact"
        )]
        let converted = value as i32;
        Some(converted)
    } else {
        None
    }
}

#[must_use]
pub fn f64_to_u32(value: f64) -> Option<u32> {
    if value.is_finite() && (0.0..=f64::from(u32::MAX)).contains(&value) {
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the range check above makes the cast exact"
        )]
        let converted = value as u32;
        Some(converted)
    } else {
        None
    }
}

#[must_use]
pub fn is_js_true(expr: &Expression<'_>) -> bool {
    match expr.get_inner_expression() {
        Expression::BooleanLiteral(lit) => lit.value,
        Expression::NumericLiteral(lit) => lit.value != 0.0,
        Expression::UnaryExpression(unary) if unary.operator.is_not() => {
            match unary.argument.get_inner_expression() {
                Expression::NumericLiteral(lit) => lit.value == 0.0,
                Expression::BooleanLiteral(lit) => !lit.value,
                _ => false,
            }
        }
        _ => false,
    }
}

#[must_use]
pub fn member_or_ident(expr: &Expression<'_>) -> Option<String> {
    match expr.get_inner_expression() {
        Expression::Identifier(id) => Some(id.name.to_string()),
        Expression::StaticMemberExpression(member) => {
            let object = member_or_ident(&member.object)?;
            Some(format!("{object}.{}", member.property.name))
        }
        Expression::PrivateFieldExpression(member) => {
            let object = member_or_ident(&member.object)?;
            Some(format!("{object}.{}", member.field.name))
        }
        _ => None,
    }
}

#[must_use]
pub fn callee_tail<'a>(call: &'a CallExpression<'a>) -> Option<&'a str> {
    callee_tail_expr(&call.callee)
}

fn callee_tail_expr<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr.get_inner_expression() {
        Expression::Identifier(id) => Some(id.name.as_str()),
        Expression::StaticMemberExpression(member) => Some(member.property.name.as_str()),
        Expression::SequenceExpression(seq) => seq.expressions.last().and_then(callee_tail_expr),
        _ => None,
    }
}

#[must_use]
pub fn argument_expr<'a>(arg: &'a Argument<'a>) -> Option<&'a Expression<'a>> {
    arg.as_expression()
}

#[must_use]
pub fn property_name<'a>(key: &'a PropertyKey<'a>) -> Option<&'a str> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
        PropertyKey::Identifier(id) => Some(id.name.as_str()),
        PropertyKey::StringLiteral(lit) => lit.value.as_str(),
        _ => None,
    }
}

#[must_use]
pub fn property_numeric_key(key: &PropertyKey<'_>) -> Option<u32> {
    match key {
        PropertyKey::NumericLiteral(lit) => f64_to_u32(lit.value),
        PropertyKey::StringLiteral(lit) => lit.value.as_str().and_then(|value| value.parse().ok()),
        PropertyKey::StaticIdentifier(id) => id.name.parse().ok(),
        _ => None,
    }
}

#[must_use]
pub fn object_prop<'a>(expr: &'a Expression<'a>, name: &str) -> Option<&'a Expression<'a>> {
    let Expression::ObjectExpression(object) = expr.get_inner_expression() else {
        return None;
    };
    for property in &object.properties {
        let ObjectPropertyKind::ObjectProperty(prop) = property else {
            continue;
        };
        if property_name(&prop.key) == Some(name) {
            return Some(&prop.value);
        }
    }
    None
}

#[must_use]
pub fn object_prop_shorthand(expr: &Expression<'_>, name: &str) -> bool {
    let Expression::ObjectExpression(object) = expr.get_inner_expression() else {
        return false;
    };
    object.properties.iter().any(|property| {
        let ObjectPropertyKind::ObjectProperty(prop) = property else {
            return false;
        };
        prop.shorthand && property_name(&prop.key) == Some(name)
    })
}

#[must_use]
pub fn assignment_ident<'a>(target: &'a AssignmentTarget<'a>) -> Option<&'a str> {
    target.get_identifier_name()
}

#[must_use]
pub fn assignment_member<'a>(
    target: &'a AssignmentTarget<'a>,
) -> Option<(&'a Expression<'a>, &'a str)> {
    match target {
        AssignmentTarget::StaticMemberExpression(member) => {
            Some((&member.object, member.property.name.as_str()))
        }
        _ => None,
    }
}

#[must_use]
pub fn binding_ident<'a>(pattern: &'a BindingPattern<'a>) -> Option<&'a str> {
    pattern.get_identifier_name().map(|name| name.as_str())
}

#[must_use]
pub fn returned_expression<'a>(body: &'a FunctionBody<'a>) -> Option<&'a Expression<'a>> {
    for stmt in body.statements.iter().rev() {
        if let Statement::ReturnStatement(ret) = stmt {
            return ret.argument.as_ref();
        }
    }
    None
}

#[must_use]
pub fn array_from_expr<'a>(expr: &'a Expression<'a>) -> Option<&'a ArrayExpression<'a>> {
    match expr.get_inner_expression() {
        Expression::ArrayExpression(array) => Some(array),
        Expression::ArrowFunctionExpression(arrow) => {
            if let Some(body) = arrow.get_expression() {
                return array_from_expr(body);
            }
            arrow
                .get_function_body()
                .and_then(returned_expression)
                .and_then(array_from_expr)
        }
        Expression::FunctionExpression(func) => func
            .body
            .as_ref()
            .and_then(|body| returned_expression(body))
            .and_then(array_from_expr),
        _ => None,
    }
}

#[must_use]
pub fn last_segment(path: &str) -> &str {
    path.rsplit(['.', '/']).next().unwrap_or(path)
}
