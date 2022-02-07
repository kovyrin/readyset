use std::iter;

use itertools::Either;
use nom_sql::{Expression, FunctionExpression, InValue, JoinRightSide, SqlQuery};
use noria_errors::{unsupported, ReadySetResult};

#[derive(Debug, PartialEq)]
pub enum SubqueryPosition<'a> {
    /// Subqueries on the right hand side of a join
    ///
    /// Invariant: This will always contain [`JoinRightSide::NestedSelect`]
    Join(&'a mut JoinRightSide),

    /// Subqueries on the right hand side of an IN
    ///
    /// Invariant: This will always contain [`InValue::Subquery`]
    In(&'a mut InValue),

    /// Subqueries in expressions.
    ///
    /// Invariant: This will always contain [`Expression::NestedSelect`]
    Expr(&'a mut Expression),
}

pub trait SubQueries {
    fn extract_subqueries(&mut self) -> ReadySetResult<Vec<SubqueryPosition>>;
}

fn extract_subqueries_from_function_call(
    call: &mut FunctionExpression,
) -> ReadySetResult<Vec<SubqueryPosition>> {
    match call {
        FunctionExpression::Avg { expr, .. }
        | FunctionExpression::Count { expr, .. }
        | FunctionExpression::Sum { expr, .. }
        | FunctionExpression::Max(expr)
        | FunctionExpression::Min(expr)
        | FunctionExpression::GroupConcat { expr, .. } => extract_subqueries_from_expression(expr),
        FunctionExpression::CountStar => Ok(vec![]),
        FunctionExpression::Call { arguments, .. } => Ok(arguments
            .iter_mut()
            .map(extract_subqueries_from_expression)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect()),
    }
}

fn extract_subqueries_from_expression(
    expr: &mut Expression,
) -> ReadySetResult<Vec<SubqueryPosition>> {
    match expr {
        Expression::BinaryOp { lhs, rhs, .. } => {
            let lb = extract_subqueries_from_expression(lhs)?;
            let rb = extract_subqueries_from_expression(rhs)?;

            Ok(lb.into_iter().chain(rb.into_iter()).collect())
        }
        Expression::UnaryOp { rhs: expr, .. } | Expression::Cast { expr, .. } => {
            extract_subqueries_from_expression(expr)
        }
        Expression::Between {
            operand, min, max, ..
        } => {
            let ob = extract_subqueries_from_expression(operand)?;
            let minb = extract_subqueries_from_expression(min)?;
            let maxb = extract_subqueries_from_expression(max)?;
            Ok(ob
                .into_iter()
                .chain(minb.into_iter())
                .chain(maxb.into_iter())
                .collect())
        }
        Expression::CaseWhen {
            condition,
            then_expr,
            else_expr,
        } => Ok(extract_subqueries_from_expression(condition)?
            .into_iter()
            .chain(extract_subqueries_from_expression(then_expr)?)
            .chain(match else_expr {
                Some(else_expr) => {
                    Either::Left(extract_subqueries_from_expression(else_expr)?.into_iter())
                }
                None => Either::Right(iter::empty()),
            })
            .collect()),
        Expression::Exists(_) => unsupported!("EXISTS not supported yet"),
        Expression::NestedSelect(_) => Ok(vec![SubqueryPosition::Expr(expr)]),
        Expression::Call(call) => extract_subqueries_from_function_call(call),
        Expression::In {
            lhs,
            rhs: rhs @ InValue::Subquery(_),
            ..
        } => Ok(extract_subqueries_from_expression(lhs)?
            .into_iter()
            .chain(iter::once(SubqueryPosition::In(rhs)))
            .collect()),
        Expression::In {
            lhs,
            rhs: InValue::List(exprs),
            ..
        } => Ok(extract_subqueries_from_expression(lhs)?
            .into_iter()
            .chain(
                exprs
                    .iter_mut()
                    .map(extract_subqueries_from_expression)
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten(),
            )
            .collect()),
        Expression::Literal(_) | Expression::Column(_) | Expression::Variable(_) => Ok(vec![]),
    }
}

impl SubQueries for SqlQuery {
    fn extract_subqueries(&mut self) -> ReadySetResult<Vec<SubqueryPosition>> {
        let mut subqueries = Vec::new();
        if let SqlQuery::Select(ref mut st) = *self {
            for jc in &mut st.join {
                if let JoinRightSide::NestedSelect(_, _) = jc.right {
                    subqueries.push(SubqueryPosition::Join(&mut jc.right));
                }
            }
            if let Some(ref mut ce) = st.where_clause {
                subqueries.extend(extract_subqueries_from_expression(ce)?);
            }
        }

        Ok(subqueries)
    }
}

#[cfg(test)]
mod tests {
    use nom_sql::{
        BinaryOperator, Column, FieldDefinitionExpression, SelectStatement, SqlQuery, Table,
    };

    use super::*;

    #[test]
    fn it_extracts_subqueries() {
        // select userid from role where type=1
        let sq = SelectStatement {
            tables: vec![Table::from("role")],
            fields: vec![FieldDefinitionExpression::from(Column::from("userid"))],
            where_clause: Some(Expression::BinaryOp {
                op: BinaryOperator::Equal,
                lhs: Box::new(Expression::Column(Column::from("type"))),
                rhs: Box::new(Expression::Literal(1.into())),
            }),
            ..Default::default()
        };

        let mut expected = InValue::Subquery(Box::new(sq));

        // select pid from post where author in (select userid from role where type=1)
        let st = SelectStatement {
            tables: vec![Table::from("post")],
            fields: vec![FieldDefinitionExpression::from(Column::from("pid"))],
            where_clause: Some(Expression::In {
                lhs: Box::new(Expression::Column(Column::from("author"))),
                rhs: expected.clone(),
                negated: false,
            }),
            ..Default::default()
        };

        let mut q = SqlQuery::Select(st);
        let res = q.extract_subqueries().unwrap();

        assert_eq!(res, vec![SubqueryPosition::In(&mut expected)]);
    }

    #[test]
    fn it_does_nothing_for_flat_queries() {
        // select userid from role where type=1
        let mut q = SqlQuery::Select(SelectStatement {
            tables: vec![Table::from("role")],
            fields: vec![FieldDefinitionExpression::from(Column::from("userid"))],
            where_clause: Some(Expression::BinaryOp {
                op: BinaryOperator::Equal,
                lhs: Box::new(Expression::Column(Column::from("type"))),
                rhs: Box::new(Expression::Literal(1.into())),
            }),
            ..Default::default()
        });

        let res = q.extract_subqueries().unwrap();
        let expected: Vec<SubqueryPosition> = Vec::new();

        assert_eq!(res, expected);
    }

    #[test]
    fn it_works_with_complex_queries() {
        // select users.name, articles.title, votes.uid \
        //          from articles, users, votes
        //          where users.id = articles.author \
        //          and votes.aid = articles.aid;

        let mut q = SqlQuery::Select(SelectStatement {
            tables: vec![
                Table::from("articles"),
                Table::from("users"),
                Table::from("votes"),
            ],
            fields: vec![
                FieldDefinitionExpression::from(Column::from("users.name")),
                FieldDefinitionExpression::from(Column::from("articles.title")),
                FieldDefinitionExpression::from(Column::from("votes.uid")),
            ],
            where_clause: Some(Expression::BinaryOp {
                lhs: Box::new(Expression::BinaryOp {
                    lhs: Box::new(Expression::Column(Column::from("users.id"))),
                    rhs: Box::new(Expression::Column(Column::from("articles.author"))),
                    op: BinaryOperator::Equal,
                }),
                rhs: Box::new(Expression::BinaryOp {
                    lhs: Box::new(Expression::Column(Column::from("votes.aid"))),
                    rhs: Box::new(Expression::Column(Column::from("articles.aid"))),
                    op: BinaryOperator::Equal,
                }),
                op: BinaryOperator::And,
            }),
            ..Default::default()
        });

        let expected: Vec<SubqueryPosition> = Vec::new();

        let res = q.extract_subqueries().unwrap();

        assert_eq!(res, expected);
    }
}
