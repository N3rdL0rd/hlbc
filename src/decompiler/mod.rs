//! The decompiler used to get haxe sources back from the bytecode definitions.
//! More info on how everything works in the [wiki](https://github.com/Gui-Yom/hlbc/wiki/Decompilation).
//!
//! The decompiler takes bytecode elements as input and outputs [ast] structures that can be displayed.

use std::collections::{HashMap, HashSet};

use ast::*;
use scopes::*;

use crate::types::{FunPtr, Function, RefField, Reg, Type, TypeObj};
use crate::Bytecode;
use crate::Opcode;

/// A simple representation for the Haxe source code generated by the decompiler
pub mod ast;
/// Functions to render the [ast] to a string
pub mod fmt;
/// Scope handling structures
mod scopes;

enum ExprCtx {
    Constructor {
        reg: Reg,
        pos: usize,
    },
    Anonymous {
        pos: usize,
        fields: HashMap<RefField, Expr>,
        remaining: usize,
    },
}

/// Decompile a function to a list of [Statement]s.
/// This works by analyzing each opcodes in order while trying to construct contexts and intents.
pub fn decompile_function(code: &Bytecode, f: &Function) -> Vec<Statement> {
    // Scope stack, holds the statements
    let mut scopes = Scopes::new();
    // Current iteration statement, to be pushed onto the finished statements or the nesting
    //let mut statement = None;
    // Expression values for each registers
    let mut reg_state = HashMap::with_capacity(f.regs.len());
    // For parsing statements made of multiple instructions like constructor calls and anonymous structures
    // TODO move this to another pass on the generated ast
    let mut expr_ctx = Vec::new();
    // Variable names we already declared
    let mut seen = HashSet::new();

    let mut start = 0;
    // First argument / First register is 'this'
    if f.is_method()
        || f.name
            .map(|n| n.resolve(&code.strings) == "__constructor__")
            .unwrap_or(false)
    {
        reg_state.insert(Reg(0), cst_this());
        start = 1;
    }

    // Initialize register state with the function arguments
    for i in start..f.ty(code).args.len() {
        let name = f.arg_name(code, i - start).map(ToOwned::to_owned);
        reg_state.insert(Reg(i as u32), Expr::Variable(Reg(i as u32), name.clone()));
        if let Some(name) = name {
            seen.insert(name);
        }
    }

    macro_rules! push_stmt {
        ($stmt:expr) => {
            //statement = Some($stmt);
            scopes.push_stmt($stmt);
        };
    }

    // Update the register state and create a statement depending on inline rules
    macro_rules! push_expr {
        ($i:expr, $dst:expr, $e:expr) => {
            let name = f.var_name(code, $i);
            let expr = $e;
            // Inline check
            if name.is_none() {
                reg_state.insert($dst, expr);
            } else {
                reg_state.insert($dst, Expr::Variable($dst, name.clone()));
                push_stmt!(Statement::Assign {
                    declaration: seen.insert(name.clone().unwrap()),
                    variable: Expr::Variable($dst, name),
                    assign: expr,
                });
            }
        };
    }

    let missing_expr = || Expr::Unknown("missing expr".to_owned());

    // Get the expr for a register
    macro_rules! expr {
        ($reg:expr) => {
            reg_state.get(&$reg).cloned().unwrap_or_else(missing_expr)
        };
    }

    // Crate a Vec<Expression> for a list of args
    macro_rules! make_args {
        ($($arg:expr),* $(,)?) => {
            vec![$(expr!($arg)),*]
        }
    }

    macro_rules! push_call {
        ($i:ident, $dst:ident, $fun:ident, $arg0:expr $(, $args:expr)*) => {
            if let Some(&ExprCtx::Constructor { reg, pos }) = expr_ctx.last() {
                if reg == $arg0 {
                    push_expr!(
                        pos,
                        reg,
                        Expr::Constructor(ConstructorCall::new(f.regtype(reg), make_args!($($args),*)))
                    );
                    expr_ctx.pop();
                }
            } else {
                match $fun.resolve(code) {
                    FunPtr::Fun(func) => {
                        let call = if func.is_method() {
                            call(Expr::Field(Box::new(expr!($arg0)), func.name.clone().unwrap().resolve(&code.strings).to_owned()), make_args!($($args),*))
                        } else {
                            call_fun($fun, make_args!($arg0 $(, $args)*))
                        };
                        if func.ty(code).ret.is_void() {
                            push_stmt!(stmt(call));
                        } else {
                            push_expr!($i, $dst, call);
                        }
                    }
                    FunPtr::Native(n) => {
                        let call = call_fun($fun, make_args!($arg0 $(, $args)*));
                        if n.ty(code).ret.is_void() {
                            push_stmt!(stmt(call));
                        } else {
                            push_expr!($i, $dst, call);
                        }
                    }
                }
            }
        };
    }

    // Process a jmp instruction, might be the exit condition of a loop or an if
    macro_rules! push_jmp {
        ($i:ident, $offset:ident, $cond:expr) => {
            if $offset > 0 {
                let cond = $cond;
                // It's a loop
                if matches!(f.ops[$i + $offset as usize], Opcode::JAlways { offset } if offset < 0) {
                    if let Some(loop_cond) = scopes.update_last_loop_cond() {
                        if matches!(loop_cond, Expr::Unknown(_)) {
                            println!("old loop cond : {:?}", loop_cond);
                            *loop_cond = cond;
                        } else {
                            scopes.push_if($offset + 1, cond);
                        }
                    } else {
                        scopes.push_if($offset + 1, cond);
                    }
                } else {
                    // It's an if
                    scopes.push_if($offset + 1, cond);
                }
            }
        }
    }

    let iter = f.ops.iter().enumerate();
    for (i, o) in iter {
        // Opcodes are grouped by semantic
        // Control flow first because they are the most important
        match o {
            //region CONTROL FLOW
            &Opcode::JTrue { cond, offset } => {
                push_jmp!(i, offset, not(expr!(cond)))
            }
            &Opcode::JFalse { cond, offset } => {
                push_jmp!(i, offset, expr!(cond))
            }
            &Opcode::JNull { reg, offset } => {
                push_jmp!(i, offset, noteq(expr!(reg), cst_null()))
            }
            &Opcode::JNotNull { reg, offset } => {
                push_jmp!(i, offset, eq(expr!(reg), cst_null()))
            }
            &Opcode::JSGte { a, b, offset } | &Opcode::JUGte { a, b, offset } => {
                push_jmp!(i, offset, gt(expr!(b), expr!(a)))
            }
            &Opcode::JSGt { a, b, offset } => {
                push_jmp!(i, offset, gte(expr!(b), expr!(a)))
            }
            &Opcode::JSLte { a, b, offset } => {
                push_jmp!(i, offset, lt(expr!(b), expr!(a)))
            }
            &Opcode::JSLt { a, b, offset } | &Opcode::JULt { a, b, offset } => {
                push_jmp!(i, offset, lte(expr!(b), expr!(a)))
            }
            &Opcode::JEq { a, b, offset } => {
                push_jmp!(i, offset, noteq(expr!(a), expr!(b)))
            }
            &Opcode::JNotEq { a, b, offset } => {
                push_jmp!(i, offset, eq(expr!(a), expr!(b)))
            }
            // Unconditional jumps can actually mean a lot of things
            &Opcode::JAlways { offset } => {
                if offset < 0 {
                    // It's either the jump backward of a loop or a continue statement
                    let loop_start = scopes
                        .last_loop_start()
                        .expect("Backward jump but we aren't in a loop ?");

                    // Scan the next instructions in order to find another jump to the same place
                    if f.ops.iter().enumerate().skip(i + 1).find_map(|(j, o)| {
                        // We found another jump to the same place !
                        if matches!(o, Opcode::JAlways {offset} if (j as i32 + offset + 1) as usize == loop_start) {
                            Some(true)
                        } else {
                            None
                        }
                    }).unwrap_or(false) {
                        // If this jump is not the last jump backward for the current loop, so it's definitely a continue; statement
                        push_stmt!(Statement::Continue);
                    } else {
                        // It's the last jump backward of the loop, which means the end of the loop
                        // we generate the loop statement
                        if let Some(stmt) = scopes.end_last_loop() {
                            push_stmt!(stmt);
                        } else {
                            panic!("Last scope is not a loop !");
                        }
                    }
                } else {
                    if let Some(offsets) = scopes.last_is_switch_ctx() {
                        if let Some(pos) = offsets.iter().position(|o| *o == i) {
                            scopes.push_switch_case(pos);
                        } else {
                            panic!("no matching offset for switch case ({i})");
                        }
                    } else if scopes.last_loop_start().is_some() {
                        // Check the instruction just before the jump target
                        // If it's a jump backward of a loop
                        if matches!(f.ops[(i as i32 + offset) as usize], Opcode::JAlways {offset} if offset < 0)
                        {
                            // It's a break condition
                            push_stmt!(Statement::Break);
                        }
                    } else if scopes.last_is_if() {
                        // It's the jump over of an else clause
                        scopes.push_else(offset + 1);
                    } else {
                        println!("JAlways > 0 with no matching scope ?");
                    }
                }
            }
            Opcode::Switch { reg, offsets, end } => {
                // Convert to absolute positions
                scopes.push_switch(
                    *end + 1,
                    expr!(reg),
                    offsets.iter().map(|o| i + *o as usize).collect(),
                );
                // The default switch case is implicit
            }
            &Opcode::Label => scopes.push_loop(i),
            &Opcode::Ret { ret } => {
                // Do not display return void; only in case of an early return
                if scopes.has_scopes() {
                    push_stmt!(Statement::Return(if f.regtype(ret).is_void() {
                        None
                    } else {
                        Some(expr!(ret))
                    }));
                } else if !f.regtype(ret).is_void() {
                    push_stmt!(Statement::Return(Some(expr!(ret))));
                }
            }
            //endregion

            //region EXCEPTIONS
            &Opcode::Throw { exc } | &Opcode::Rethrow { exc } => {
                push_stmt!(Statement::Throw(expr!(exc)));
            }
            &Opcode::Trap { exc, offset } => {
                scopes.push_try(offset + 1);
            }
            &Opcode::EndTrap { exc } => {
                // TODO try catch
            }
            //endregion

            //region CONSTANTS
            &Opcode::Int { dst, ptr } => {
                push_expr!(i, dst, cst_int(ptr.resolve(&code.ints)));
            }
            &Opcode::Float { dst, ptr } => {
                push_expr!(i, dst, cst_float(ptr.resolve(&code.floats)));
            }
            &Opcode::Bool { dst, value } => {
                push_expr!(i, dst, cst_bool(value.0));
            }
            &Opcode::String { dst, ptr } => {
                push_expr!(i, dst, cst_refstring(ptr, code));
            }
            &Opcode::Null { dst } => {
                push_expr!(i, dst, cst_null());
            }
            //endregion

            //region OPERATORS
            &Opcode::Mov { dst, src } => {
                push_expr!(i, dst, expr!(src));
                // Workaround for when the instructions after this one use dst and src interchangeably.
                reg_state.insert(src, Expr::Variable(dst, f.var_name(code, i)));
            }
            &Opcode::Add { dst, a, b } => {
                push_expr!(i, dst, add(expr!(a), expr!(b)));
            }
            &Opcode::Sub { dst, a, b } => {
                push_expr!(i, dst, sub(expr!(a), expr!(b)));
            }
            &Opcode::Mul { dst, a, b } => {
                push_expr!(i, dst, mul(expr!(a), expr!(b)));
            }
            &Opcode::SDiv { dst, a, b } | &Opcode::UDiv { dst, a, b } => {
                push_expr!(i, dst, div(expr!(a), expr!(b)));
            }
            &Opcode::SMod { dst, a, b } | &Opcode::UMod { dst, a, b } => {
                push_expr!(i, dst, modulo(expr!(a), expr!(b)));
            }
            &Opcode::Shl { dst, a, b } => {
                push_expr!(i, dst, shl(expr!(a), expr!(b)));
            }
            &Opcode::SShr { dst, a, b } | &Opcode::UShr { dst, a, b } => {
                push_expr!(i, dst, shr(expr!(a), expr!(b)));
            }
            &Opcode::And { dst, a, b } => {
                push_expr!(i, dst, and(expr!(a), expr!(b)));
            }
            &Opcode::Or { dst, a, b } => {
                push_expr!(i, dst, or(expr!(a), expr!(b)));
            }
            &Opcode::Xor { dst, a, b } => {
                push_expr!(i, dst, xor(expr!(a), expr!(b)));
            }
            &Opcode::Neg { dst, src } => {
                push_expr!(i, dst, neg(expr!(src)));
            }
            &Opcode::Not { dst, src } => {
                push_expr!(i, dst, not(expr!(src)));
            }
            &Opcode::Incr { dst } => {
                // FIXME sometimes it should be an expression
                push_stmt!(stmt(incr(expr!(dst))));
            }
            &Opcode::Decr { dst } => {
                push_stmt!(stmt(decr(expr!(dst))));
            }
            //endregion

            //region CALLS
            &Opcode::Call0 { dst, fun } => {
                if fun.ty(code).ret.is_void() {
                    push_stmt!(stmt(call_fun(fun, Vec::new())));
                } else {
                    push_expr!(i, dst, call_fun(fun, Vec::new()));
                }
            }
            &Opcode::Call1 { dst, fun, arg0 } => {
                push_call!(i, dst, fun, arg0)
            }
            &Opcode::Call2 {
                dst,
                fun,
                arg0,
                arg1,
            } => {
                push_call!(i, dst, fun, arg0, arg1)
            }
            &Opcode::Call3 {
                dst,
                fun,
                arg0,
                arg1,
                arg2,
            } => {
                push_call!(i, dst, fun, arg0, arg1, arg2)
            }
            &Opcode::Call4 {
                dst,
                fun,
                arg0,
                arg1,
                arg2,
                arg3,
            } => {
                push_call!(i, dst, fun, arg0, arg1, arg2, arg3)
            }
            Opcode::CallN { dst, fun, args } => {
                if let Some(&ExprCtx::Constructor { reg, pos }) = expr_ctx.last() {
                    if reg == args[0] {
                        push_expr!(
                            pos,
                            reg,
                            Expr::Constructor(ConstructorCall::new(
                                f.regtype(reg),
                                args[1..].iter().map(|x| expr!(x)).collect::<Vec<_>>()
                            ))
                        );
                    }
                } else {
                    let call = call_fun(*fun, args.iter().map(|x| expr!(x)).collect::<Vec<_>>());
                    if fun.ty(code).ret.is_void() {
                        push_stmt!(stmt(call));
                    } else {
                        push_expr!(i, *dst, call);
                    }
                }
            }
            Opcode::CallMethod { dst, field, args } => {
                let call = call(
                    ast::field(expr!(args[0]), f.regtype(args[0]), *field, code),
                    args.iter().skip(1).map(|x| expr!(x)).collect::<Vec<_>>(),
                );
                if f.regtype(args[0])
                    .method(field.0, code)
                    .and_then(|p| p.findex.resolve_as_fn(code))
                    .map(|fun| fun.ty(code).ret.is_void())
                    .unwrap_or(false)
                {
                    push_stmt!(stmt(call));
                } else {
                    push_expr!(i, *dst, call);
                }
            }
            Opcode::CallThis { dst, field, args } => {
                let method = f.regs[0].method(field.0, code).unwrap();
                let call = call(
                    Expr::Field(
                        Box::new(cst_this()),
                        method.name.resolve(&code.strings).to_owned(),
                    ),
                    args.iter().map(|x| expr!(x)).collect::<Vec<_>>(),
                );
                if method
                    .findex
                    .resolve_as_fn(code)
                    .map(|fun| fun.ty(code).ret.is_void())
                    .unwrap_or(false)
                {
                    push_stmt!(stmt(call));
                } else {
                    push_expr!(i, *dst, call);
                }
            }
            Opcode::CallClosure { dst, fun, args } => {
                let call = call(
                    expr!(*fun),
                    args.iter().map(|x| expr!(x)).collect::<Vec<_>>(),
                );
                if f.regtype(*fun)
                    .resolve_as_fun(&code.types)
                    .map(|ty| ty.ret.is_void())
                    .unwrap_or(false)
                {
                    push_stmt!(stmt(call));
                } else {
                    push_expr!(i, *dst, call);
                }
            }
            //endregion

            //region CLOSURES
            &Opcode::StaticClosure { dst, fun } => {
                push_expr!(
                    i,
                    dst,
                    Expr::Closure(
                        fun,
                        decompile_function(code, fun.resolve_as_fn(code).unwrap())
                    )
                );
            }
            &Opcode::InstanceClosure { dst, obj, fun } => {
                push_expr!(
                    i,
                    dst,
                    Expr::Field(
                        Box::new(expr!(obj)),
                        fun.resolve_as_fn(code)
                            .unwrap()
                            .name(code)
                            .unwrap_or("_")
                            .to_owned(),
                    )
                );
            }
            //endregion

            //region ACCESSES
            &Opcode::GetGlobal { dst, global } => {
                // Is a string
                if f.regtype(dst).0 == 13 {
                    push_expr!(
                        i,
                        dst,
                        cst_string(
                            code.globals_initializers
                                .get(&global)
                                .and_then(|&x| {
                                    code.constants.as_ref().map(|constants| {
                                        code.strings[constants[x].fields[0]].to_owned()
                                    })
                                })
                                .unwrap()
                        )
                    );
                } else {
                    match f.regtype(dst).resolve(&code.types) {
                        Type::Obj(obj) | Type::Struct(obj) => {
                            push_expr!(i, dst, Expr::Variable(dst, Some(obj.name.display(code))));
                        }
                        Type::Enum { .. } => {
                            push_expr!(i, dst, Expr::Unknown("unknown enum variant".to_owned()));
                        }
                        _ => {}
                    }
                }
            }
            &Opcode::Field { dst, obj, field } => {
                push_expr!(i, dst, ast::field(expr!(obj), f.regtype(obj), field, code));
            }
            &Opcode::SetField { obj, field, src } => {
                let ctx = expr_ctx.pop();
                // Might be a SetField for an anonymous structure
                if let Some(ExprCtx::Anonymous {
                    pos,
                    mut fields,
                    mut remaining,
                }) = ctx
                {
                    fields.insert(field, expr!(src));
                    remaining -= 1;
                    // If we filled all the structure fields, we emit an expr
                    if remaining == 0 {
                        push_expr!(pos, obj, Expr::Anonymous(f.regtype(obj), fields));
                    } else {
                        expr_ctx.push(ExprCtx::Anonymous {
                            pos,
                            fields,
                            remaining,
                        });
                    }
                } else if let Some(ctx) = ctx {
                    expr_ctx.push(ctx);
                } else {
                    // Otherwise this is just a normal field set
                    push_stmt!(Statement::Assign {
                        declaration: false,
                        variable: ast::field(expr!(obj), f.regtype(obj), field, code),
                        assign: expr!(src),
                    });
                }
            }
            &Opcode::GetThis { dst, field } => {
                push_expr!(i, dst, ast::field(cst_this(), f.regs[0], field, code));
            }
            &Opcode::SetThis { field, src } => {
                push_stmt!(Statement::Assign {
                    declaration: false,
                    variable: ast::field(cst_this(), f.regs[0], field, code),
                    assign: expr!(src),
                });
            }
            &Opcode::DynGet { dst, obj, field } => {
                push_expr!(i, dst, array(expr!(obj), cst_refstring(field, code)));
            }
            &Opcode::DynSet { obj, field, src } => {
                push_stmt!(Statement::Assign {
                    declaration: false,
                    variable: array(expr!(obj), cst_refstring(field, code)),
                    assign: expr!(src)
                });
            }
            //endregion

            //region VALUES
            &Opcode::ToDyn { dst, src }
            | &Opcode::ToSFloat { dst, src }
            | &Opcode::ToUFloat { dst, src }
            | &Opcode::ToInt { dst, src }
            | &Opcode::SafeCast { dst, src }
            | &Opcode::UnsafeCast { dst, src }
            | &Opcode::ToVirtual { dst, src } => {
                push_expr!(i, dst, expr!(src));
            }
            &Opcode::New { dst } => {
                // Constructor analysis
                let ty = f.regtype(dst).resolve(&code.types);
                match ty {
                    Type::Obj(_) | Type::Struct(_) => {
                        expr_ctx.push(ExprCtx::Constructor { reg: dst, pos: i });
                    }
                    Type::Virtual { fields } => {
                        expr_ctx.push(ExprCtx::Anonymous {
                            pos: i,
                            fields: HashMap::with_capacity(fields.len()),
                            remaining: fields.len(),
                        });
                    }
                    _ => {
                        push_expr!(
                            i,
                            dst,
                            Expr::Constructor(ConstructorCall::new(f.regtype(dst), Vec::new()))
                        );
                    }
                }
            }
            //endregion

            //region ENUMS
            &Opcode::EnumAlloc { dst, construct } => {
                push_expr!(
                    i,
                    dst,
                    Expr::EnumConstr(f.regtype(dst), construct, Vec::new())
                );
            }
            Opcode::MakeEnum {
                dst,
                construct,
                args,
            } => {
                push_expr!(
                    i,
                    *dst,
                    Expr::EnumConstr(
                        f.regtype(*dst),
                        *construct,
                        args.iter().map(|x| expr!(x)).collect()
                    )
                );
            }
            /*
            &Opcode::EnumIndex { dst, value } => {
                // TODO get enum variant
            }
            &Opcode::EnumField {
                dst,
                value,
                construct,
                field,
            } => {
                // TODO get enum field
            }
            &Opcode::SetEnumField { value, field, src } => {
                // TODO set enum field
            }*/
            //endregion
            &Opcode::GetMem { dst, bytes, index } => {
                push_expr!(i, dst, array(expr!(bytes), expr!(index)));
            }
            _ => {}
        }
        scopes.advance();
    }
    scopes.statements()
}

/*
fn if_expression(stmts: &mut Vec<Statement>) {
    let mut iter = stmts.iter_mut();
    while let Some(stmt) = iter.next() {
        if let Statement::If {
            stmts: if_stmts, ..
        } = stmt
        {
            if let Some(Statement::Assign { variable: if_v, .. }) = if_stmts.last() {
                if let Some(Statement::Else {
                    stmts: else_stmts, ..
                }) = iter.next()
                {
                    if let Some(Statement::Assign {
                        variable: else_v, ..
                    }) = else_stmts.last()
                    {
                        if if_v == else_v {
                            // This if/else could be used as an expression
                        }
                    }
                } else {
                    // This if could be used as en expression
                }
            }
        }
    }
}*/

/// Decompile a class with its static and instance fields and methods.
pub fn decompile_class(code: &Bytecode, obj: &TypeObj) -> Class {
    let static_type = obj.get_static_type(code);

    let mut fields = Vec::new();
    for (i, f) in obj.own_fields.iter().enumerate() {
        if obj
            .bindings
            .get(&RefField(i + obj.fields.len() - obj.own_fields.len()))
            .is_some()
        {
            continue;
        }
        fields.push(ClassField {
            name: f.name.display(code),
            static_: false,
            ty: f.t,
        });
    }
    if let Some(ty) = static_type {
        for (i, f) in ty.own_fields.iter().enumerate() {
            if ty
                .bindings
                .get(&RefField(i + ty.fields.len() - ty.own_fields.len()))
                .is_some()
            {
                continue;
            }
            fields.push(ClassField {
                name: f.name.display(code),
                static_: true,
                ty: f.t,
            });
        }
    }

    let mut methods = Vec::new();
    for fun in obj.bindings.values() {
        methods.push(Method {
            fun: *fun,
            static_: false,
            dynamic: true,
            statements: decompile_function(code, fun.resolve_as_fn(code).unwrap()),
        })
    }
    if let Some(ty) = static_type {
        for fun in ty.bindings.values() {
            methods.push(Method {
                fun: *fun,
                static_: true,
                dynamic: false,
                statements: decompile_function(code, fun.resolve_as_fn(code).unwrap()),
            })
        }
    }
    for f in &obj.protos {
        methods.push(Method {
            fun: f.findex,
            static_: false,
            dynamic: false,
            statements: decompile_function(code, f.findex.resolve_as_fn(code).unwrap()),
        })
    }

    Class {
        name: obj.name.resolve(&code.strings).to_owned(),
        parent: obj
            .super_
            .and_then(|ty| ty.resolve_as_obj(&code.types))
            .map(|ty| ty.name.display(code)),
        fields,
        methods,
    }
}
