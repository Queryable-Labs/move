// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    loader::{Function, Loader, Resolver},
    native_functions::NativeContext,
    trace,
};
use fail::fail_point;
use move_binary_format::{
    errors::*,
    file_format::{Bytecode, FunctionHandleIndex, FunctionInstantiationIndex},
};
use move_core_types::{
    account_address::AccountAddress,
    gas_algebra::{NumArgs, NumBytes},
    language_storage::TypeTag,
    trace::{CallTrace, CallType},
    vm_status::{StatusCode, StatusType},
};
use move_core_types::value::MoveTypeLayout;
use move_vm_types::{
    data_store::DataStore,
    gas::{GasMeter, SimpleInstruction},
    loaded_data::runtime_types::Type,
    values::{
        self, GlobalValue, IntegerValue, Locals, Reference, Struct, StructRef, VMValueCast, Value,
        Vector, VectorRef,
    },
    views::TypeView,
};

use crate::native_extensions::NativeContextExtensions;
use std::{cmp::min, collections::VecDeque, fmt::Write, mem, sync::Arc};
use tracing::{error, warn};

macro_rules! debug_write {
    ($($toks: tt)*) => {
        write!($($toks)*).map_err(|_|
            PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                .with_message("failed to write to buffer".to_string())
        )
    };
}

macro_rules! debug_writeln {
    ($($toks: tt)*) => {
        writeln!($($toks)*).map_err(|_|
            PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                .with_message("failed to write to buffer".to_string())
        )
    };
}

macro_rules! set_err_info {
    ($frame:ident, $e:expr) => {{
        $e.at_code_offset($frame.function.index(), $frame.pc)
            .finish($frame.location())
    }};
}

/// `Interpreter` instances can execute Move functions.
///
/// An `Interpreter` instance is a stand alone execution context for a function.
/// It mimics execution on a single thread, with an call stack and an operand stack.
pub(crate) struct Interpreter {
    /// Operand stack, where Move `Value`s are stored for stack operations.
    operand_stack: Stack,
    /// The stack of active functions.
    call_stack: CallStack,
    /// List of captured call traces
    call_traces: Vec<CallTrace>,
}

pub(crate) struct InterpreterEntrypointResult {
    pub values: VMResult<Vec<Value>>,
    pub call_traces: Vec<CallTrace>,
}

struct TypeWithLoader<'a, 'b> {
    ty: &'a Type,
    loader: &'b Loader,
}

impl<'a, 'b> TypeView for TypeWithLoader<'a, 'b> {
    fn to_type_tag(&self) -> TypeTag {
        self.loader.type_to_type_tag(self.ty).unwrap()
    }
}

impl Interpreter {
    /// Entrypoint into the interpreter. All external calls need to be routed through this
    /// function.
    pub(crate) fn entrypoint(
        function: Arc<Function>,
        ty_args: Vec<Type>,
        args: Vec<Value>,
        data_store: &mut impl DataStore,
        gas_meter: &mut impl GasMeter,
        extensions: &mut NativeContextExtensions,
        loader: &Loader,
    ) -> VMResult<InterpreterEntrypointResult> {
        let mut interp = Interpreter {
            operand_stack: Stack::new(),
            call_stack: CallStack::new(),
            call_traces: Vec::new(),
        };

        let values = interp.execute_main(
            loader, data_store, gas_meter, extensions, function, ty_args, args,
        );

        Ok(InterpreterEntrypointResult {
            values,
            call_traces: interp.call_traces,
        })
    }

    /// Main loop for the execution of a function.
    ///
    /// This function sets up a `Frame` and calls `execute_code_unit` to execute code of the
    /// function represented by the frame. Control comes back to this function on return or
    /// on call. When that happens the frame is changes to a new one (call) or to the one
    /// at the top of the stack (return). If the call stack is empty execution is completed.
    fn execute_main(
        &mut self,
        loader: &Loader,
        data_store: &mut impl DataStore,
        gas_meter: &mut impl GasMeter,
        extensions: &mut NativeContextExtensions,
        function: Arc<Function>,
        ty_args: Vec<Type>,
        args: Vec<Value>,
    ) -> VMResult<Vec<Value>> {
        let mut locals = Locals::new(function.local_count());
        for (i, value) in args.into_iter().enumerate() {
            locals
                .store_loc(i, value)
                .map_err(|e| self.set_location(e))?;
        }

        let mut current_frame = Frame::new(function, ty_args, locals, 0);

        let (args_types, args_values) =
            current_frame.extract_types_and_arguments(loader, data_store);

        let call_trace = CallTrace {
            depth: self.call_stack.len() as u32,
            call_type: match current_frame.ty_args.len() > 0 {
                true => CallType::CallGeneric,
                false => CallType::Call,
            },
            module_id: if let Some(module_id) = current_frame.function.module_id() {
                Some(module_id.short_str_lossless())
            } else {
                None
            },
            function: current_frame.function.name().into(),
            ty_args: current_frame
                .ty_args
                .iter()
                .map(|ty| {
                    loader.type_to_type_layout(&ty).unwrap().to_string().into_bytes()
                })
                .collect(),
            args_types: args_types.clone(),
            args_values: args_values.clone(),
            gas_used: 0,
            err: None,
        };

        self.call_traces.push(call_trace);

        let mut gas_used_before_call = gas_meter.charged_already_total().unwrap();

        loop {
            let resolver = current_frame.resolver(loader);
            let exit_code =
                current_frame //self
                    .execute_code(&resolver, self, data_store, gas_meter)
                    .map_err(|err| {
                        let gas_used_after_call = gas_meter.charged_already_total().unwrap();

                        if let Some(call_trace) = self.call_traces.get_mut(current_frame.call_trace_index) {
                            call_trace.err = Some(err.clone().into_vm_status());
                            call_trace.gas_used += gas_used_after_call
                                .checked_sub(gas_used_before_call)
                                .unwrap()
                                .value();
                        }

                        self.maybe_core_dump(err, &current_frame)
                    })?;

            let gas_used_after_call = gas_meter.charged_already_total().unwrap();

            if let Some(call_trace) = self.call_traces.get_mut(current_frame.call_trace_index) {
                call_trace.gas_used += gas_used_after_call
                    .checked_sub(gas_used_before_call)
                    .unwrap()
                    .value();
            } else {
                warn!("Failed to locate Call Trace after successful frame execution");
            }

            gas_used_before_call = gas_used_after_call;

            match exit_code {
                ExitCode::Return => {
                    let non_ref_vals = current_frame
                        .locals
                        .drop_all_values()
                        .map(|(_idx, val)| val);

                    // TODO: Check if the error location is set correctly.
                    gas_meter
                        .charge_drop_frame(non_ref_vals.into_iter())
                        .map_err(|e| {
                            let err = self.set_location(e);

                            let gas_used_after_call = gas_meter.charged_already_total().unwrap();

                            if let Some(call_trace) = self.call_traces.get_mut(current_frame.call_trace_index) {
                                call_trace.err = Some(err.clone().into_vm_status());
                                call_trace.gas_used += gas_used_after_call
                                    .checked_sub(gas_used_before_call)
                                    .unwrap()
                                    .value();
                            }

                            err
                        })?;

                    if let Some(frame) = self.call_stack.pop() {
                        // Note: the caller will find the callee's return values at the top of the shared operand stack
                        current_frame = frame;
                        current_frame.pc += 1; // advance past the Call instruction in the caller
                    } else {
                        // end of execution. `self` should no longer be used afterward
                        return Ok(mem::take(&mut self.operand_stack.0));
                    }
                }
                ExitCode::Call(fh_idx) => {
                    let func = resolver.function_from_handle(fh_idx);

                    // Charge gas
                    let module_id = func
                        .module_id()
                        .ok_or_else(|| {
                            PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                                .with_message("Failed to get native function module id".to_string())
                        })
                        .map_err(|e| set_err_info!(current_frame, e))?;
                    gas_meter
                        .charge_call(
                            module_id,
                            func.name(),
                            self.operand_stack
                                .last_n(func.arg_count())
                                .map_err(|e| set_err_info!(current_frame, e))?,
                            (func.local_count() as u64).into(),
                        )
                        .map_err(|e| set_err_info!(current_frame, e))?;

                    if func.is_native() {
                        self.call_native(
                            &resolver,
                            data_store,
                            gas_meter,
                            extensions,
                            func,
                            vec![],
                        )?;
                        current_frame.pc += 1; // advance past the Call instruction in the caller
                        continue;
                    }
                    let frame = self
                        .make_call_frame(func, vec![], loader, data_store)
                        .map_err(|err| self.maybe_core_dump(err, &current_frame))?;
                    self.call_stack.push(current_frame).map_err(|frame| {
                        let err = PartialVMError::new(StatusCode::CALL_STACK_OVERFLOW);
                        let err = set_err_info!(frame, err);
                        self.maybe_core_dump(err, &frame)
                    })?;
                    // Note: the caller will find the the callee's return values at the top of the shared operand stack
                    current_frame = frame;
                }
                ExitCode::CallGeneric(idx) => {
                    // TODO(Gas): We should charge gas as we do type substitution...
                    let ty_args = resolver
                        .instantiate_generic_function(idx, current_frame.ty_args())
                        .map_err(|e| set_err_info!(current_frame, e))?;
                    let func = resolver.function_from_instantiation(idx);

                    // Charge gas
                    let module_id = func
                        .module_id()
                        .ok_or_else(|| {
                            PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                                .with_message("Failed to get native function module id".to_string())
                        })
                        .map_err(|e| set_err_info!(current_frame, e))?;
                    gas_meter
                        .charge_call_generic(
                            module_id,
                            func.name(),
                            ty_args.iter().map(|ty| TypeWithLoader { ty, loader }),
                            self.operand_stack
                                .last_n(func.arg_count())
                                .map_err(|e| set_err_info!(current_frame, e))?,
                            (func.local_count() as u64).into(),
                        )
                        .map_err(|e| set_err_info!(current_frame, e))?;

                    if func.is_native() {
                        self.call_native(
                            &resolver, data_store, gas_meter, extensions, func, ty_args,
                        )?;
                        current_frame.pc += 1; // advance past the Call instruction in the caller
                        continue;
                    }
                    let frame = self
                        .make_call_frame(func, ty_args, loader, data_store)
                        .map_err(|err| self.maybe_core_dump(err, &current_frame))?;
                    self.call_stack.push(current_frame).map_err(|frame| {
                        let err = PartialVMError::new(StatusCode::CALL_STACK_OVERFLOW);
                        let err = set_err_info!(frame, err);
                        self.maybe_core_dump(err, &frame)
                    })?;
                    current_frame = frame;
                }
            }
        }
    }

    /// Returns a `Frame` if the call is to a Move function. Calls to native functions are
    /// "inlined" and this returns `None`.
    ///
    /// Native functions do not push a frame at the moment and as such errors from a native
    /// function are incorrectly attributed to the caller.
    fn make_call_frame(&mut self, func: Arc<Function>, ty_args: Vec<Type>, loader: &Loader, data_store: &mut impl DataStore) -> VMResult<Frame> {
        let mut locals = Locals::new(func.local_count());
        let arg_count = func.arg_count();
        for i in 0..arg_count {
            locals
                .store_loc(
                    arg_count - i - 1,
                    self.operand_stack.pop().map_err(|e| self.set_location(e))?,
                )
                .map_err(|e| self.set_location(e))?;
        }

        let frame = Frame::new(func, ty_args, locals, self.call_traces.len());

        let (args_types, args_values) =
            frame.extract_types_and_arguments(loader, data_store);

        let call_trace = CallTrace {
            depth: self.call_stack.len() as u32,
            call_type: match frame.ty_args.len() > 0 {
                true => CallType::CallGeneric,
                false => CallType::Call,
            },
            module_id: if let Some(module_id) = frame.function.module_id() {
                Some(module_id.short_str_lossless())
            } else {
                None
            },
            function: frame.function.name().into(),
            ty_args: frame
                .ty_args
                .iter()
                .map(|ty| {
                    match loader.type_to_type_layout(&ty) {
                        Err(err) => {
                            warn!("Failed to convert type to type_layout, error: {}", err);
                            vec![]
                        },
                        Ok(value) => value.to_string().into_bytes()
                    }
                })
                .collect(),
            args_types: args_types.clone(),
            args_values: args_values.clone(),
            gas_used: 0,
            err: None,
        };

        self.call_traces.push(call_trace);

        Ok(frame)
    }

    /// Call a native functions.
    fn call_native(
        &mut self,
        resolver: &Resolver,
        data_store: &mut dyn DataStore,
        gas_meter: &mut impl GasMeter,
        extensions: &mut NativeContextExtensions,
        function: Arc<Function>,
        ty_args: Vec<Type>,
    ) -> VMResult<()> {
        // Note: refactor if native functions push a frame on the stack
        self.call_native_impl(
            resolver,
            data_store,
            gas_meter,
            extensions,
            function.clone(),
            ty_args,
        )
        .map_err(|e| match function.module_id() {
            Some(id) => e
                .at_code_offset(function.index(), 0)
                .finish(Location::Module(id.clone())),
            None => {
                let err = PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                    .with_message("Unexpected native function not located in a module".to_owned());
                self.set_location(err)
            }
        })
    }

    fn call_native_impl(
        &mut self,
        resolver: &Resolver,
        data_store: &mut dyn DataStore,
        gas_meter: &mut impl GasMeter,
        extensions: &mut NativeContextExtensions,
        function: Arc<Function>,
        ty_args: Vec<Type>,
    ) -> PartialVMResult<()> {
        let return_type_count = function.return_type_count();
        let mut args = VecDeque::new();
        let expected_args = function.arg_count();
        for _ in 0..expected_args {
            args.push_front(self.operand_stack.pop()?);
        }
        let mut native_context = NativeContext::new(self, data_store, resolver, extensions);
        let native_function = function.get_native()?;

        gas_meter.charge_native_function_before_execution(
            ty_args.iter().map(|ty| TypeWithLoader {
                ty,
                loader: resolver.loader(),
            }),
            args.iter(),
        )?;

        let result = native_function(&mut native_context, ty_args, args)?;

        // Note(Gas): The order by which gas is charged / error gets returned MUST NOT be modified
        //            here or otherwise it becomes an incompatible change!!!
        let return_values = match result.result {
            Ok(vals) => {
                gas_meter.charge_native_function(result.cost, Some(vals.iter()))?;
                vals
            }
            Err(code) => {
                gas_meter.charge_native_function(
                    result.cost,
                    Option::<std::iter::Empty<&Value>>::None,
                )?;
                return Err(PartialVMError::new(StatusCode::ABORTED).with_sub_status(code));
            }
        };

        // Paranoid check to protect us against incorrect native function implementations. A native function that
        // returns a different number of values than its declared types will trigger this check
        if return_values.len() != return_type_count {
            return Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR).with_message(
                    "Arity mismatch: return value count does not match return type count"
                        .to_string(),
                ),
            );
        }
        // Put return values on the top of the operand stack, where the caller will find them.
        // This is one of only two times the operand stack is shared across call stack frames; the other is in handling
        // the Return instruction for normal calls
        for value in return_values {
            self.operand_stack.push(value)?;
        }
        Ok(())
    }

    /// Perform a binary operation to two values at the top of the stack.
    fn binop<F, T>(&mut self, f: F) -> PartialVMResult<()>
    where
        Value: VMValueCast<T>,
        F: FnOnce(T, T) -> PartialVMResult<Value>,
    {
        let rhs = self.operand_stack.pop_as::<T>()?;
        let lhs = self.operand_stack.pop_as::<T>()?;
        let result = f(lhs, rhs)?;
        self.operand_stack.push(result)
    }

    /// Perform a binary operation for integer values.
    fn binop_int<F>(&mut self, f: F) -> PartialVMResult<()>
    where
        F: FnOnce(IntegerValue, IntegerValue) -> PartialVMResult<IntegerValue>,
    {
        self.binop(|lhs, rhs| {
            Ok(match f(lhs, rhs)? {
                IntegerValue::U8(x) => Value::u8(x),
                IntegerValue::U16(x) => Value::u16(x),
                IntegerValue::U32(x) => Value::u32(x),
                IntegerValue::U64(x) => Value::u64(x),
                IntegerValue::U128(x) => Value::u128(x),
                IntegerValue::U256(x) => Value::u256(x),
            })
        })
    }

    /// Perform a binary operation for boolean values.
    fn binop_bool<F, T>(&mut self, f: F) -> PartialVMResult<()>
    where
        Value: VMValueCast<T>,
        F: FnOnce(T, T) -> PartialVMResult<bool>,
    {
        self.binop(|lhs, rhs| Ok(Value::bool(f(lhs, rhs)?)))
    }

    /// Loads a resource from the data store and return the number of bytes read from the storage.
    fn load_resource<'b>(
        gas_meter: &mut impl GasMeter,
        data_store: &'b mut impl DataStore,
        addr: AccountAddress,
        ty: &Type,
    ) -> PartialVMResult<&'b mut GlobalValue> {
        match data_store.load_resource(addr, ty) {
            Ok((gv, load_res)) => {
                if let Some(loaded) = load_res {
                    let opt = match loaded {
                        Some(num_bytes) => {
                            let view = gv.view().ok_or_else(|| {
                                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                                    .with_message(
                                        "Failed to create view for global value".to_owned(),
                                    )
                            })?;

                            Some((num_bytes, view))
                        }
                        None => None,
                    };
                    gas_meter.charge_load_resource(opt)?;
                }
                Ok(gv)
            }
            Err(e) => {
                error!(
                    "[VM] error loading resource at ({}, {:?}): {:?} from data store",
                    addr, ty, e
                );
                Err(e)
            }
        }
    }

    /// BorrowGlobal (mutable and not) opcode.
    fn borrow_global(
        &mut self,
        is_mut: bool,
        is_generic: bool,
        loader: &Loader,
        gas_meter: &mut impl GasMeter,
        data_store: &mut impl DataStore,
        addr: AccountAddress,
        ty: &Type,
    ) -> PartialVMResult<()> {
        let res = Self::load_resource(gas_meter, data_store, addr, ty)?.borrow_global();
        gas_meter.charge_borrow_global(
            is_mut,
            is_generic,
            TypeWithLoader { ty, loader },
            res.is_ok(),
        )?;
        self.operand_stack.push(res?)?;
        Ok(())
    }

    /// Exists opcode.
    fn exists(
        &mut self,
        is_generic: bool,
        loader: &Loader,
        gas_meter: &mut impl GasMeter,
        data_store: &mut impl DataStore,
        addr: AccountAddress,
        ty: &Type,
    ) -> PartialVMResult<()> {
        let gv = Self::load_resource(gas_meter, data_store, addr, ty)?;
        let exists = gv.exists()?;
        gas_meter.charge_exists(is_generic, TypeWithLoader { ty, loader }, exists)?;
        self.operand_stack.push(Value::bool(exists))?;
        Ok(())
    }

    /// MoveFrom opcode.
    fn move_from(
        &mut self,
        is_generic: bool,
        loader: &Loader,
        gas_meter: &mut impl GasMeter,
        data_store: &mut impl DataStore,
        addr: AccountAddress,
        ty: &Type,
    ) -> PartialVMResult<()> {
        let resource = match Self::load_resource(gas_meter, data_store, addr, ty)?.move_from() {
            Ok(resource) => {
                gas_meter.charge_move_from(
                    is_generic,
                    TypeWithLoader { ty, loader },
                    Some(&resource),
                )?;
                resource
            }
            Err(err) => {
                let val: Option<&Value> = None;
                gas_meter.charge_move_from(is_generic, TypeWithLoader { ty, loader }, val)?;
                return Err(err);
            }
        };
        self.operand_stack.push(resource)?;
        Ok(())
    }

    /// MoveTo opcode.
    fn move_to(
        &mut self,
        is_generic: bool,
        loader: &Loader,
        gas_meter: &mut impl GasMeter,
        data_store: &mut impl DataStore,
        addr: AccountAddress,
        ty: &Type,
        resource: Value,
    ) -> PartialVMResult<()> {
        let gv = Self::load_resource(gas_meter, data_store, addr, ty)?;
        // NOTE(Gas): To maintain backward compatibility, we need to charge gas after attempting
        //            the move_to operation.
        match gv.move_to(resource) {
            Ok(()) => {
                gas_meter.charge_move_to(
                    is_generic,
                    TypeWithLoader { ty, loader },
                    gv.view().unwrap(),
                    true,
                )?;
                Ok(())
            }
            Err((err, resource)) => {
                gas_meter.charge_move_to(
                    is_generic,
                    TypeWithLoader { ty, loader },
                    &resource,
                    false,
                )?;
                Err(err)
            }
        }
    }

    //
    // Debugging and logging helpers.
    //

    /// Given an `VMStatus` generate a core dump if the error is an `InvariantViolation`.
    fn maybe_core_dump(&self, mut err: VMError, current_frame: &Frame) -> VMError {
        // a verification error cannot happen at runtime so change it into an invariant violation.
        if err.status_type() == StatusType::Verification {
            error!("Verification error during runtime: {:?}", err);
            let new_err = PartialVMError::new(StatusCode::VERIFICATION_ERROR);
            let new_err = match err.message() {
                None => new_err,
                Some(msg) => new_err.with_message(msg.to_owned()),
            };
            err = new_err.finish(err.location().clone())
        }
        if err.status_type() == StatusType::InvariantViolation {
            let state = self.internal_state_str(current_frame);

            error!(
                "Error: {:?}\nCORE DUMP: >>>>>>>>>>>>\n{}\n<<<<<<<<<<<<\n",
                err, state,
            );
        }
        err
    }

    #[allow(dead_code)]
    fn debug_print_frame<B: Write>(
        &self,
        buf: &mut B,
        loader: &Loader,
        idx: usize,
        frame: &Frame,
    ) -> PartialVMResult<()> {
        // Print out the function name with type arguments.
        let func = &frame.function;

        debug_write!(buf, "    [{}] ", idx)?;
        if let Some(module) = func.module_id() {
            debug_write!(buf, "{}::{}::", module.address(), module.name(),)?;
        }
        debug_write!(buf, "{}", func.name())?;
        let ty_args = frame.ty_args();
        let mut ty_tags = vec![];
        for ty in ty_args {
            ty_tags.push(loader.type_to_type_tag(ty)?);
        }
        if !ty_tags.is_empty() {
            debug_write!(buf, "<")?;
            let mut it = ty_tags.iter();
            if let Some(tag) = it.next() {
                debug_write!(buf, "{}", tag)?;
                for tag in it {
                    debug_write!(buf, ", ")?;
                    debug_write!(buf, "{}", tag)?;
                }
            }
            debug_write!(buf, ">")?;
        }
        debug_writeln!(buf)?;

        // Print out the current instruction.
        debug_writeln!(buf)?;
        debug_writeln!(buf, "        Code:")?;
        let pc = frame.pc as usize;
        let code = func.code();
        let before = if pc > 3 { pc - 3 } else { 0 };
        let after = min(code.len(), pc + 4);
        for (idx, instr) in code.iter().enumerate().take(pc).skip(before) {
            debug_writeln!(buf, "            [{}] {:?}", idx, instr)?;
        }
        debug_writeln!(buf, "          > [{}] {:?}", pc, &code[pc])?;
        for (idx, instr) in code.iter().enumerate().take(after).skip(pc + 1) {
            debug_writeln!(buf, "            [{}] {:?}", idx, instr)?;
        }

        // Print out the locals.
        debug_writeln!(buf)?;
        debug_writeln!(buf, "        Locals:")?;
        if func.local_count() > 0 {
            values::debug::print_locals(buf, &frame.locals)?;
            debug_writeln!(buf)?;
        } else {
            debug_writeln!(buf, "            (none)")?;
        }

        debug_writeln!(buf)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn debug_print_stack_trace<B: Write>(
        &self,
        buf: &mut B,
        loader: &Loader,
    ) -> PartialVMResult<()> {
        debug_writeln!(buf, "Call Stack:")?;
        for (i, frame) in self.call_stack.0.iter().enumerate() {
            self.debug_print_frame(buf, loader, i, frame)?;
        }
        debug_writeln!(buf, "Operand Stack:")?;
        for (idx, val) in self.operand_stack.0.iter().enumerate() {
            // TODO: Currently we do not know the types of the values on the operand stack.
            // Revisit.
            debug_write!(buf, "    [{}] ", idx)?;
            values::debug::print_value(buf, val)?;
            debug_writeln!(buf)?;
        }
        Ok(())
    }

    /// Generate a string which is the status of the interpreter: call stack, current bytecode
    /// stream, locals and operand stack.
    ///
    /// It is used when generating a core dump but can be used for debugging of the interpreter.
    /// It will be exposed via a debug module to give developers a way to print the internals
    /// of an execution.
    fn internal_state_str(&self, current_frame: &Frame) -> String {
        let mut internal_state = "Call stack:\n".to_string();
        for (i, frame) in self.call_stack.0.iter().enumerate() {
            internal_state.push_str(
                format!(
                    " frame #{}: {} [pc = {}]\n",
                    i,
                    frame.function.pretty_string(),
                    frame.pc,
                )
                .as_str(),
            );
        }
        internal_state.push_str(
            format!(
                "*frame #{}: {} [pc = {}]:\n",
                self.call_stack.0.len(),
                current_frame.function.pretty_string(),
                current_frame.pc,
            )
            .as_str(),
        );
        let code = current_frame.function.code();
        let pc = current_frame.pc as usize;
        if pc < code.len() {
            let mut i = 0;
            for bytecode in &code[..pc] {
                internal_state.push_str(format!("{}> {:?}\n", i, bytecode).as_str());
                i += 1;
            }
            internal_state.push_str(format!("{}* {:?}\n", i, code[pc]).as_str());
        }
        internal_state.push_str(format!("Locals:\n{}\n", current_frame.locals).as_str());
        internal_state.push_str("Operand Stack:\n");
        for value in &self.operand_stack.0 {
            internal_state.push_str(format!("{}\n", value).as_str());
        }
        internal_state
    }

    fn set_location(&self, err: PartialVMError) -> VMError {
        err.finish(self.call_stack.current_location())
    }

    fn get_internal_state(&self) -> ExecutionState {
        self.get_stack_frames(usize::MAX)
    }

    /// Get count stack frames starting from the top of the stack.
    pub(crate) fn get_stack_frames(&self, count: usize) -> ExecutionState {
        // collect frames in the reverse order as this is what is
        // normally expected from the stack trace (outermost frame
        // is the last one)
        let stack_trace = self
            .call_stack
            .0
            .iter()
            .rev()
            .take(count)
            .map(|frame| {
                (
                    frame.function.module_id().cloned(),
                    frame.function.index(),
                    frame.pc,
                )
            })
            .collect();
        ExecutionState::new(stack_trace)
    }
}

// TODO Determine stack size limits based on gas limit
const OPERAND_STACK_SIZE_LIMIT: usize = 1024;
const CALL_STACK_SIZE_LIMIT: usize = 1024;

/// The operand stack.
struct Stack(Vec<Value>);

impl Stack {
    /// Create a new empty operand stack.
    fn new() -> Self {
        Stack(vec![])
    }

    /// Push a `Value` on the stack if the max stack size has not been reached. Abort execution
    /// otherwise.
    fn push(&mut self, value: Value) -> PartialVMResult<()> {
        if self.0.len() < OPERAND_STACK_SIZE_LIMIT {
            self.0.push(value);
            Ok(())
        } else {
            Err(PartialVMError::new(StatusCode::EXECUTION_STACK_OVERFLOW))
        }
    }

    /// Pop a `Value` off the stack or abort execution if the stack is empty.
    fn pop(&mut self) -> PartialVMResult<Value> {
        self.0
            .pop()
            .ok_or_else(|| PartialVMError::new(StatusCode::EMPTY_VALUE_STACK))
    }

    /// Pop a `Value` of a given type off the stack. Abort if the value is not of the given
    /// type or if the stack is empty.
    fn pop_as<T>(&mut self) -> PartialVMResult<T>
    where
        Value: VMValueCast<T>,
    {
        self.pop()?.value_as()
    }

    /// Pop n values off the stack.
    fn popn(&mut self, n: u16) -> PartialVMResult<Vec<Value>> {
        let remaining_stack_size = self
            .0
            .len()
            .checked_sub(n as usize)
            .ok_or_else(|| PartialVMError::new(StatusCode::EMPTY_VALUE_STACK))?;
        let args = self.0.split_off(remaining_stack_size);
        Ok(args)
    }

    fn last_n(&self, n: usize) -> PartialVMResult<impl ExactSizeIterator<Item = &Value>> {
        if self.0.len() < n {
            return Err(PartialVMError::new(StatusCode::EMPTY_VALUE_STACK)
                .with_message("Failed to get last n arguments on the argument stack".to_string()));
        }
        Ok(self.0[(self.0.len() - n)..].iter())
    }
}

/// A call stack.
// #[derive(Debug)]
struct CallStack(Vec<Frame>);

impl CallStack {
    /// Create a new empty call stack.
    fn new() -> Self {
        CallStack(vec![])
    }

    /// Push a `Frame` on the call stack.
    fn push(&mut self, frame: Frame) -> ::std::result::Result<(), Frame> {
        if self.0.len() < CALL_STACK_SIZE_LIMIT {
            self.0.push(frame);
            Ok(())
        } else {
            Err(frame)
        }
    }

    /// Pop a `Frame` off the call stack.
    fn pop(&mut self) -> Option<Frame> {
        self.0.pop()
    }

    fn current_location(&self) -> Location {
        let location_opt = self.0.last().map(|frame| frame.location());
        location_opt.unwrap_or(Location::Undefined)
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

/// A `Frame` is the execution context for a function. It holds the locals of the function and
/// the function itself.
// #[derive(Debug)]
struct Frame {
    pc: u16,
    locals: Locals,
    function: Arc<Function>,
    ty_args: Vec<Type>,
    call_trace_index: usize,
}

/// An `ExitCode` from `execute_code_unit`.
#[derive(Debug)]
enum ExitCode {
    Return,
    Call(FunctionHandleIndex),
    CallGeneric(FunctionInstantiationIndex),
}

impl Frame {
    /// Create a new `Frame` given a `Function` and the function `Locals`.
    ///
    /// The locals must be loaded before calling this.
    fn new(function: Arc<Function>, ty_args: Vec<Type>, locals: Locals, call_trace_index: usize) -> Self {
        Frame {
            pc: 0,
            locals,
            function,
            ty_args,
            call_trace_index,
        }
    }

    /// Execute a Move function until a return or a call opcode is found.
    fn execute_code(
        &mut self,
        resolver: &Resolver,
        interpreter: &mut Interpreter,
        data_store: &mut impl DataStore,
        gas_meter: &mut impl GasMeter,
    ) -> VMResult<ExitCode> {
        self.execute_code_impl(resolver, interpreter, data_store, gas_meter)
            .map_err(|e| {
                let e = if cfg!(feature = "testing") || cfg!(feature = "stacktrace") {
                    e.with_exec_state(interpreter.get_internal_state())
                } else {
                    e
                };
                e.at_code_offset(self.function.index(), self.pc)
                    .finish(self.location())
            })
    }

    fn execute_code_impl(
        &mut self,
        resolver: &Resolver,
        interpreter: &mut Interpreter,
        data_store: &mut impl DataStore,
        gas_meter: &mut impl GasMeter,
    ) -> PartialVMResult<ExitCode> {
        use SimpleInstruction as S;

        macro_rules! make_ty {
            ($ty: expr) => {
                TypeWithLoader {
                    ty: $ty,
                    loader: resolver.loader(),
                }
            };
        }

        let code = self.function.code();
        loop {
            for instruction in &code[self.pc as usize..] {
                trace!(
                    &self.function,
                    &self.locals,
                    self.pc,
                    instruction,
                    resolver,
                    interpreter
                );

                fail_point!("move_vm::interpreter_loop", |_| {
                    Err(
                        PartialVMError::new(StatusCode::VERIFIER_INVARIANT_VIOLATION).with_message(
                            "Injected move_vm::interpreter verifier failure".to_owned(),
                        ),
                    )
                });

                match instruction {
                    Bytecode::Pop => {
                        let popped_val = interpreter.operand_stack.pop()?;
                        gas_meter.charge_pop(popped_val)?;
                    }
                    Bytecode::Ret => {
                        gas_meter.charge_simple_instr(S::Ret)?;
                        return Ok(ExitCode::Return);
                    }
                    Bytecode::BrTrue(offset) => {
                        gas_meter.charge_simple_instr(S::BrTrue)?;
                        if interpreter.operand_stack.pop_as::<bool>()? {
                            self.pc = *offset;
                            break;
                        }
                    }
                    Bytecode::BrFalse(offset) => {
                        gas_meter.charge_simple_instr(S::BrFalse)?;
                        if !interpreter.operand_stack.pop_as::<bool>()? {
                            self.pc = *offset;
                            break;
                        }
                    }
                    Bytecode::Branch(offset) => {
                        gas_meter.charge_simple_instr(S::Branch)?;
                        self.pc = *offset;
                        break;
                    }
                    Bytecode::LdU8(int_const) => {
                        gas_meter.charge_simple_instr(S::LdU8)?;
                        interpreter.operand_stack.push(Value::u8(*int_const))?;
                    }
                    Bytecode::LdU16(int_const) => {
                        gas_meter.charge_simple_instr(S::LdU16)?;
                        interpreter.operand_stack.push(Value::u16(*int_const))?;
                    }
                    Bytecode::LdU32(int_const) => {
                        gas_meter.charge_simple_instr(S::LdU32)?;
                        interpreter.operand_stack.push(Value::u32(*int_const))?;
                    }
                    Bytecode::LdU64(int_const) => {
                        gas_meter.charge_simple_instr(S::LdU64)?;
                        interpreter.operand_stack.push(Value::u64(*int_const))?;
                    }
                    Bytecode::LdU128(int_const) => {
                        gas_meter.charge_simple_instr(S::LdU128)?;
                        interpreter.operand_stack.push(Value::u128(*int_const))?;
                    }
                    Bytecode::LdU256(int_const) => {
                        gas_meter.charge_simple_instr(S::LdU256)?;
                        interpreter.operand_stack.push(Value::u256(*int_const))?;
                    }
                    Bytecode::LdConst(idx) => {
                        let constant = resolver.constant_at(*idx);
                        gas_meter.charge_ld_const(NumBytes::new(constant.data.len() as u64))?;

                        let val = Value::deserialize_constant(constant).ok_or_else(|| {
                            PartialVMError::new(StatusCode::VERIFIER_INVARIANT_VIOLATION)
                                .with_message(
                                    "Verifier failed to verify the deserialization of constants"
                                        .to_owned(),
                                )
                        })?;

                        gas_meter.charge_ld_const_after_deserialization(&val)?;

                        interpreter.operand_stack.push(val)?
                    }
                    Bytecode::LdTrue => {
                        gas_meter.charge_simple_instr(S::LdTrue)?;
                        interpreter.operand_stack.push(Value::bool(true))?;
                    }
                    Bytecode::LdFalse => {
                        gas_meter.charge_simple_instr(S::LdFalse)?;
                        interpreter.operand_stack.push(Value::bool(false))?;
                    }
                    Bytecode::CopyLoc(idx) => {
                        // TODO(Gas): We should charge gas before copying the value.
                        let local = self.locals.copy_loc(*idx as usize)?;
                        gas_meter.charge_copy_loc(&local)?;
                        interpreter.operand_stack.push(local)?;
                    }
                    Bytecode::MoveLoc(idx) => {
                        let local = self.locals.move_loc(*idx as usize)?;
                        gas_meter.charge_move_loc(&local)?;

                        interpreter.operand_stack.push(local)?;
                    }
                    Bytecode::StLoc(idx) => {
                        let value_to_store = interpreter.operand_stack.pop()?;
                        gas_meter.charge_store_loc(&value_to_store)?;
                        self.locals.store_loc(*idx as usize, value_to_store)?;
                    }
                    Bytecode::Call(idx) => {
                        return Ok(ExitCode::Call(*idx));
                    }
                    Bytecode::CallGeneric(idx) => {
                        return Ok(ExitCode::CallGeneric(*idx));
                    }
                    Bytecode::MutBorrowLoc(idx) | Bytecode::ImmBorrowLoc(idx) => {
                        let instr = match instruction {
                            Bytecode::MutBorrowLoc(_) => S::MutBorrowLoc,
                            _ => S::ImmBorrowLoc,
                        };
                        gas_meter.charge_simple_instr(instr)?;
                        interpreter
                            .operand_stack
                            .push(self.locals.borrow_loc(*idx as usize)?)?;
                    }
                    Bytecode::ImmBorrowField(fh_idx) | Bytecode::MutBorrowField(fh_idx) => {
                        let instr = match instruction {
                            Bytecode::MutBorrowField(_) => S::MutBorrowField,
                            _ => S::ImmBorrowField,
                        };
                        gas_meter.charge_simple_instr(instr)?;

                        let reference = interpreter.operand_stack.pop_as::<StructRef>()?;
                        let offset = resolver.field_offset(*fh_idx);
                        let field_ref = reference.borrow_field(offset)?;
                        interpreter.operand_stack.push(field_ref)?;
                    }
                    Bytecode::ImmBorrowFieldGeneric(fi_idx)
                    | Bytecode::MutBorrowFieldGeneric(fi_idx) => {
                        let instr = match instruction {
                            Bytecode::MutBorrowField(_) => S::MutBorrowFieldGeneric,
                            _ => S::ImmBorrowFieldGeneric,
                        };
                        gas_meter.charge_simple_instr(instr)?;

                        let reference = interpreter.operand_stack.pop_as::<StructRef>()?;
                        let offset = resolver.field_instantiation_offset(*fi_idx);
                        let field_ref = reference.borrow_field(offset)?;
                        interpreter.operand_stack.push(field_ref)?;
                    }
                    Bytecode::Pack(sd_idx) => {
                        let field_count = resolver.field_count(*sd_idx);
                        gas_meter.charge_pack(
                            false,
                            interpreter.operand_stack.last_n(field_count as usize)?,
                        )?;
                        let args = interpreter.operand_stack.popn(field_count)?;
                        interpreter
                            .operand_stack
                            .push(Value::struct_(Struct::pack(args)))?;
                    }
                    Bytecode::PackGeneric(si_idx) => {
                        let field_count = resolver.field_instantiation_count(*si_idx);
                        gas_meter.charge_pack(
                            true,
                            interpreter.operand_stack.last_n(field_count as usize)?,
                        )?;
                        let args = interpreter.operand_stack.popn(field_count)?;
                        interpreter
                            .operand_stack
                            .push(Value::struct_(Struct::pack(args)))?;
                    }
                    Bytecode::Unpack(_sd_idx) => {
                        let struct_ = interpreter.operand_stack.pop_as::<Struct>()?;

                        gas_meter.charge_unpack(false, struct_.field_views())?;

                        for value in struct_.unpack()? {
                            interpreter.operand_stack.push(value)?;
                        }
                    }
                    Bytecode::UnpackGeneric(_si_idx) => {
                        let struct_ = interpreter.operand_stack.pop_as::<Struct>()?;

                        gas_meter.charge_unpack(true, struct_.field_views())?;

                        // TODO: Whether or not we want this gas metering in the loop is
                        // questionable.  However, if we don't have it in the loop we could wind up
                        // doing a fair bit of work before charging for it.
                        for value in struct_.unpack()? {
                            interpreter.operand_stack.push(value)?;
                        }
                    }
                    Bytecode::ReadRef => {
                        let reference = interpreter.operand_stack.pop_as::<Reference>()?;
                        gas_meter.charge_read_ref(reference.value_view())?;
                        let value = reference.read_ref()?;
                        interpreter.operand_stack.push(value)?;
                    }
                    Bytecode::WriteRef => {
                        let reference = interpreter.operand_stack.pop_as::<Reference>()?;
                        let value = interpreter.operand_stack.pop()?;
                        gas_meter.charge_write_ref(&value, reference.value_view())?;
                        reference.write_ref(value)?;
                    }
                    Bytecode::CastU8 => {
                        gas_meter.charge_simple_instr(S::CastU8)?;
                        let integer_value = interpreter.operand_stack.pop_as::<IntegerValue>()?;
                        interpreter
                            .operand_stack
                            .push(Value::u8(integer_value.cast_u8()?))?;
                    }
                    Bytecode::CastU16 => {
                        gas_meter.charge_simple_instr(S::CastU16)?;
                        let integer_value = interpreter.operand_stack.pop_as::<IntegerValue>()?;
                        interpreter
                            .operand_stack
                            .push(Value::u16(integer_value.cast_u16()?))?;
                    }
                    Bytecode::CastU32 => {
                        gas_meter.charge_simple_instr(S::CastU16)?;
                        let integer_value = interpreter.operand_stack.pop_as::<IntegerValue>()?;
                        interpreter
                            .operand_stack
                            .push(Value::u32(integer_value.cast_u32()?))?;
                    }
                    Bytecode::CastU64 => {
                        gas_meter.charge_simple_instr(S::CastU64)?;
                        let integer_value = interpreter.operand_stack.pop_as::<IntegerValue>()?;
                        interpreter
                            .operand_stack
                            .push(Value::u64(integer_value.cast_u64()?))?;
                    }
                    Bytecode::CastU128 => {
                        gas_meter.charge_simple_instr(S::CastU128)?;
                        let integer_value = interpreter.operand_stack.pop_as::<IntegerValue>()?;
                        interpreter
                            .operand_stack
                            .push(Value::u128(integer_value.cast_u128()?))?;
                    }
                    Bytecode::CastU256 => {
                        gas_meter.charge_simple_instr(S::CastU16)?;
                        let integer_value = interpreter.operand_stack.pop_as::<IntegerValue>()?;
                        interpreter
                            .operand_stack
                            .push(Value::u256(integer_value.cast_u256()?))?;
                    }
                    // Arithmetic Operations
                    Bytecode::Add => {
                        gas_meter.charge_simple_instr(S::Add)?;
                        interpreter.binop_int(IntegerValue::add_checked)?
                    }
                    Bytecode::Sub => {
                        gas_meter.charge_simple_instr(S::Sub)?;
                        interpreter.binop_int(IntegerValue::sub_checked)?
                    }
                    Bytecode::Mul => {
                        gas_meter.charge_simple_instr(S::Mul)?;
                        interpreter.binop_int(IntegerValue::mul_checked)?
                    }
                    Bytecode::Mod => {
                        gas_meter.charge_simple_instr(S::Mod)?;
                        interpreter.binop_int(IntegerValue::rem_checked)?
                    }
                    Bytecode::Div => {
                        gas_meter.charge_simple_instr(S::Div)?;
                        interpreter.binop_int(IntegerValue::div_checked)?
                    }
                    Bytecode::BitOr => {
                        gas_meter.charge_simple_instr(S::BitOr)?;
                        interpreter.binop_int(IntegerValue::bit_or)?
                    }
                    Bytecode::BitAnd => {
                        gas_meter.charge_simple_instr(S::BitAnd)?;
                        interpreter.binop_int(IntegerValue::bit_and)?
                    }
                    Bytecode::Xor => {
                        gas_meter.charge_simple_instr(S::Xor)?;
                        interpreter.binop_int(IntegerValue::bit_xor)?
                    }
                    Bytecode::Shl => {
                        gas_meter.charge_simple_instr(S::Shl)?;
                        let rhs = interpreter.operand_stack.pop_as::<u8>()?;
                        let lhs = interpreter.operand_stack.pop_as::<IntegerValue>()?;
                        interpreter
                            .operand_stack
                            .push(lhs.shl_checked(rhs)?.into_value())?;
                    }
                    Bytecode::Shr => {
                        gas_meter.charge_simple_instr(S::Shr)?;
                        let rhs = interpreter.operand_stack.pop_as::<u8>()?;
                        let lhs = interpreter.operand_stack.pop_as::<IntegerValue>()?;
                        interpreter
                            .operand_stack
                            .push(lhs.shr_checked(rhs)?.into_value())?;
                    }
                    Bytecode::Or => {
                        gas_meter.charge_simple_instr(S::Or)?;
                        interpreter.binop_bool(|l, r| Ok(l || r))?
                    }
                    Bytecode::And => {
                        gas_meter.charge_simple_instr(S::And)?;
                        interpreter.binop_bool(|l, r| Ok(l && r))?
                    }
                    Bytecode::Lt => {
                        gas_meter.charge_simple_instr(S::Lt)?;
                        interpreter.binop_bool(IntegerValue::lt)?
                    }
                    Bytecode::Gt => {
                        gas_meter.charge_simple_instr(S::Gt)?;
                        interpreter.binop_bool(IntegerValue::gt)?
                    }
                    Bytecode::Le => {
                        gas_meter.charge_simple_instr(S::Le)?;
                        interpreter.binop_bool(IntegerValue::le)?
                    }
                    Bytecode::Ge => {
                        gas_meter.charge_simple_instr(S::Ge)?;
                        interpreter.binop_bool(IntegerValue::ge)?
                    }
                    Bytecode::Abort => {
                        gas_meter.charge_simple_instr(S::Abort)?;
                        let error_code = interpreter.operand_stack.pop_as::<u64>()?;
                        let error = PartialVMError::new(StatusCode::ABORTED)
                            .with_sub_status(error_code)
                            .with_message(format!(
                                "{} at offset {}",
                                self.function.pretty_string(),
                                self.pc,
                            ));
                        return Err(error);
                    }
                    Bytecode::Eq => {
                        let lhs = interpreter.operand_stack.pop()?;
                        let rhs = interpreter.operand_stack.pop()?;
                        gas_meter.charge_eq(&lhs, &rhs)?;
                        interpreter
                            .operand_stack
                            .push(Value::bool(lhs.equals(&rhs)?))?;
                    }
                    Bytecode::Neq => {
                        let lhs = interpreter.operand_stack.pop()?;
                        let rhs = interpreter.operand_stack.pop()?;
                        gas_meter.charge_neq(&lhs, &rhs)?;
                        interpreter
                            .operand_stack
                            .push(Value::bool(!lhs.equals(&rhs)?))?;
                    }
                    Bytecode::MutBorrowGlobal(sd_idx) | Bytecode::ImmBorrowGlobal(sd_idx) => {
                        let is_mut = matches!(instruction, Bytecode::MutBorrowGlobal(_));
                        let addr = interpreter.operand_stack.pop_as::<AccountAddress>()?;
                        let ty = resolver.get_struct_type(*sd_idx);
                        interpreter.borrow_global(
                            is_mut,
                            false,
                            resolver.loader(),
                            gas_meter,
                            data_store,
                            addr,
                            &ty,
                        )?;
                    }
                    Bytecode::MutBorrowGlobalGeneric(si_idx)
                    | Bytecode::ImmBorrowGlobalGeneric(si_idx) => {
                        let is_mut = matches!(instruction, Bytecode::MutBorrowGlobalGeneric(_));
                        let addr = interpreter.operand_stack.pop_as::<AccountAddress>()?;
                        let ty = resolver.instantiate_generic_type(*si_idx, self.ty_args())?;
                        interpreter.borrow_global(
                            is_mut,
                            true,
                            resolver.loader(),
                            gas_meter,
                            data_store,
                            addr,
                            &ty,
                        )?;
                    }
                    Bytecode::Exists(sd_idx) => {
                        let addr = interpreter.operand_stack.pop_as::<AccountAddress>()?;
                        let ty = resolver.get_struct_type(*sd_idx);
                        interpreter.exists(
                            false,
                            resolver.loader(),
                            gas_meter,
                            data_store,
                            addr,
                            &ty,
                        )?;
                    }
                    Bytecode::ExistsGeneric(si_idx) => {
                        let addr = interpreter.operand_stack.pop_as::<AccountAddress>()?;
                        let ty = resolver.instantiate_generic_type(*si_idx, self.ty_args())?;
                        interpreter.exists(
                            true,
                            resolver.loader(),
                            gas_meter,
                            data_store,
                            addr,
                            &ty,
                        )?;
                    }
                    Bytecode::MoveFrom(sd_idx) => {
                        let addr = interpreter.operand_stack.pop_as::<AccountAddress>()?;
                        let ty = resolver.get_struct_type(*sd_idx);
                        interpreter.move_from(
                            false,
                            resolver.loader(),
                            gas_meter,
                            data_store,
                            addr,
                            &ty,
                        )?;
                    }
                    Bytecode::MoveFromGeneric(si_idx) => {
                        let addr = interpreter.operand_stack.pop_as::<AccountAddress>()?;
                        let ty = resolver.instantiate_generic_type(*si_idx, self.ty_args())?;
                        interpreter.move_from(
                            true,
                            resolver.loader(),
                            gas_meter,
                            data_store,
                            addr,
                            &ty,
                        )?;
                    }
                    Bytecode::MoveTo(sd_idx) => {
                        let resource = interpreter.operand_stack.pop()?;
                        let signer_reference = interpreter.operand_stack.pop_as::<StructRef>()?;
                        let addr = signer_reference
                            .borrow_field(0)?
                            .value_as::<Reference>()?
                            .read_ref()?
                            .value_as::<AccountAddress>()?;
                        let ty = resolver.get_struct_type(*sd_idx);
                        // REVIEW: Can we simplify Interpreter::move_to?
                        interpreter.move_to(
                            false,
                            resolver.loader(),
                            gas_meter,
                            data_store,
                            addr,
                            &ty,
                            resource,
                        )?;
                    }
                    Bytecode::MoveToGeneric(si_idx) => {
                        let resource = interpreter.operand_stack.pop()?;
                        let signer_reference = interpreter.operand_stack.pop_as::<StructRef>()?;
                        let addr = signer_reference
                            .borrow_field(0)?
                            .value_as::<Reference>()?
                            .read_ref()?
                            .value_as::<AccountAddress>()?;
                        let ty = resolver.instantiate_generic_type(*si_idx, self.ty_args())?;
                        interpreter.move_to(
                            true,
                            resolver.loader(),
                            gas_meter,
                            data_store,
                            addr,
                            &ty,
                            resource,
                        )?;
                    }
                    Bytecode::FreezeRef => {
                        gas_meter.charge_simple_instr(S::FreezeRef)?;
                        // FreezeRef should just be a null op as we don't distinguish between mut
                        // and immut ref at runtime.
                    }
                    Bytecode::Not => {
                        gas_meter.charge_simple_instr(S::Not)?;
                        let value = !interpreter.operand_stack.pop_as::<bool>()?;
                        interpreter.operand_stack.push(Value::bool(value))?;
                    }
                    Bytecode::Nop => {
                        gas_meter.charge_simple_instr(S::Nop)?;
                    }
                    Bytecode::VecPack(si, num) => {
                        let ty = resolver.instantiate_single_type(*si, self.ty_args())?;
                        gas_meter.charge_vec_pack(
                            make_ty!(&ty),
                            interpreter.operand_stack.last_n(*num as usize)?,
                        )?;
                        let elements = interpreter.operand_stack.popn(*num as u16)?;
                        let value = Vector::pack(&ty, elements)?;
                        interpreter.operand_stack.push(value)?;
                    }
                    Bytecode::VecLen(si) => {
                        let vec_ref = interpreter.operand_stack.pop_as::<VectorRef>()?;
                        let ty = &resolver.instantiate_single_type(*si, self.ty_args())?;
                        gas_meter.charge_vec_len(TypeWithLoader {
                            ty,
                            loader: resolver.loader(),
                        })?;
                        let value = vec_ref.len(ty)?;
                        interpreter.operand_stack.push(value)?;
                    }
                    Bytecode::VecImmBorrow(si) => {
                        let idx = interpreter.operand_stack.pop_as::<u64>()? as usize;
                        let vec_ref = interpreter.operand_stack.pop_as::<VectorRef>()?;
                        let ty = resolver.instantiate_single_type(*si, self.ty_args())?;
                        let res = vec_ref.borrow_elem(idx, &ty);
                        gas_meter.charge_vec_borrow(false, make_ty!(&ty), res.is_ok())?;
                        interpreter.operand_stack.push(res?)?;
                    }
                    Bytecode::VecMutBorrow(si) => {
                        let idx = interpreter.operand_stack.pop_as::<u64>()? as usize;
                        let vec_ref = interpreter.operand_stack.pop_as::<VectorRef>()?;
                        let ty = &resolver.instantiate_single_type(*si, self.ty_args())?;
                        let res = vec_ref.borrow_elem(idx, ty);
                        gas_meter.charge_vec_borrow(true, make_ty!(ty), res.is_ok())?;
                        interpreter.operand_stack.push(res?)?;
                    }
                    Bytecode::VecPushBack(si) => {
                        let elem = interpreter.operand_stack.pop()?;
                        let vec_ref = interpreter.operand_stack.pop_as::<VectorRef>()?;
                        let ty = &resolver.instantiate_single_type(*si, self.ty_args())?;
                        gas_meter.charge_vec_push_back(make_ty!(ty), &elem)?;
                        vec_ref.push_back(elem, ty)?;
                    }
                    Bytecode::VecPopBack(si) => {
                        let vec_ref = interpreter.operand_stack.pop_as::<VectorRef>()?;
                        let ty = &resolver.instantiate_single_type(*si, self.ty_args())?;
                        let res = vec_ref.pop(ty);
                        gas_meter.charge_vec_pop_back(make_ty!(ty), res.as_ref().ok())?;
                        interpreter.operand_stack.push(res?)?;
                    }
                    Bytecode::VecUnpack(si, num) => {
                        let vec_val = interpreter.operand_stack.pop_as::<Vector>()?;
                        let ty = &resolver.instantiate_single_type(*si, self.ty_args())?;
                        gas_meter.charge_vec_unpack(
                            make_ty!(ty),
                            NumArgs::new(*num),
                            vec_val.elem_views(),
                        )?;
                        let elements = vec_val.unpack(ty, *num)?;
                        for value in elements {
                            interpreter.operand_stack.push(value)?;
                        }
                    }
                    Bytecode::VecSwap(si) => {
                        let idx2 = interpreter.operand_stack.pop_as::<u64>()? as usize;
                        let idx1 = interpreter.operand_stack.pop_as::<u64>()? as usize;
                        let vec_ref = interpreter.operand_stack.pop_as::<VectorRef>()?;
                        let ty = &resolver.instantiate_single_type(*si, self.ty_args())?;
                        gas_meter.charge_vec_swap(make_ty!(ty))?;
                        vec_ref.swap(idx1, idx2, ty)?;
                    }
                }
                // invariant: advance to pc +1 is iff instruction at pc executed without aborting
                self.pc += 1;
            }
            // ok we are out, it's a branch, check the pc for good luck
            // TODO: re-work the logic here. Tests should have a more
            // natural way to plug in
            if self.pc as usize >= code.len() {
                if cfg!(test) {
                    // In order to test the behavior of an instruction stream, hitting end of the
                    // code should report no error so that we can check the
                    // locals.
                    return Ok(ExitCode::Return);
                } else {
                    return Err(PartialVMError::new(StatusCode::PC_OVERFLOW));
                }
            }
        }
    }

    fn ty_args(&self) -> &[Type] {
        &self.ty_args
    }

    fn resolver<'a>(&self, loader: &'a Loader) -> Resolver<'a> {
        self.function.get_resolver(loader)
    }

    fn location(&self) -> Location {
        match self.function.module_id() {
            None => Location::Script,
            Some(id) => Location::Module(id.clone()),
        }
    }

    fn extract_types_and_arguments(
        &self,
        loader: &Loader,
        data_store: &mut impl DataStore,
    ) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        if self.function.module_id().is_none() {
            return (vec![], vec![]);
        }

        let function_parameters_result = loader
            .function_parameters(
                &self.function,
                self.function.module_id().unwrap(),
                data_store,
            )
            .unwrap()
            .into_iter()
            .map(|ty| ty.subst(&self.ty_args))
            .collect::<PartialVMResult<Vec<_>>>();

        if function_parameters_result.is_err() {

        }

        let function_parameters: Vec<Type> = match function_parameters_result {
            Err(err) => {
                warn!("Failed to extract function parameters, {}", err);

                return (vec![], vec![]);
            }
            Ok(value) => value
        };

        let mut args_types: Vec<Vec<u8>> = vec![];
        let mut args_values: Vec<Vec<u8>> = vec![];

        for i in 0..function_parameters.len() {
            let arg_type_result = function_parameters.get(i.clone());

            let arg_type = match arg_type_result {
                Some(arg_type) => arg_type,
                None => {
                    warn!("Failed to get function parameter by index {}", i);

                    continue;
                }
            };

            let arg_type_layout: MoveTypeLayout;
            let arg_value: Vec<u8>;

            match arg_type {
                Type::Reference(_) | Type::MutableReference(_) => {
                    arg_type_layout = match loader.reference_type_to_type_layout(arg_type) {
                        Err(err) => {
                            warn!(
                            "Failed to convert reference type to type layout, error: {}",
                            err
                        );

                            continue;
                        }
                        Ok(value) => value
                    };

                    let value = match self.locals.copy_loc(i.clone()) {
                        Err(err) => {
                            warn!(
                            "Failed to copy primitive form loc: {}, error: {}",
                            i.clone(),
                            err
                        );

                            continue;
                        },
                        Ok(value) => value
                    };

                    let ref_value = match value.value_as::<Reference>() {
                        Err(err) => {
                            warn!(
                            "Failed to convert reference as value reference, error: {}",
                            err
                        );

                            continue;
                        }
                        Ok(value) => value
                    };

                    let ref_arg_value = match ref_value.read_ref() {
                        Err(err) => {
                            warn!(
                            "Failed to read reference, error: {}",
                            err
                        );

                            continue;
                        }
                        Ok(value) => value
                    };

                    let arg_value_result = ref_arg_value.simple_serialize(&arg_type_layout);

                    arg_value = match arg_value_result {
                        Some(arg_value) => arg_value,
                        None => {
                            warn!("Failed to serialize primitive");

                            vec![]
                        }
                    };
                }
                _ => {
                    arg_type_layout = match loader.type_to_type_layout(&arg_type) {
                        Err(err) => {
                            warn!(
                                "Failed to convert primitive type to type layout, error: {}",
                                err
                            );

                            continue;
                        }
                        Ok(value) => value
                    };

                    let value = match self.locals.copy_loc(i.clone()) {
                        Err(err) => {
                            warn!(
                                "Failed to copy primitive form loc: {}, error: {}",
                                i.clone(),
                                err
                            );

                            continue;
                        }
                        Ok(value) => value
                    };

                    arg_value = match value.simple_serialize(&arg_type_layout) {
                        Some(arg_value) => arg_value,
                        None => {
                            warn!("Failed to serialize primitive");

                            vec![]
                        }
                    };
                }
            };

            args_types.push(arg_type_layout.to_string().into_bytes());
            args_values.push(arg_value);
        }

        (args_types, args_values)
    }
}
