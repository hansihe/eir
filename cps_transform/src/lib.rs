//! When we have a way to calculate live values in the input IR, this transform
//! is actually pretty easy.
//! For every function call site, we generate two closures, one for the exception
//! branch and one for the ok branch.
//! These closures gets passed into the call to the function as the first two
//! arguments.
//! The result is that every function signature gets two extra arguments after
//! the transform has completed. Argument 0 is the ok continuation, argument
//! 1 is the error continuation.
//!
//! TODO
//! Right now the input continuations are manually injected into every closure
//! inside the function. This is not optimal if the function terminates
//! without calling a continuation. This should be relatively rare, so this is
//! probably not a big deal.
//!
//! ## Generated functions
//! It should be noted that arguments only get added to the entry EBB. The
//! Function ident is identical, but with a new lambda env.
//!
//! There are 3 different kinds of functions generated by this transformation:
//! 1. Functions. The entry EBB arity gets gets two arguments added to the
//!    front, the OK continuation and the ERR continuation. Both of these
//!    need to be of function type 3.
//! 2. Closures. These functions take an env as the first argument, as before.
//!    The second and third argument are the OK and ERR continuations.
//! 3. Continuations. These functions have the ident of a closure. They take
//!    an env as the first argument, and the return value as the second argument.
//!    They are always of arity 2.

use std::collections::{ HashMap, HashSet, VecDeque };

use eir::{ Module, Function, FunctionBuilder, Dialect };
use eir::FunctionIdent;
use eir::op::{ OpKind, CallType };
use eir::{ ModuleEnvs, ClosureEnv };
use eir::{ Ebb, Op, Value, EbbCall };
use eir::fun::live::LiveValues;
use eir::{ AttributeKey, AttributeValue };

fn copy_op(
    src_fun: &Function,
    src_op: Op,
    b: &mut FunctionBuilder,
    // Map from source function => dest function
    val_map: &mut HashMap<Value, Value>,
    ebb_map: &mut HashMap<Op, Ebb>,
) {
    let kind = src_fun.op_kind(src_op);
    println!("Copy: {:?}", kind);
    b.op_build_start(kind.clone());

    for write in src_fun.op_writes(src_op) {
        let new = b.op_build_write();
        val_map.insert(*write, new);
    }
    for read in src_fun.op_reads(src_op) {
        if src_fun.value_is_constant(*read) {
            let value = b.create_constant(src_fun.value_constant(*read).clone());
            b.op_build_read(value);
        } else {
            b.op_build_read(val_map[read]);
        }
    }
    for branch in src_fun.op_branches(src_op) {
        let old_target = src_fun.ebb_call_target(*branch);
        let old_target_op = src_fun.ebb_first_op(old_target);

        //O: let new = if let Some(ebb) = ebb_map.get(&old_target) {
        let new = if let Some(ebb) = ebb_map.get(&old_target_op) {
            *ebb
        } else {
            let new = b.insert_ebb();
            //O: ebb_map.insert(old_target, new);
            assert!(!ebb_map.contains_key(&old_target_op));
            ebb_map.insert(old_target_op, new);
            for arg in src_fun.ebb_args(old_target) {
                let val = b.add_ebb_argument(new);
                val_map.insert(*arg, val);
            }
            new
        };

        let mut buf = Vec::new();
        for arg in src_fun.ebb_call_args(*branch) {
            if src_fun.value_is_constant(*arg) {
                let value = b.create_constant(src_fun.value_constant(*arg).clone());
                buf.push(value);
            } else {
                buf.push(val_map[arg]);
            }
        }
        let call = b.create_ebb_call(new, &buf);
        b.op_build_ebb_call(call);
    }

    if let Some(op) = src_fun.op_after(src_op) {
        ebb_map.insert(op, b.current_ebb());
    }

    b.op_build_end();
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
enum ContSite {
    EbbCall(EbbCall, Option<Value>),
    Op(Op),
}

fn copy_read(src_fun: &Function, b: &mut FunctionBuilder,
             val_map: &HashMap<Value, Value>,
             src_val: Value) -> Value {
    if src_fun.value_is_constant(src_val) {
        let value = b.create_constant(
            src_fun.value_constant(src_val).clone());
        value
    } else {
        val_map[&src_val]
    }
}

fn gen_chunk(
    src_fun: &Function,
    site: ContSite,
    cont_sites: &HashSet<Op>,
    live: &LiveValues,
    env_idx_gen: &mut ModuleEnvs,
    needed_continuations: &mut Vec<(ContSite, ClosureEnv)>,
    continuaitons: &mut HashMap<ContSite, ClosureEnv>,
    // If this is false, this is an entry point
    // If this is true, this is a continuation
    cont: Option<ClosureEnv>,
) -> Function {
    println!("Chunk");

    let mut ident = src_fun.ident().clone();
    if let Some(env_idx) = cont {
        ident.lambda = Some((env_idx, 0));
    }

    let mut fun = Function::new(ident, Dialect::CPS);

    {
        let mut b = FunctionBuilder::new(&mut fun);

        if cont.is_some() {
            b.put_attribute(AttributeKey::Continuation, AttributeValue::None);
        }

        let mut to_process = VecDeque::new();
        let mut val_map = HashMap::new();
        let mut ebb_map: HashMap<Op, Ebb> = HashMap::new();
        let mut handled_ops = HashSet::new();
        // Temp
        let mut call_renames: HashMap<Value, Value> = HashMap::new();

        // Add entry ebb
        let entry_ebb = b.insert_ebb_entry();
        b.position_at_end(entry_ebb);

        let ok_ret_cont;
        let err_ret_cont;

        let src_first_op;

        if cont.is_some() {
            // This is a continuation

            let result_src_val;

            let mut env_vals = Vec::new();
            match site {
                ContSite::Op(op) => {
                    // We entered the continuation from flow
                    let prev_op = src_fun.op_before(op).unwrap();
                    let live_vals = &live.flow_live[&prev_op];
                    let result_src_val_i = src_fun.op_writes(prev_op)[0];
                    result_src_val = Some(result_src_val_i);
                    for src_live in live_vals.iter(&live.pool) {
                        if src_live == result_src_val_i {
                            continue
                        }
                        env_vals.push(src_live);
                    }
                }
                ContSite::EbbCall(call, result_after_branch) => {
                    // We entered the continuation from a jump
                    result_src_val = result_after_branch;
                    let call_source = src_fun.ebb_call_source(call);
                    let call_target = src_fun.ebb_call_target(call);
                    let live_vals = &live.ebb_live[&call_target];
                    let src_result_before_val = src_fun.op_writes(call_source)[1];
                    for src_after_live in live_vals.iter(&live.pool) {
                        assert!(src_result_before_val != src_after_live);
                        if Some(src_after_live) == result_after_branch {
                            continue
                        }
                        env_vals.push(src_after_live);
                    }
                }
            }

            // Get first op and its ebb
            src_first_op = match site {
                ContSite::Op(op) => op,
                ContSite::EbbCall(call, _) => {
                    let target = src_fun.ebb_call_target(call);
                    src_fun.ebb_first_op(target)
                },
            };
            let src_first_ebb = src_fun.op_ebb(src_first_op);
            //C: ebb_map.insert(src_first_ebb, entry_ebb);
            ebb_map.insert(src_first_op, entry_ebb);

            // Argument for environment
            let env_val = b.add_ebb_argument(entry_ebb);

            // Argument for result
            let res_val = b.add_ebb_argument(entry_ebb);
            if let Some(v) = result_src_val {
                val_map.insert(v, res_val);
            }

            let mut new_env_vars = Vec::new();
            // +2 for ok and err function continuations
            b.op_unpack_env(env_val, env_vals.len() + 2, &mut new_env_vars);

            // Continuations
            ok_ret_cont = new_env_vars[0];
            err_ret_cont = new_env_vars[1];

            // Insert mappings for all in env
            for (src, dst) in env_vals.iter().zip(new_env_vars.iter().skip(2)) {
                val_map.insert(*src, *dst);
            }

        } else {
            // This is a entry point

            // Get Op and Ebb, insert binding
            src_first_op = if let ContSite::Op(op) = site { op } else { panic!() };
            let src_first_ebb = src_fun.op_ebb(src_first_op);
            //C: ebb_map.insert(src_first_ebb, entry_ebb);
            ebb_map.insert(src_first_op, entry_ebb);

            // Arguments for continuations
            ok_ret_cont = b.add_ebb_argument(entry_ebb);
            err_ret_cont = b.add_ebb_argument(entry_ebb);

            // Entry Ebb arguments, insert bindings
            for arg in src_fun.ebb_args(src_first_ebb) {
                let val = b.add_ebb_argument(entry_ebb);
                val_map.insert(*arg, val);
            }

        }

        // Seed op
        to_process.push_back(src_first_op);

        while to_process.len() > 0 {
            let src_op = to_process.pop_front().unwrap();

            if handled_ops.contains(&src_op) { continue; }
            handled_ops.insert(src_op);

            let src_ebb = src_fun.op_ebb(src_op);
            b.position_at_end(ebb_map[&src_op]);
            println!("src: {:?} dst {:?}", src_ebb, ebb_map[&src_op]);

            // If we hit a continuation site
            if cont_sites.contains(&src_op) {
                let kind = src_fun.op_kind(src_op);
                println!("{:?}", kind);

                let is_tail;
                match kind {
                    OpKind::Apply { call_type: CallType::Normal } => is_tail = false,
                    OpKind::Call { call_type: CallType::Normal, .. } => is_tail = false,
                    OpKind::Apply { call_type: CallType::Tail } => is_tail = true,
                    OpKind::Call { call_type: CallType::Tail, .. } => is_tail = true,
                    _ => panic!(),
                }

                let mut buf = Vec::new();

                let writes = src_fun.op_writes(src_op);
                if is_tail {
                    assert!(writes.len() == 0);
                } else {
                    assert!(writes.len() == 2);
                }

                let ok_cont;
                let err_cont;

                if !is_tail {
                    let ok_val = writes[0];
                    let nok_val = writes[1];

                    // =========================
                    // ==== Ok continuation ====
                    // =========================

                    // Live variables at the control flow edge
                    let ok_live = &live.flow_live[&src_op];

                    // Construct the closure environment for the continuation
                    buf.clear();
                    buf.push(ok_ret_cont);
                    buf.push(err_ret_cont);
                    for live in ok_live.iter(&live.pool) {
                        if live == ok_val {
                            continue;
                        }
                        buf.push(val_map[&live]);
                    }

                    let src_next_op = src_fun.op_after(src_op).unwrap();
                    let cont_site = ContSite::Op(src_next_op);
                    let env_idx = if let Some(env_idx) =
                        continuaitons.get(&cont_site)
                    {
                        *env_idx
                    } else {
                        let env_idx = env_idx_gen.add();
                        env_idx_gen.env_set_captures_num(env_idx, buf.len());

                        continuaitons.insert(cont_site, env_idx);

                        // Schedule control flow edge for continuation generation
                        needed_continuations.push(
                            (ContSite::Op(src_next_op), env_idx));

                        env_idx
                    };
                    let env = b.op_make_closure_env(env_idx, &buf);

                    // Bind closure for continuation
                    let mut ident = src_fun.ident().clone();
                    ident.lambda = Some((env_idx, 0));
                    ok_cont = b.op_bind_closure(ident, env);

                    // ============================
                    // ==== Throw continuation ====
                    // ============================

                    // Live variables at the exception edge
                    // if this is not a tail call
                    let err_live;
                    let src_branch = src_fun.op_branches(src_op)[0];
                    let src_target = src_fun.ebb_call_target(src_branch);
                    err_live = &live.ebb_live[&src_target];

                    // Generate rename map.
                    // Maps values after the ebb call to values before
                    call_renames.clear();
                    for (from, to) in src_fun.ebb_call_args(src_branch).iter()
                        .zip(src_fun.ebb_args(src_target).iter())
                    {
                        call_renames.insert(*to, *from);
                    }

                    let mut renamed_nok_val = None;

                    // Construct the closure environment for the continuation
                    buf.clear();
                    buf.push(ok_ret_cont);
                    buf.push(err_ret_cont);
                    for live in err_live.iter(&live.pool) {
                        let renamed = call_renames.get(&live).cloned().unwrap_or(live);
                        if renamed == nok_val {
                            renamed_nok_val = Some(live);
                            continue;
                        }
                        buf.push(val_map[&renamed]);
                    }

                    let cont_site = ContSite::EbbCall(src_branch, renamed_nok_val);
                    let env_idx = if let Some(env_idx) =
                        continuaitons.get(&cont_site)
                    {
                        *env_idx
                    } else {
                        let env_idx = env_idx_gen.add();
                        env_idx_gen.env_set_captures_num(env_idx, buf.len());

                        continuaitons.insert(cont_site, env_idx);

                        // Schedule exception edge for continuation generation
                        needed_continuations.push((
                            ContSite::EbbCall(src_branch, renamed_nok_val), env_idx));

                        env_idx
                    };
                    let env = b.op_make_closure_env(env_idx, &buf);

                    // Bind closure for continuation
                    let mut ident = src_fun.ident().clone();
                    ident.lambda = Some((env_idx, 0));
                    err_cont = b.op_bind_closure(ident, env);

                } else {
                    // In the case of a tail call, don't create a new return
                    // continuation, instead do a tail call with the return
                    // continuations passed to the function as arguments.
                    ok_cont = ok_ret_cont;
                    err_cont = err_ret_cont;
                }

                // =======================
                // ==== Function call ====
                // =======================

                buf.clear();
                buf.push(ok_cont);
                buf.push(err_cont);
                match kind {
                    OpKind::Apply { call_type } => {
                        assert!(*call_type == CallType::Normal
                                || *call_type == CallType::Tail);
                        let reads = src_fun.op_reads(src_op);

                        for read in reads.iter().skip(1) {
                            buf.push(copy_read(src_fun, &mut b, &val_map, *read));
                        }

                        println!("Apply");
                        b.op_tail_apply(val_map[&reads[0]], &buf);
                    },
                    OpKind::Call { call_type, arity } => {
                        assert!(*call_type == CallType::Normal
                                || *call_type == CallType::Tail);
                        let reads = src_fun.op_reads(src_op);

                        for read in reads.iter().skip(2) {
                            println!("{:?}", read);
                            buf.push(copy_read(src_fun, &mut b, &val_map, *read));
                        }

                        let name_val = copy_read(src_fun, &mut b, &val_map, reads[0]);
                        let module_val = copy_read(src_fun, &mut b, &val_map, reads[1]);

                        println!("Call");
                        b.op_tail_call(name_val, module_val, *arity, &buf);
                    },
                    _ => panic!(),
                }

                // Do not copy the current op, continue processing queue
                continue;
            }

            let kind = src_fun.op_kind(src_op);
            match kind {
                // Call the return continuation
                OpKind::ReturnOk => {
                    b.position_at_end(ebb_map[&src_op]);
                    let res = src_fun.op_reads(src_op)[0];
                    println!("ReturnOk");
                    b.op_cont_apply(ok_ret_cont, &[val_map[&res]]);
                },
                // Call the throw continuation
                OpKind::ReturnThrow => {
                    b.position_at_end(ebb_map[&src_op]);
                    let res = src_fun.op_reads(src_op)[0];
                    println!("ReturnErr");
                    b.op_cont_apply(err_ret_cont, &[val_map[&res]]);
                },
                // If this is a normal Op, copy it and add outgoing edges to
                // processing queue
                _ => {
                    copy_op(src_fun, src_op, &mut b, &mut val_map, &mut ebb_map);

                    // Add outgoing edges to processing queue
                    if let Some(next_op) = src_fun.op_after(src_op) {
                        to_process.push_back(next_op);
                    }
                    for branch in src_fun.op_branches(src_op) {
                        let target = src_fun.ebb_call_target(*branch);
                        let first_op = src_fun.ebb_first_op(target);
                        to_process.push_back(first_op);
                    }

                },
            }

        }

    }

    fun
}

pub fn transform_module(module: &Module) -> Module {
    let mut funs = HashMap::new();
    let mut env_gen = module.envs.clone();

    // Doing this to get deterministic ordering
    let mut fun_idents: Vec<_> = module.functions.keys().collect();
    fun_idents.sort();

    for ident in fun_idents.iter() {
        let fun = &module.functions[ident];
        transform_function(fun, &mut env_gen, &mut funs);
    }

    Module {
        name: module.name.clone(),
        functions: funs,
        envs: env_gen,
    }
}

pub fn transform_function(
    src_fun: &Function,
    env_idx_gen: &mut ModuleEnvs,
    result_functions: &mut HashMap<FunctionIdent, Function>,
) {
    let live = src_fun.live_values();

    println!("{}", src_fun.ident());

    // Identify continuation sites
    let mut cont_sites = HashSet::new();
    for ebb in src_fun.iter_ebb() {
        for op in src_fun.iter_op(ebb) {
            let kind = src_fun.op_kind(op);
            match kind {
                OpKind::Call { .. } => {
                    cont_sites.insert(op);
                },
                OpKind::Apply { .. } => {
                    cont_sites.insert(op);
                },
                _ => (),
            }
        }
    }
    //for op in live.flow_live.keys() {
    //    let kind = src_fun.op_kind(*op);
    //    match kind {
    //        OpKind::Call { .. } => {
    //            cont_sites.insert(*op);
    //        },
    //        OpKind::Apply { .. } => {
    //            cont_sites.insert(*op);
    //        },
    //        _ => (),
    //    }
    //}

    let mut generated = HashSet::new();
    let mut generated2 = HashSet::new();

    let mut needed = Vec::new();
    let mut needed_map = HashMap::new();

    let entry = src_fun.ebb_entry();
    let fun = gen_chunk(
        src_fun,
        ContSite::Op(src_fun.ebb_first_op(entry)),
        &cont_sites,
        &live,
        env_idx_gen,
        &mut needed,
        &mut needed_map,
        None,
    );
    result_functions.insert(fun.ident().clone(), fun);

    while needed.len() > 0 {
        let (site, env) = needed.pop().unwrap();
        println!("Site {:?}", site);
        println!("Done {:?}", generated);
        if let ContSite::EbbCall(call, val) = site {
            if generated.contains(&(call, val)) { continue }
            generated.insert((call, val));
        }
        if let ContSite::Op(op) = site {
            if generated2.contains(&op) {
                continue;
            }
            generated2.insert(op);
        }

        println!("StartChunk: {:?} {:?}", site, env);
        let fun = gen_chunk(
            src_fun,
            site,
            &cont_sites,
            &live,
            env_idx_gen,
            &mut needed,
            &mut needed_map,
            Some(env),
        );
        result_functions.insert(fun.ident().clone(), fun);
    }

}
