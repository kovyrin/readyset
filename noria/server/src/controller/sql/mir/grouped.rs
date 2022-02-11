use std::collections::{HashMap, HashSet};

use dataflow::post_lookup::{
    PostLookupAggregate, PostLookupAggregateFunction, PostLookupAggregates,
};
use mir::node::node_inner::MirNodeInner;
use mir::{Column, MirNodeRef};
use nom_sql::analysis::ReferredColumns;
use nom_sql::FunctionExpression::*;
use nom_sql::{self, Expression, FieldDefinitionExpression, SelectStatement};
use noria_errors::{internal, invariant, unsupported, ReadySetError};
use noria_sql_passes::is_aggregate;

use crate::controller::sql::mir::join::make_joins_for_aggregates;
use crate::controller::sql::mir::SqlToMirConverter;
use crate::controller::sql::query_graph::{QueryGraph, QueryGraphEdge};
use crate::ReadySetResult;

// Move predicates above grouped_by nodes
pub(super) fn make_predicates_above_grouped<'a>(
    mir_converter: &SqlToMirConverter,
    name: &str,
    qg: &QueryGraph,
    node_for_rel: &HashMap<&str, MirNodeRef>,
    node_count: usize,
    column_to_predicates: &HashMap<Column, Vec<&'a Expression>>,
    prev_node: &mut Option<MirNodeRef>,
) -> ReadySetResult<(Vec<&'a Expression>, Vec<MirNodeRef>)> {
    let mut created_predicates = Vec::new();
    let mut predicates_above_group_by_nodes = Vec::new();
    let mut node_count = node_count;

    if let Some(computed_cols_cgn) = qg.relations.get("computed_columns") {
        for ccol in &computed_cols_cgn.columns {
            // whenever we have a column getting aggregated (i.e. an over column
            // rather than a group by column) we won't be able to filter on it
            // later, so any filters involving it need to get moved above
            for over_col in
                Expression::Call(ccol.function.as_deref().unwrap().clone()).referred_columns()
            {
                let over_table = over_col
                    .table
                    .as_ref()
                    .ok_or_else(|| ReadySetError::NoSuchColumn(over_col.name.clone()))?
                    .as_str();
                let col = Column::from(over_col.clone());

                if column_to_predicates.contains_key(&col) {
                    let parent = match *prev_node {
                        Some(ref p) => p.clone(),
                        None => node_for_rel[over_table].clone(),
                    };

                    let new_mpns = mir_converter.predicates_above_group_by(
                        &format!("{}_n{}", name, node_count),
                        column_to_predicates,
                        col,
                        parent,
                        &mut created_predicates,
                    )?;

                    node_count += predicates_above_group_by_nodes.len();
                    if !new_mpns.is_empty() {
                        // TODO(ENG-939): updating prev_node here isn't quite right. But moving
                        // this doesn't solve all our problems either. See ticket.
                        *prev_node = Some(new_mpns.last().unwrap().clone());
                        predicates_above_group_by_nodes.extend(new_mpns);
                    }
                }
            }
        }
    }

    Ok((created_predicates, predicates_above_group_by_nodes))
}

/// Normally, projection happens after grouped nodes - however, if aggregates used in grouped
/// expressions reference expressions rather than columns directly, we need to project them out
/// before the grouped nodes.
///
/// This does that projection, and returns a mapping from the expressions themselves to the names of
/// the columns they have been projected to
pub(super) fn make_expressions_above_grouped(
    mir_converter: &SqlToMirConverter,
    name: &str,
    qg: &QueryGraph,
    node_count: usize,
    prev_node: &mut Option<MirNodeRef>,
) -> HashMap<Expression, String> {
    let exprs: Vec<_> = qg
        .relations
        .get("computed_columns")
        .iter()
        .flat_map(|cgn| &cgn.columns)
        .filter_map(|c| c.function.as_ref())
        .filter(|f| is_aggregate(f))
        .flat_map(|f| f.arguments())
        // We don't need to do any work for bare column expressions
        .filter(|arg| !matches!(arg, Expression::Column(_)))
        .map(|expr| (expr.to_string(), expr.clone()))
        .collect();

    if !exprs.is_empty() {
        let cols = prev_node.as_ref().unwrap().borrow().columns.to_vec();

        let node = mir_converter.make_project_node(
            &format!("{}_n{}", name, node_count),
            prev_node.clone().unwrap(),
            cols.iter().collect(),
            exprs.clone(),
            vec![],
            false,
        );
        *prev_node = Some(node);
        exprs.into_iter().map(|(e, n)| (n, e)).collect()
    } else {
        HashMap::new()
    }
}

pub(super) fn make_grouped(
    mir_converter: &SqlToMirConverter,
    name: &str,
    qg: &QueryGraph,
    node_for_rel: &HashMap<&str, MirNodeRef>,
    node_count: usize,
    prev_node: &mut Option<MirNodeRef>,
    projected_exprs: &HashMap<Expression, String>,
) -> ReadySetResult<Vec<MirNodeRef>> {
    let mut agg_nodes: Vec<MirNodeRef> = Vec::new();
    let mut node_count = node_count;

    if let Some(computed_cols_cgn) = qg.relations.get("computed_columns") {
        let gb_edges: Vec<_> = qg
            .edges
            .values()
            .filter(|e| matches!(e, QueryGraphEdge::GroupBy(_)))
            .collect();

        for computed_col in computed_cols_cgn.columns.iter().cloned() {
            // We must also push parameter columns through the group by
            let call_expr = Expression::Call(computed_col.function.as_deref().unwrap().clone());
            let mut over_cols = call_expr.referred_columns().peekable();

            let parent_node = match *prev_node {
                // If no explicit parent node is specified, we extract
                // the base node from the "over" column's specification
                None => {
                    // If we don't have a parent node yet, that means no joins or unions can
                    // have happened yet, which means there *must* only be one table referred in
                    // the aggregate expression. Let's just take the first.
                    node_for_rel[over_cols.peek().unwrap().table.as_ref().unwrap().as_str()].clone()
                }
                // We have an explicit parent node (likely a projection
                // helper), so use that
                Some(ref node) => node.clone(),
            };

            let name = &format!("{}_n{}", name, node_count);

            let (parent_node, group_cols) = if !gb_edges.is_empty() {
                // Function columns with GROUP BY clause
                let mut gb_cols: Vec<&nom_sql::Column> = Vec::new();

                for e in &gb_edges {
                    match **e {
                        QueryGraphEdge::GroupBy(ref gbc) => {
                            let table = gbc.first().unwrap().table.as_ref().unwrap();
                            invariant!(gbc.iter().all(|c| c.table.as_ref().unwrap() == table));
                            gb_cols.extend(gbc);
                        }
                        _ => internal!(),
                    }
                }

                // get any parameter columns that aren't also in the group-by
                // column set
                let param_cols: Vec<_> = qg.relations.values().fold(vec![], |acc, rel| {
                    acc.into_iter()
                        .chain(
                            rel.parameters
                                .iter()
                                .map(|param| &param.col)
                                .filter(|c| !gb_cols.contains(c)),
                        )
                        .collect()
                });
                // combine and dedup
                #[allow(clippy::needless_collect)] // necessary to avoid cloning param_cols
                let dedup_gb_cols: Vec<_> = gb_cols
                    .into_iter()
                    .filter(|gbc| !param_cols.contains(gbc))
                    .collect();
                let gb_and_param_cols = dedup_gb_cols
                    .into_iter()
                    .chain(param_cols.into_iter())
                    .map(Column::from);

                let mut have_parent_cols = HashSet::new();
                // we cannot have duplicate columns at the data-flow level, as it confuses our
                // migration analysis code.
                let gb_and_param_cols = gb_and_param_cols
                    .filter_map(|mut c| {
                        let pn = parent_node.borrow();
                        let pc = pn.columns().iter().position(|pc| *pc == c);
                        if let Some(pc) = pc {
                            if !have_parent_cols.contains(&pc) {
                                have_parent_cols.insert(pc);
                                let pc = pn.columns()[pc].clone();
                                if pc.name != c.name || pc.table != c.table {
                                    // remember the alias with the parent column
                                    c.aliases.push(pc);
                                }
                                Some(c)
                            } else {
                                // we already have this column, so eliminate duplicate
                                None
                            }
                        } else {
                            Some(c)
                        }
                    })
                    .collect();

                (parent_node, gb_and_param_cols)
            } else {
                let proj_cols_from_target_table = over_cols
                    .flat_map(|col| &qg.relations[col.table.as_ref().unwrap()].columns)
                    .map(Column::from)
                    .collect::<Vec<_>>();

                let (group_cols, parent_node) = if proj_cols_from_target_table.is_empty() {
                    // slightly messy hack: if there are no group columns and the
                    // table on which we compute has no projected columns in the
                    // output, we make one up a group column by adding an extra
                    // projection node
                    let proj_name = format!("{}_prj_hlpr", name);
                    let fn_cols: Vec<_> =
                        Expression::Call(computed_col.function.as_deref().unwrap().clone())
                            .referred_columns()
                            .map(|c| Column::from(c.clone()))
                            .collect();
                    // TODO(grfn) this double-collect is really gross- make_projection_helper takes
                    // a Vec<&mir::Column> but we have a Vec<&nom_sql::Column> and there's no way to
                    // make the former from the latter without doing some upsetting allocations
                    let fn_cols = fn_cols.iter().collect();
                    let proj =
                        mir_converter.make_projection_helper(&proj_name, parent_node, fn_cols);

                    agg_nodes.push(proj.clone());
                    node_count += 1;

                    let bogo_group_col = Column::new(None, "grp");
                    (vec![bogo_group_col], proj)
                } else {
                    (proj_cols_from_target_table, parent_node)
                };

                (parent_node, group_cols)
            };

            let nodes: Vec<MirNodeRef> = mir_converter.make_aggregate_node(
                name,
                &Column::from(computed_col),
                group_cols.iter().collect(),
                parent_node.clone(),
                projected_exprs,
            )?;

            node_count += nodes.len();
            agg_nodes.extend(nodes);
        }

        let joinable_agg_nodes = joinable_aggregate_nodes(&agg_nodes);

        if joinable_agg_nodes.len() >= 2 {
            let join_nodes =
                make_joins_for_aggregates(mir_converter, name, &joinable_agg_nodes, node_count)?;
            agg_nodes.extend(join_nodes);
        }

        if !agg_nodes.is_empty() {
            *prev_node = Some(agg_nodes.last().unwrap().clone());
        }
    }

    Ok(agg_nodes)
}

// joinable_aggregate_nodes will take in a list of aggregate nodes and return a list of aggregate
// nodes in the same order they appeared in the input list, and filter out nodes that should not be
// joined. For example, we could see a projection node appear as an aggregate node in the case:
//
// ```
// SELECT 5 * sum(col1)
// ```
//
// The projection node would represent the 5 * being applied to sum(col1), which we would not want
// to accidentally join.
fn joinable_aggregate_nodes(agg_nodes: &[MirNodeRef]) -> Vec<MirNodeRef> {
    agg_nodes
        .iter()
        .filter_map(|node| match node.borrow().inner {
            MirNodeInner::Aggregation { .. } => Some(node.clone()),
            MirNodeInner::Extremum { .. } => Some(node.clone()),
            _ => None,
        })
        .collect()
}

/// Build up the set of [`PostLookupAggregates`] for the given query, given as both the query
/// graph itself and the select statement that the query is built from.
///
/// This function is *not* responsible for determining whether the query *requires* post-lookup
/// aggregation - that's the responsibility of the caller. This function will only return [`None`]
/// if the query contains no aggregates.
pub(super) fn post_lookup_aggregates(
    qg: &QueryGraph,
    stmt: &SelectStatement,
) -> ReadySetResult<Option<PostLookupAggregates<Column>>> {
    if stmt.distinct {
        // DISTINCT is the equivalent of grouping by all projected columns but not actually doing
        // any aggregation function
        return Ok(Some(PostLookupAggregates {
            group_by: stmt
                .fields
                .iter()
                .filter_map(|expr| match expr {
                    FieldDefinitionExpression::Expression {
                        alias: Some(alias), ..
                    } => Some(Column::named(alias.clone())),
                    FieldDefinitionExpression::Expression {
                        expr: Expression::Column(col),
                        ..
                    } => Some(Column::from(col)),
                    FieldDefinitionExpression::Expression { expr, .. } => {
                        Some(Column::named(expr.to_string()))
                    }
                    _ => None,
                })
                .collect(),
            aggregates: vec![],
        }));
    }

    let aggregate_columns = if let Some(qgn) = qg.relations.get("computed_columns") {
        if qgn.columns.is_empty() {
            return Ok(None);
        } else {
            &qgn.columns
        }
    } else {
        return Ok(None);
    };

    let group_by = qg
        .edges
        .values()
        .flat_map(|e| match e {
            QueryGraphEdge::GroupBy(cols) => cols.clone(),
            _ => vec![],
        })
        .map(|col| col.into())
        .collect::<Vec<_>>();

    let mut aggregates = vec![];
    for col in aggregate_columns {
        aggregates.push(PostLookupAggregate {
            column: col.clone().into(),
            function: match col.function.as_deref() {
                Some(Avg { .. }) => {
                    unsupported!("Average is not supported as a post-lookup aggregate")
                }
                // Count and sum are handled the same way, as re-aggregating counts is
                // done by just summing the numbers together
                Some(Count { .. } | CountStar | Sum { .. }) => PostLookupAggregateFunction::Sum,
                Some(Max(_)) => PostLookupAggregateFunction::Max,
                Some(Min(_)) => PostLookupAggregateFunction::Min,
                Some(GroupConcat { separator, .. }) => PostLookupAggregateFunction::GroupConcat {
                    separator: separator.clone(),
                },
                Some(Call { .. }) => continue,
                None => continue,
            },
        });
    }

    Ok(Some(PostLookupAggregates {
        group_by,
        aggregates,
    }))
}
