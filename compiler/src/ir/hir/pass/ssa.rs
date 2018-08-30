//! Lowers the scoping semantics of core erlang into a permissive
//! form of SSA. It is not strict in the way that it does allow a
//! variable to be bound multiple times.
//!
//! While this might seem like a useless exercise at first, it is
//! simpler to convert this into a stricter form of SSA in a later
//! pass, since it dramatically simplifies some other compilation
//! passes. (See pattern match compilation)

use ::std::collections::HashMap;
use ::ir::SSAVariable;
use ::Variable;
use ::ir::hir::{ Expression, SingleExpression, SingleExpressionKind };
use ::util::ssa_variable::SSAVariableGenerator;

use ::ir::hir::scope_tracker::{ ScopeTracker, ScopeDefinition, LambdaEnv,
                                LambdaEnvIdx };

pub fn assign_ssa_expression(env: &mut ScopeTracker, expr: &mut Expression) {
    for single in &mut expr.values {
        assign_ssa_single_expression(env, single);
    }
}

pub fn assign_ssa_single_expression(env: &mut ScopeTracker,
                                    expr: &mut SingleExpression) {
    match expr.kind {
        SingleExpressionKind::Variable(ref mut var) => {
            if let Some(ssa) = env.get(&ScopeDefinition::Variable(var.var.clone())) {
                var.ssa = ssa;
                expr.ssa = ssa;
            } else {
                panic!("variable {} not found in scope", var.var);
            }
        },
        SingleExpressionKind::InterModuleCall { ref mut module, ref mut name, ref mut args } => {
            assign_ssa_single_expression(env, module);
            assign_ssa_single_expression(env, name);
            for arg in args {
                assign_ssa_single_expression(env, arg);
            }
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::Let { ref mut val, ref mut vars, ref mut body } => {
            assign_ssa_expression(env, val);

            let mut scope = HashMap::new();
            for (idx, var) in vars.iter_mut().enumerate() {
                var.ssa = val.values[idx].ssa;
                scope.insert(ScopeDefinition::Variable(var.var.clone()), var.ssa);
            }
            env.push_scope(scope);
            assign_ssa_single_expression(env, body);
            env.pop_scope();
            expr.ssa = body.ssa;
        },
        SingleExpressionKind::ApplyCall { ref mut fun, ref mut args } => {
            for arg in args {
                assign_ssa_single_expression(env, arg);
            }
            assign_ssa_single_expression(env, fun);
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::Try { ref mut body, ref mut then_vars, ref mut then,
                                    ref mut catch_vars, ref mut catch } => {
            assert!(body.values.len() == then_vars.len());

            assign_ssa_expression(env, body);

            let mut scope = HashMap::new();
            for (idx, var) in then_vars.iter_mut().enumerate() {
                var.ssa = body.values[idx].ssa;
                scope.insert(ScopeDefinition::Variable(var.var.clone()), var.ssa);
            }
            env.push_scope(scope);
            assign_ssa_single_expression(env, then);
            env.pop_scope();

            let mut scope = HashMap::new();
            for var in catch_vars.iter_mut() {
                var.ssa = env.new_ssa();
                scope.insert(ScopeDefinition::Variable(var.var.clone()), var.ssa);
            }
            env.push_scope(scope);
            assign_ssa_single_expression(env, catch);
            env.pop_scope();

            expr.ssa = env.new_ssa();
        },
        // TODO
        SingleExpressionKind::Case { ref mut val, ref mut clauses,
                                     ref mut values } => {
            assign_ssa_expression(env, val);

            // Pattern values are not bound to variables, they are not inserted
            // into scope.
            for value in values {
                assign_ssa_single_expression(env, value);
            }

            // Assume that all matches in a pattern can see all variables here.
            // This should be validated later when compiling the pattern.
            for clause in clauses {
                let mut scope = HashMap::new();
                for pattern in clause.patterns.iter_mut() {
                    for &mut (ref var, ref mut ssa) in &mut pattern.binds {
                        *ssa = env.new_ssa();
                        scope.insert(ScopeDefinition::Variable(var.clone()), *ssa);
                    }
                }

                env.push_scope(scope.clone());
                assign_ssa_single_expression(env, &mut clause.guard);
                assign_ssa_single_expression(env, &mut clause.body);
                env.pop_scope();
            }
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::Atomic(_) => {
            expr.ssa = env.new_ssa();
        },
        // TODO
        SingleExpressionKind::NamedFunction { ref name, ref mut is_lambda } => {
            if let Some(ssa) = env.get(&ScopeDefinition::Function(name.var.clone())) {
                *is_lambda = true;
                expr.ssa = ssa;
            } else {
                *is_lambda = false;
                expr.ssa = env.new_ssa();
            }
        },
        SingleExpressionKind::ExternalNamedFunction { .. } => {
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::Tuple(ref mut vals) => {
            for val in vals {
                assign_ssa_single_expression(env, val);
            }
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::List { ref mut head, ref mut tail } => {
            for val in head {
                assign_ssa_single_expression(env, val);
            }
            assign_ssa_single_expression(env, tail);
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::Map { ref mut values, ref mut merge } => {
            for &mut (ref mut key, ref mut val) in values.iter_mut() {
                assign_ssa_single_expression(env, key);
                assign_ssa_single_expression(env, val);
            }
            merge.as_mut().map(|v| assign_ssa_single_expression(env, v));
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::Binary(ref mut elems) => {
            for (ref mut val, ref mut opts) in elems {
                assign_ssa_single_expression(env, val);
                for ref mut opt in opts {
                    assign_ssa_single_expression(env, opt);
                }
            }
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::PrimOp { ref mut args, .. } => {
            for arg in args {
                assign_ssa_single_expression(env, arg);
            }
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::Do(ref mut e1, ref mut e2) => {
            assign_ssa_expression(env, e1);
            assign_ssa_single_expression(env, e2);
            expr.ssa = e2.ssa;
        },
        SingleExpressionKind::Receive { ref mut clauses, ref mut pattern_values,
                                        ref mut timeout_time,
                                        ref mut timeout_body } => {
            for value in pattern_values {
                assign_ssa_single_expression(env, value);
            }

            for clause in clauses {
                let mut scope = HashMap::new();
                for pattern in clause.patterns.iter_mut() {
                    for &mut (ref var, ref mut ssa) in &mut pattern.binds {
                        *ssa = env.new_ssa();
                        scope.insert(ScopeDefinition::Variable(var.clone()), *ssa);
                    }
                }
                env.push_scope(scope);
                assign_ssa_single_expression(env, &mut clause.guard);
                assign_ssa_single_expression(env, &mut clause.body);
                env.pop_scope();
            }
            assign_ssa_single_expression(env, timeout_time);
            assign_ssa_single_expression(env, timeout_body);
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::BindClosure { ref mut closure, ref mut lambda_env,
                                            ref mut env_ssa } => {
            env.push_tracking();

            let mut scope = HashMap::new();
            for arg in &mut closure.fun.as_mut().unwrap().args {
                arg.ssa = env.new_ssa();
                scope.insert(ScopeDefinition::Variable(arg.var.clone()), arg.ssa);
            }
            env.push_scope(scope);

            assign_ssa_single_expression(
                env, &mut closure.fun.as_mut().unwrap().body);

            env.pop_scope();
            let captures_map = env.pop_tracking();
            let captures = captures_map.iter()
                .map(|(k, &(o, i))| (k.clone(), o, i))
                .collect();

            let env_idx = env.add_lambda_env(LambdaEnv {
                captures: captures,
                meta_binds: vec![], // TODO
            });

            *lambda_env = Some(env_idx);
            closure.env = *lambda_env;

            *env_ssa = env.new_ssa();
            expr.ssa = env.new_ssa();
        },
        SingleExpressionKind::BindClosures { ref mut closures, ref mut body,
                                             ref mut lambda_env, ref mut env_ssa } => {

            let mut closures_scope = HashMap::new();
            for closure in closures.iter_mut() {
                let alias = closure.alias.as_mut().unwrap();
                alias.ssa = env.new_ssa();
                closures_scope.insert(
                    ScopeDefinition::Function(alias.var.clone()),
                    alias.ssa.clone());
            }
            env.push_scope(closures_scope);

            env.push_tracking();
            for closure in closures.iter_mut() {
                let mut scope = HashMap::new();
                for arg in &mut closure.fun.as_mut().unwrap().args {
                    arg.ssa = env.new_ssa();
                    scope.insert(ScopeDefinition::Variable(arg.var.clone()), arg.ssa);
                }
                env.push_scope(scope);
                assign_ssa_single_expression(
                    env, &mut closure.fun.as_mut().unwrap().body);
                env.pop_scope();
            }

            let captures_map = env.pop_tracking();
            let captures = captures_map.iter()
                .map(|(k, &(o, i))| (k.clone(), o, i))
                .collect();

            let env_idx = env.add_lambda_env(LambdaEnv {
                captures: captures,
                meta_binds: vec![], // TODO: Meta binds
            });

            *lambda_env = Some(env_idx);
            for closure in closures.iter_mut() {
                closure.env = *lambda_env;
            }

            assign_ssa_single_expression(env, body);
            env.pop_scope();

            *env_ssa = env.new_ssa();
            expr.ssa = env.new_ssa();
        },
        ref e => panic!("Unhandled: {:?}", e),
    }
}
