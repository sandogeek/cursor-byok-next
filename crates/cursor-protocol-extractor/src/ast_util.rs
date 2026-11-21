//! Small oxc helpers shared by the bundle scanner.

use oxc::ast::ast::{
    Argument, ArrayExpression, AssignmentTarget, BindingPattern, CallExpression, Expression,
    FormalParameter, FunctionBody, ObjectPropertyKind, PropertyKey, Statement,
};
use oxc::str::JSStr;
use oxc::syntax::operator::UnaryOperator;

pub fn ident_name<'a>(expr: &Expression<'a>) -> Option<&'a str> {
    match expr.get_inner_expression() {
        Expression::Identifier(id) => Some(id.name.as_str()),
        _ => None,
    }
}

pub fn js_str<'a>(value: &'a JSStr<'a>) -> Option<&'a str> {
    value.as_str()
}

pub fn string_lit<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr.get_inner_expression() {
        Expression::StringLiteral(lit) => js_str(&lit.value),
        _ => None,
    }
}

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

pub fn argument_expr<'a>(arg: &'a Argument<'a>) -> Option<&'a Expression<'a>> {
    arg.as_expression()
}

pub fn property_name<'a>(key: &'a PropertyKey<'a>) -> Option<&'a str> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
        PropertyKey::Identifier(id) => Some(id.name.as_str()),
        PropertyKey::StringLiteral(lit) => js_str(&lit.value),
        _ => None,
    }
}

pub fn property_numeric_key(key: &PropertyKey<'_>) -> Option<u32> {
    match key {
        PropertyKey::NumericLiteral(lit) => Some(lit.value as u32),
        PropertyKey::StringLiteral(lit) => js_str(&lit.value).and_then(|value| value.parse().ok()),
        PropertyKey::StaticIdentifier(id) => id.name.parse().ok(),
        _ => None,
    }
}

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

pub fn assignment_ident<'a>(target: &'a AssignmentTarget<'a>) -> Option<&'a str> {
    target.get_identifier_name()
}

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

pub fn binding_ident<'a>(pattern: &'a BindingPattern<'a>) -> Option<&'a str> {
    pattern.get_identifier_name().map(|name| name.as_str())
}

pub fn returned_expression<'a>(body: &'a FunctionBody<'a>) -> Option<&'a Expression<'a>> {
    for stmt in body.statements.iter().rev() {
        if let Statement::ReturnStatement(ret) = stmt {
            return ret.argument.as_ref();
        }
    }
    None
}

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

pub fn param_names(params: &[FormalParameter<'_>]) -> Vec<String> {
    params
        .iter()
        .filter_map(|param| binding_ident(&param.pattern).map(str::to_string))
        .collect()
}

pub fn last_segment(path: &str) -> &str {
    path.rsplit(['.', '/']).next().unwrap_or(path)
}
