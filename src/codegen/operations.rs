use melior::{
    dialect::{
        arith,
        arith::CmpiPredicate,
        cf,
        llvm::{self, r#type::pointer, LoadStoreOptions},
        ods,
    },
    ir::{
        attribute::IntegerAttribute, r#type::IntegerType, Attribute, Block, BlockRef, Location,
        Region,
    },
};

use super::context::OperationCtx;
use crate::{
    constants::{gas_cost, MEMORY_PTR_GLOBAL, MEMORY_SIZE_GLOBAL},
    errors::CodegenError,
    program::Operation,
    syscall::ExitStatusCode,
    utils::{
        allocate_and_store_value, check_if_zero, check_stack_has_at_least,
        check_stack_has_space_for, compare_values, compute_log_dynamic_gas,
        constant_value_from_i64, consume_gas, consume_gas_as_value, extend_memory,
        get_nth_from_stack, get_remaining_gas, get_stack_pointer, inc_stack_pointer,
        integer_constant_from_i64, llvm_mlir, return_empty_result, return_result_from_stack,
        stack_pop, stack_push, swap_stack_elements,
    },
};

use num_bigint::BigUint;

/// Generates blocks for target [`Operation`].
/// Returns both the starting block, and the unterminated last block of the generated code.
pub fn generate_code_for_op<'c>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'c Region<'c>,
    op: Operation,
) -> Result<(BlockRef<'c, 'c>, BlockRef<'c, 'c>), CodegenError> {
    match op {
        Operation::Stop => codegen_stop(op_ctx, region),
        Operation::Push0 => codegen_push(op_ctx, region, BigUint::ZERO, true),
        Operation::Push((_, x)) => codegen_push(op_ctx, region, x, false),
        Operation::Add => codegen_add(op_ctx, region),
        Operation::Mul => codegen_mul(op_ctx, region),
        Operation::Sub => codegen_sub(op_ctx, region),
        Operation::Div => codegen_div(op_ctx, region),
        Operation::Sdiv => codegen_sdiv(op_ctx, region),
        Operation::Mod => codegen_mod(op_ctx, region),
        Operation::SMod => codegen_smod(op_ctx, region),
        Operation::Addmod => codegen_addmod(op_ctx, region),
        Operation::Mulmod => codegen_mulmod(op_ctx, region),
        Operation::Exp => codegen_exp(op_ctx, region),
        Operation::SignExtend => codegen_signextend(op_ctx, region),
        Operation::Lt => codegen_lt(op_ctx, region),
        Operation::Gt => codegen_gt(op_ctx, region),
        Operation::Slt => codegen_slt(op_ctx, region),
        Operation::Sgt => codegen_sgt(op_ctx, region),
        Operation::Eq => codegen_eq(op_ctx, region),
        Operation::IsZero => codegen_iszero(op_ctx, region),
        Operation::And => codegen_and(op_ctx, region),
        Operation::Or => codegen_or(op_ctx, region),
        Operation::Xor => codegen_xor(op_ctx, region),
        Operation::Byte => codegen_byte(op_ctx, region),
        Operation::Shr => codegen_shr(op_ctx, region),
        Operation::Shl => codegen_shl(op_ctx, region),
        Operation::Sar => codegen_sar(op_ctx, region),
        Operation::Codesize => codegen_codesize(op_ctx, region),
        Operation::Pop => codegen_pop(op_ctx, region),
        Operation::Mload => codegen_mload(op_ctx, region),
        Operation::Jump => codegen_jump(op_ctx, region),
        Operation::Jumpi => codegen_jumpi(op_ctx, region),
        Operation::PC { pc } => codegen_pc(op_ctx, region, pc),
        Operation::Msize => codegen_msize(op_ctx, region),
        Operation::Gas => codegen_gas(op_ctx, region),
        Operation::Jumpdest { pc } => codegen_jumpdest(op_ctx, region, pc),
        Operation::Mcopy => codegen_mcopy(op_ctx, region),
        Operation::Dup(x) => codegen_dup(op_ctx, region, x),
        Operation::Swap(x) => codegen_swap(op_ctx, region, x),
        Operation::Return => codegen_return(op_ctx, region),
        Operation::Revert => codegen_revert(op_ctx, region),
        Operation::Mstore => codegen_mstore(op_ctx, region),
        Operation::Mstore8 => codegen_mstore8(op_ctx, region),
        Operation::Log(x) => codegen_log(op_ctx, region, x),
        Operation::CalldataLoad => codegen_calldataload(op_ctx, region),
        Operation::CallDataSize => codegen_calldatasize(op_ctx, region),
    }
}

fn codegen_calldatasize<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    let gas_flag = consume_gas(context, &start_block, gas_cost::CALLDATASIZE)?;

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        gas_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    // Get the calldata size using a syscall
    let uint256 = IntegerType::new(context, 256).into();
    let calldatasize = op_ctx.get_calldata_size_syscall(&ok_block, location)?;
    let extended_size = ok_block
        .append_operation(arith::extui(calldatasize, uint256, location))
        .result(0)?
        .into();
    stack_push(context, &ok_block, extended_size)?;

    Ok((start_block, ok_block))
}

fn codegen_exp<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::EXP)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();
    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(ods::math::ipowi(context, rhs, lhs, location).into())
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_iszero<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 1)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::ISZERO)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let value = stack_pop(context, &ok_block)?;
    let zero_constant = constant_value_from_i64(context, &ok_block, 0)?;

    let result = ok_block
        .append_operation(arith::cmpi(
            context,
            arith::CmpiPredicate::Eq,
            value,
            zero_constant,
            location,
        ))
        .result(0)?
        .into();

    //Extend the 1 bit result to 256 bits.
    let uint256 = IntegerType::new(context, 256);
    let result = ok_block
        .append_operation(arith::extui(result, uint256.into(), location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_and<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::AND)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::andi(lhs, rhs, location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_gt<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::GT)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();
    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let rhs = stack_pop(context, &ok_block)?;
    let lhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::cmpi(
            context,
            arith::CmpiPredicate::Ugt,
            lhs,
            rhs,
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_or<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::OR)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::ori(lhs, rhs, location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_lt<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::LT)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::cmpi(
            context,
            arith::CmpiPredicate::Ult,
            lhs,
            rhs,
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_sgt<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::SGT)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::cmpi(
            context,
            arith::CmpiPredicate::Sgt,
            lhs,
            rhs,
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_eq<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::EQ)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();
    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::cmpi(
            context,
            arith::CmpiPredicate::Eq,
            lhs,
            rhs,
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_push<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
    value_to_push: BigUint,
    is_zero: bool,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough space in stack
    let flag = check_stack_has_space_for(context, &start_block, 1)?;
    let gas_cost = if is_zero {
        gas_cost::PUSH0
    } else {
        gas_cost::PUSHN
    };
    let gas_flag = consume_gas(context, &start_block, gas_cost)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let constant_value = Attribute::parse(context, &format!("{} : i256", value_to_push)).unwrap();
    let constant_value = ok_block
        .append_operation(arith::constant(context, constant_value, location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, constant_value)?;

    Ok((start_block, ok_block))
}

fn codegen_dup<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
    nth: u8,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    debug_assert!(nth > 0 && nth <= 16);
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, nth as u32)?;

    let gas_flag = consume_gas(context, &start_block, gas_cost::DUPN)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();
    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let (nth_value, _) = get_nth_from_stack(context, &ok_block, nth)?;

    stack_push(context, &ok_block, nth_value)?;

    Ok((start_block, ok_block))
}

fn codegen_swap<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
    nth: u8,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    debug_assert!(nth > 0 && nth <= 16);
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, (nth + 1) as u32)?;

    let gas_flag = consume_gas(context, &start_block, gas_cost::SWAPN)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    swap_stack_elements(context, &ok_block, 1, nth + 1)?;

    Ok((start_block, ok_block))
}

fn codegen_add<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;

    let gas_flag = consume_gas(context, &start_block, gas_cost::ADD)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::addi(lhs, rhs, location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_sub<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;

    let gas_flag = consume_gas(context, &start_block, gas_cost::SUB)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::subi(lhs, rhs, location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_div<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let stack_size_flag = check_stack_has_at_least(context, &start_block, 2)?;

    // Check there's enough gas to compute the operation
    let gas_flag = consume_gas(context, &start_block, gas_cost::DIV)?;

    let ok_flag = start_block
        .append_operation(arith::andi(stack_size_flag, gas_flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        ok_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let num = stack_pop(context, &ok_block)?;
    let den = stack_pop(context, &ok_block)?;

    let den_is_zero = check_if_zero(context, &ok_block, &den)?;
    let den_zero_bloq = region.append_block(Block::new(&[]));
    let den_not_zero_bloq = region.append_block(Block::new(&[]));
    let return_block = region.append_block(Block::new(&[]));

    // Denominator is zero path
    let zero_value = constant_value_from_i64(context, &den_zero_bloq, 0i64)?;
    stack_push(context, &den_zero_bloq, zero_value)?;
    den_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    // Denominator is not zero path
    let result = den_not_zero_bloq
        .append_operation(arith::divui(num, den, location))
        .result(0)?
        .into();

    stack_push(context, &den_not_zero_bloq, result)?;
    den_not_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    // Branch to den_zero if den_is_zero == true; else branch to den_not_zero
    ok_block.append_operation(cf::cond_br(
        context,
        den_is_zero,
        &den_zero_bloq,
        &den_not_zero_bloq,
        &[],
        &[],
        location,
    ));

    Ok((start_block, return_block))
}

fn codegen_sdiv<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let stack_size_flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::SDIV)?;

    let ok_flag = start_block
        .append_operation(arith::andi(stack_size_flag, gas_flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        ok_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let num = stack_pop(context, &ok_block)?;
    let den = stack_pop(context, &ok_block)?;
    let den_is_zero = check_if_zero(context, &ok_block, &den)?;
    let den_zero_bloq = region.append_block(Block::new(&[]));
    let den_not_zero_bloq = region.append_block(Block::new(&[]));
    let return_block = region.append_block(Block::new(&[]));

    // Denominator is zero path
    let zero_value = constant_value_from_i64(context, &den_zero_bloq, 0i64)?;
    stack_push(context, &den_zero_bloq, zero_value)?;
    den_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    // Denominator is not zero path
    let result = den_not_zero_bloq
        .append_operation(ods::llvm::sdiv(context, num, den, location).into())
        .result(0)?
        .into();

    stack_push(context, &den_not_zero_bloq, result)?;
    den_not_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    // Branch to den_zero if den_is_zero == true; else branch to den_not_zero
    ok_block.append_operation(cf::cond_br(
        context,
        den_is_zero,
        &den_zero_bloq,
        &den_not_zero_bloq,
        &[],
        &[],
        location,
    ));

    Ok((start_block, return_block))
}

fn codegen_mul<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let stack_size_flag = check_stack_has_at_least(context, &start_block, 2)?;
    // Check there's enough gas to compute the operation
    let gas_flag = consume_gas(context, &start_block, gas_cost::MUL)?;

    let ok_flag = start_block
        .append_operation(arith::andi(stack_size_flag, gas_flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        ok_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::muli(lhs, rhs, location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_mod<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::MOD)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let num = stack_pop(context, &ok_block)?;
    let den = stack_pop(context, &ok_block)?;

    let den_is_zero = check_if_zero(context, &ok_block, &den)?;
    let den_zero_bloq = region.append_block(Block::new(&[]));
    let den_not_zero_bloq = region.append_block(Block::new(&[]));
    let return_block = region.append_block(Block::new(&[]));

    let constant_value = den_zero_bloq
        .append_operation(arith::constant(
            context,
            integer_constant_from_i64(context, 0i64).into(),
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &den_zero_bloq, constant_value)?;

    den_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    let mod_result = den_not_zero_bloq
        .append_operation(arith::remui(num, den, location))
        .result(0)?
        .into();

    stack_push(context, &den_not_zero_bloq, mod_result)?;

    den_not_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    ok_block.append_operation(cf::cond_br(
        context,
        den_is_zero,
        &den_zero_bloq,
        &den_not_zero_bloq,
        &[],
        &[],
        location,
    ));

    Ok((start_block, return_block))
}

fn codegen_smod<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::SMOD)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let num = stack_pop(context, &ok_block)?;
    let den = stack_pop(context, &ok_block)?;

    let den_is_zero = check_if_zero(context, &ok_block, &den)?;
    let den_zero_bloq = region.append_block(Block::new(&[]));
    let den_not_zero_bloq = region.append_block(Block::new(&[]));
    let return_block = region.append_block(Block::new(&[]));

    let constant_value = den_zero_bloq
        .append_operation(arith::constant(
            context,
            integer_constant_from_i64(context, 0i64).into(),
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &den_zero_bloq, constant_value)?;

    den_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    let mod_result = den_not_zero_bloq
        .append_operation(ods::llvm::srem(context, num, den, location).into())
        .result(0)?
        .into();

    stack_push(context, &den_not_zero_bloq, mod_result)?;

    den_not_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    ok_block.append_operation(cf::cond_br(
        context,
        den_is_zero,
        &den_zero_bloq,
        &den_not_zero_bloq,
        &[],
        &[],
        location,
    ));

    Ok((start_block, return_block))
}

fn codegen_addmod<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 3)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::ADDMOD)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let a = stack_pop(context, &ok_block)?;
    let b = stack_pop(context, &ok_block)?;
    let den = stack_pop(context, &ok_block)?;

    let den_is_zero = check_if_zero(context, &ok_block, &den)?;
    let den_zero_bloq = region.append_block(Block::new(&[]));
    let den_not_zero_bloq = region.append_block(Block::new(&[]));
    let return_block = region.append_block(Block::new(&[]));

    let constant_value = den_zero_bloq
        .append_operation(arith::constant(
            context,
            integer_constant_from_i64(context, 0i64).into(),
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &den_zero_bloq, constant_value)?;

    den_zero_bloq.append_operation(cf::br(&return_block, &[], location));
    let uint256 = IntegerType::new(context, 256).into();
    let uint257 = IntegerType::new(context, 257).into();

    // extend the operands to 257 bits before the addition
    let extended_a = den_not_zero_bloq
        .append_operation(arith::extui(a, uint257, location))
        .result(0)?
        .into();
    let extended_b = den_not_zero_bloq
        .append_operation(arith::extui(b, uint257, location))
        .result(0)?
        .into();
    let extended_den = den_not_zero_bloq
        .append_operation(arith::extui(den, uint257, location))
        .result(0)?
        .into();
    let add_result = den_not_zero_bloq
        .append_operation(arith::addi(extended_a, extended_b, location))
        .result(0)?
        .into();
    let mod_result = den_not_zero_bloq
        .append_operation(arith::remui(add_result, extended_den, location))
        .result(0)?
        .into();
    let truncated_result = den_not_zero_bloq
        .append_operation(arith::trunci(mod_result, uint256, location))
        .result(0)?
        .into();

    stack_push(context, &den_not_zero_bloq, truncated_result)?;

    den_not_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    ok_block.append_operation(cf::cond_br(
        context,
        den_is_zero,
        &den_zero_bloq,
        &den_not_zero_bloq,
        &[],
        &[],
        location,
    ));

    Ok((start_block, return_block))
}

fn codegen_mulmod<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 3)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::MULMOD)?;
    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let a = stack_pop(context, &ok_block)?;
    let b = stack_pop(context, &ok_block)?;
    let den = stack_pop(context, &ok_block)?;

    let den_is_zero = check_if_zero(context, &ok_block, &den)?;
    let den_zero_bloq = region.append_block(Block::new(&[]));
    let den_not_zero_bloq = region.append_block(Block::new(&[]));
    let return_block = region.append_block(Block::new(&[]));

    let constant_value = den_zero_bloq
        .append_operation(arith::constant(
            context,
            integer_constant_from_i64(context, 0i64).into(),
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &den_zero_bloq, constant_value)?;

    den_zero_bloq.append_operation(cf::br(&return_block, &[], location));

    let uint256 = IntegerType::new(context, 256).into();
    let uint512 = IntegerType::new(context, 512).into();

    // extend the operands to 512 bits before the multiplication
    let extended_a = den_not_zero_bloq
        .append_operation(arith::extui(a, uint512, location))
        .result(0)?
        .into();
    let extended_b = den_not_zero_bloq
        .append_operation(arith::extui(b, uint512, location))
        .result(0)?
        .into();
    let extended_den = den_not_zero_bloq
        .append_operation(arith::extui(den, uint512, location))
        .result(0)?
        .into();

    let mul_result = den_not_zero_bloq
        .append_operation(arith::muli(extended_a, extended_b, location))
        .result(0)?
        .into();
    let mod_result = den_not_zero_bloq
        .append_operation(arith::remui(mul_result, extended_den, location))
        .result(0)?
        .into();
    let truncated_result = den_not_zero_bloq
        .append_operation(arith::trunci(mod_result, uint256, location))
        .result(0)?
        .into();

    stack_push(context, &den_not_zero_bloq, truncated_result)?;
    den_not_zero_bloq.append_operation(cf::br(&return_block, &[], location));
    ok_block.append_operation(cf::cond_br(
        context,
        den_is_zero,
        &den_zero_bloq,
        &den_not_zero_bloq,
        &[],
        &[],
        location,
    ));
    Ok((start_block, return_block))
}

fn codegen_xor<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;

    let gas_flag = consume_gas(context, &start_block, gas_cost::XOR)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::xori(lhs, rhs, location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_shr<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);
    let uint256 = IntegerType::new(context, 256);

    // Check there's enough elements in stack
    let mut flag = check_stack_has_at_least(context, &start_block, 2)?;

    let gas_flag = consume_gas(context, &start_block, 3)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let shift = stack_pop(context, &ok_block)?;
    let value = stack_pop(context, &ok_block)?;

    let value_255 = ok_block
        .append_operation(arith::constant(
            context,
            IntegerAttribute::new(uint256.into(), 255_i64).into(),
            location,
        ))
        .result(0)?
        .into();

    flag = compare_values(context, &ok_block, CmpiPredicate::Ult, shift, value_255)?;

    let ok_ok_block = region.append_block(Block::new(&[]));
    let altv_block = region.append_block(Block::new(&[]));
    // to unify the blocks after the branching
    let empty_block = region.append_block(Block::new(&[]));

    ok_block.append_operation(cf::cond_br(
        context,
        flag,
        &ok_ok_block,
        &altv_block,
        &[],
        &[],
        location,
    ));

    // if shift is less than 255
    let result = ok_ok_block
        .append_operation(arith::shrui(value, shift, location))
        .result(0)?
        .into();

    stack_push(context, &ok_ok_block, result)?;

    ok_ok_block.append_operation(cf::br(&empty_block, &[], location));

    // if shift is greater than 255
    let result = altv_block
        .append_operation(arith::constant(
            context,
            IntegerAttribute::new(uint256.into(), 0_i64).into(),
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &altv_block, result)?;

    altv_block.append_operation(cf::br(&empty_block, &[], location));

    Ok((start_block, empty_block))
}

fn codegen_shl<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);
    let uint256 = IntegerType::new(context, 256);

    // Check there's enough elements in stack
    let mut flag = check_stack_has_at_least(context, &start_block, 2)?;

    let gas_flag = consume_gas(context, &start_block, gas_cost::SHL)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let shift = stack_pop(context, &ok_block)?;
    let value = stack_pop(context, &ok_block)?;

    let value_255 = ok_block
        .append_operation(arith::constant(
            context,
            IntegerAttribute::new(uint256.into(), 255_i64).into(),
            location,
        ))
        .result(0)?
        .into();

    flag = compare_values(context, &ok_block, CmpiPredicate::Ult, shift, value_255)?;

    let ok_ok_block = region.append_block(Block::new(&[]));
    let altv_block = region.append_block(Block::new(&[]));
    // to unify the blocks after the branching
    let empty_block = region.append_block(Block::new(&[]));

    ok_block.append_operation(cf::cond_br(
        context,
        flag,
        &ok_ok_block,
        &altv_block,
        &[],
        &[],
        location,
    ));

    // if shift is less than 255
    let result = ok_ok_block
        .append_operation(arith::shli(value, shift, location))
        .result(0)?
        .into();

    stack_push(context, &ok_ok_block, result)?;

    ok_ok_block.append_operation(cf::br(&empty_block, &[], location));

    // if shift is greater than 255
    let result = altv_block
        .append_operation(arith::constant(
            context,
            IntegerAttribute::new(uint256.into(), 0_i64).into(),
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &altv_block, result)?;

    altv_block.append_operation(cf::br(&empty_block, &[], location));

    Ok((start_block, empty_block))
}

fn codegen_pop<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's at least 1 element in stack
    let flag = check_stack_has_at_least(context, &start_block, 1)?;

    let gas_flag = consume_gas(context, &start_block, gas_cost::POP)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    stack_pop(context, &ok_block)?;

    Ok((start_block, ok_block))
}

fn codegen_mload<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);
    let uint256 = IntegerType::new(context, 256);
    let uint32 = IntegerType::new(context, 32);
    let uint8 = IntegerType::new(context, 8);
    let ptr_type = pointer(context, 0);

    let stack_flag = check_stack_has_at_least(context, &start_block, 1)?;
    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        stack_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let offset = stack_pop(context, &ok_block)?;

    // Compute required memory size
    let offset = ok_block
        .append_operation(arith::trunci(offset, uint32.into(), location))
        .result(0)
        .unwrap()
        .into();
    let value_size = ok_block
        .append_operation(arith::constant(
            context,
            IntegerAttribute::new(uint32.into(), 32).into(),
            location,
        ))
        .result(0)?
        .into();
    let required_size = ok_block
        .append_operation(arith::addi(offset, value_size, location))
        .result(0)?
        .into();

    let memory_access_block = region.append_block(Block::new(&[]));

    extend_memory(
        op_ctx,
        &ok_block,
        &memory_access_block,
        region,
        required_size,
        gas_cost::MLOAD,
    )?;

    // Memory access
    let memory_ptr_ptr = memory_access_block
        .append_operation(llvm_mlir::addressof(
            context,
            MEMORY_PTR_GLOBAL,
            ptr_type,
            location,
        ))
        .result(0)?;

    let memory_ptr = memory_access_block
        .append_operation(llvm::load(
            context,
            memory_ptr_ptr.into(),
            ptr_type,
            location,
            LoadStoreOptions::default(),
        ))
        .result(0)?
        .into();

    let memory_destination = memory_access_block
        .append_operation(llvm::get_element_ptr_dynamic(
            context,
            memory_ptr,
            &[offset],
            uint8.into(),
            ptr_type,
            location,
        ))
        .result(0)?
        .into();

    let read_value = memory_access_block
        .append_operation(llvm::load(
            context,
            memory_destination,
            uint256.into(),
            location,
            LoadStoreOptions::new()
                .align(IntegerAttribute::new(IntegerType::new(context, 64).into(), 1).into()),
        ))
        .result(0)?
        .into();

    // check system endianness before pushing the value
    let read_value = if cfg!(target_endian = "little") {
        // if the system is little endian, we convert the value to big endian
        memory_access_block
            .append_operation(llvm::intr_bswap(read_value, uint256.into(), location))
            .result(0)?
            .into()
    } else {
        // if the system is big endian, there is no need to convert the value
        read_value
    };

    stack_push(context, &memory_access_block, read_value)?;

    Ok((start_block, memory_access_block))
}

fn codegen_codesize<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);
    let uint256 = IntegerType::new(context, 256);

    // Check there's stack overflow
    let stack_flag = check_stack_has_space_for(context, &start_block, 1)?;
    // Check there's enough gas
    let gas_flag = consume_gas(context, &start_block, gas_cost::CODESIZE)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, stack_flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let codesize = ok_block
        .append_operation(arith::constant(
            context,
            IntegerAttribute::new(uint256.into(), op_ctx.program.code_size as i64).into(),
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &ok_block, codesize)?;

    Ok((start_block, ok_block))
}

fn codegen_sar<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    // Check there's enough gas
    let gas_flag = consume_gas(context, &start_block, gas_cost::SAR)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let shift = stack_pop(context, &ok_block)?;
    let value = stack_pop(context, &ok_block)?;

    // max_shift = 255
    let max_shift = ok_block
        .append_operation(arith::constant(
            context,
            integer_constant_from_i64(context, 255).into(),
            location,
        ))
        .result(0)?
        .into();

    // if shift > 255  then after applying the `shrsi` operation the result will be poisoned
    // to avoid the poisoning we set shift = min(shift, 255)
    let shift = ok_block
        .append_operation(arith::minui(shift, max_shift, location))
        .result(0)?
        .into();

    let result = ok_block
        .append_operation(arith::shrsi(value, shift, location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_byte<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    // Check there's enough gas
    let gas_flag = consume_gas(context, &start_block, gas_cost::BYTE)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    // in out_of_bounds_block a 0 is pushed to the stack
    let out_of_bounds_block = region.append_block(Block::new(&[]));

    // in offset_ok_block the byte operation is performed
    let offset_ok_block = region.append_block(Block::new(&[]));

    let end_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let offset = stack_pop(context, &ok_block)?;
    let value = stack_pop(context, &ok_block)?;

    const BITS_PER_BYTE: u8 = 8;
    const MAX_SHIFT: u8 = 31;

    let constant_bits_per_byte = constant_value_from_i64(context, &ok_block, BITS_PER_BYTE as i64)?;
    let constant_max_shift_in_bits =
        constant_value_from_i64(context, &ok_block, (MAX_SHIFT * BITS_PER_BYTE) as i64)?;

    let offset_in_bits = ok_block
        .append_operation(arith::muli(offset, constant_bits_per_byte, location))
        .result(0)?
        .into();

    // compare  offset > max_shift?
    let is_offset_out_of_bounds = ok_block
        .append_operation(arith::cmpi(
            context,
            arith::CmpiPredicate::Ugt,
            offset_in_bits,
            constant_max_shift_in_bits,
            location,
        ))
        .result(0)?
        .into();

    // if offset > max_shift => branch to out_of_bounds_block
    // else => branch to offset_ok_block
    ok_block.append_operation(cf::cond_br(
        context,
        is_offset_out_of_bounds,
        &out_of_bounds_block,
        &offset_ok_block,
        &[],
        &[],
        location,
    ));

    let zero_constant_value = constant_value_from_i64(context, &out_of_bounds_block, 0_i64)?;

    // push zero to the stack
    stack_push(context, &out_of_bounds_block, zero_constant_value)?;

    out_of_bounds_block.append_operation(cf::br(&end_block, &[], location));

    // the idea is to use a right shift to place the byte in the right-most side
    // and then apply a bitwise AND with a 0xFF mask
    //
    // for example, if we want to extract the 0xFF byte in the following value
    // (for simplicity the value has fewer bytes than it has in reality)
    //
    // value = 0xAABBCCDDFFAABBCC
    //                   ^^
    //              desired byte
    //
    // we can shift the value to the right
    //
    // value = 0xAABBCCDDFFAABBCC -> 0x000000AABBCCDDFF
    //                   ^^                          ^^
    // and then apply the bitwise AND it to the right to remove the right-side bytes
    //
    //  value = 0x000000AABBCCDDFF
    //          AND
    //  mask  = 0x00000000000000FF
    //------------------------------
    // result = 0x00000000000000FF

    // compute how many bits the value has to be shifted
    // shift_right_in_bits = max_shift - offset
    let shift_right_in_bits = offset_ok_block
        .append_operation(arith::subi(
            constant_max_shift_in_bits,
            offset_in_bits,
            location,
        ))
        .result(0)?
        .into();

    // shift the value to the right
    let shifted_right_value = offset_ok_block
        .append_operation(arith::shrui(value, shift_right_in_bits, location))
        .result(0)?
        .into();

    let mask = offset_ok_block
        .append_operation(arith::constant(
            context,
            integer_constant_from_i64(context, 0xff).into(),
            location,
        ))
        .result(0)?
        .into();

    // compute (value AND mask)
    let result = offset_ok_block
        .append_operation(arith::andi(shifted_right_value, mask, location))
        .result(0)?
        .into();

    stack_push(context, &offset_ok_block, result)?;

    offset_ok_block.append_operation(cf::br(&end_block, &[], location));

    Ok((start_block, end_block))
}

fn codegen_jumpdest<'c>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'c Region<'c>,
    pc: usize,
) -> Result<(BlockRef<'c, 'c>, BlockRef<'c, 'c>), CodegenError> {
    let landing_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough gas to compute the operation
    let gas_flag = consume_gas(context, &landing_block, gas_cost::JUMPDEST)?;

    let ok_block = region.append_block(Block::new(&[]));

    landing_block.append_operation(cf::cond_br(
        context,
        gas_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    // Register jumpdest block in context
    op_ctx.register_jump_destination(pc, landing_block);

    Ok((landing_block, ok_block))
}

fn codegen_jumpi<'c, 'r: 'c>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;
    // Check there's enough gas
    let gas_flag = consume_gas(context, &start_block, gas_cost::JUMPI)?;

    let ok_block = region.append_block(Block::new(&[]));

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let pc = stack_pop(context, &ok_block)?;
    let condition = stack_pop(context, &ok_block)?;

    let false_block = region.append_block(Block::new(&[]));

    let zero = ok_block
        .append_operation(arith::constant(
            context,
            integer_constant_from_i64(context, 0i64).into(),
            location,
        ))
        .result(0)?
        .into();

    // compare  condition != 0  to convert condition from u256 to 1-bit signless integer
    let condition = ok_block
        .append_operation(arith::cmpi(
            context,
            arith::CmpiPredicate::Ne,
            condition,
            zero,
            location,
        ))
        .result(0)?;

    ok_block.append_operation(cf::cond_br(
        context,
        condition.into(),
        &op_ctx.jumptable_block,
        &false_block,
        &[pc],
        &[],
        location,
    ));

    Ok((start_block, false_block))
}

fn codegen_jump<'c, 'r: 'c>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    // it reverts if Counter offset is not a JUMPDEST.
    // The error is generated even if the JUMP would not have been done

    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 1)?;
    // Check there's enough gas
    let gas_flag = consume_gas(context, &start_block, gas_cost::JUMP)?;

    let ok_block = region.append_block(Block::new(&[]));

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let pc = stack_pop(context, &ok_block)?;

    // appends operation to ok_block to jump to the `jump table block``
    // in the jump table block the pc is checked and if its ok
    // then it jumps to the block associated with that pc
    op_ctx.add_jump_op(ok_block, pc, location);

    // TODO: we are creating an empty block that won't ever be reached
    // probably there's a better way to do this
    let empty_block = region.append_block(Block::new(&[]));
    Ok((start_block, empty_block))
}

fn codegen_pc<'c>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'c Region<'c>,
    pc: usize,
) -> Result<(BlockRef<'c, 'c>, BlockRef<'c, 'c>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    let stack_size_flag = check_stack_has_space_for(context, &start_block, 1)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::PC)?;

    let ok_flag = start_block
        .append_operation(arith::andi(stack_size_flag, gas_flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        ok_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let pc_value = ok_block
        .append_operation(arith::constant(
            context,
            integer_constant_from_i64(context, pc as i64).into(),
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &ok_block, pc_value)?;

    Ok((start_block, ok_block))
}

fn codegen_msize<'c>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'c Region<'c>,
) -> Result<(BlockRef<'c, 'c>, BlockRef<'c, 'c>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = op_ctx.mlir_context;
    let location = Location::unknown(context);

    let ptr_type = pointer(context, 0);
    let uint32 = IntegerType::new(context, 32).into();
    let uint256 = IntegerType::new(context, 256).into();

    let stack_flag = check_stack_has_space_for(context, &start_block, 1)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::MSIZE)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, stack_flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    // Get address of memory size global
    let memory_ptr = ok_block
        .append_operation(llvm_mlir::addressof(
            context,
            MEMORY_SIZE_GLOBAL,
            ptr_type,
            location,
        ))
        .result(0)?;

    // Load memory size
    let memory_size = ok_block
        .append_operation(llvm::load(
            context,
            memory_ptr.into(),
            uint32,
            location,
            LoadStoreOptions::default(),
        ))
        .result(0)?
        .into();

    let memory_size_extended = ok_block
        .append_operation(arith::extui(memory_size, uint256, location))
        .result(0)?
        .into();

    stack_push(context, &ok_block, memory_size_extended)?;

    Ok((start_block, ok_block))
}

fn codegen_return<'c>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'c Region<'c>,
) -> Result<(BlockRef<'c, 'c>, BlockRef<'c, 'c>), CodegenError> {
    let context = op_ctx.mlir_context;
    let location = Location::unknown(context);

    let start_block = region.append_block(Block::new(&[]));
    let ok_block = region.append_block(Block::new(&[]));

    let flag = check_stack_has_at_least(context, &start_block, 2)?;

    start_block.append_operation(cf::cond_br(
        context,
        flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    return_result_from_stack(op_ctx, region, &ok_block, ExitStatusCode::Return, location)?;

    let empty_block = region.append_block(Block::new(&[]));

    Ok((start_block, empty_block))
}

// Stop the current context execution, revert the state changes
// (see STATICCALL for a list of state changing opcodes) and
// return the unused gas to the caller. It also reverts the gas refund to i
// ts value before the current context. If the execution is stopped with REVERT,
// the value 0 is put on the stack of the calling context, which continues to execute normally.
// The return data of the calling context is set as the given
// chunk of memory of this context.
fn codegen_revert<'c>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'c Region<'c>,
) -> Result<(BlockRef<'c, 'c>, BlockRef<'c, 'c>), CodegenError> {
    let context = op_ctx.mlir_context;
    let location = Location::unknown(context);

    let start_block = region.append_block(Block::new(&[]));
    let ok_block = region.append_block(Block::new(&[]));

    let flag = check_stack_has_at_least(context, &start_block, 2)?;

    start_block.append_operation(cf::cond_br(
        context,
        flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    return_result_from_stack(op_ctx, region, &ok_block, ExitStatusCode::Revert, location)?;

    let empty_block = region.append_block(Block::new(&[]));

    Ok((start_block, empty_block))
}

fn codegen_stop<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    return_empty_result(op_ctx, &start_block, ExitStatusCode::Stop, location)?;

    let empty_block = region.append_block(Block::new(&[]));

    Ok((start_block, empty_block))
}

fn codegen_signextend<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let stack_size_flag = check_stack_has_at_least(context, &start_block, 2)?;
    let gas_flag = consume_gas(context, &start_block, gas_cost::SIGNEXTEND)?;

    // Check there's enough gas to perform the operation
    let ok_flag = start_block
        .append_operation(arith::andi(stack_size_flag, gas_flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        ok_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let byte_size = stack_pop(context, &ok_block)?;
    let value_to_extend = stack_pop(context, &ok_block)?;

    // Constant definition
    let max_byte_size = constant_value_from_i64(context, &ok_block, 31)?;
    let bits_per_byte = constant_value_from_i64(context, &ok_block, 8)?;
    let sign_bit_position_on_byte = constant_value_from_i64(context, &ok_block, 7)?;
    let max_bits = constant_value_from_i64(context, &ok_block, 255)?;

    // byte_size = min(max_byte_size, byte_size)
    let byte_size = ok_block
        .append_operation(arith::minui(byte_size, max_byte_size, location))
        .result(0)?
        .into();

    // bits_to_shift = max_bits - byte_size * bits_per_byte + sign_bit_position_on_byte
    let byte_number_in_bits = ok_block
        .append_operation(arith::muli(byte_size, bits_per_byte, location))
        .result(0)?
        .into();

    let value_size_in_bits = ok_block
        .append_operation(arith::addi(
            byte_number_in_bits,
            sign_bit_position_on_byte,
            location,
        ))
        .result(0)?
        .into();

    let bits_to_shift = ok_block
        .append_operation(arith::subi(max_bits, value_size_in_bits, location))
        .result(0)?
        .into();

    // value_to_extend << bits_to_shift
    let left_shifted_value = ok_block
        .append_operation(ods::llvm::shl(context, value_to_extend, bits_to_shift, location).into())
        .result(0)?
        .into();

    // value_to_extend >> bits_to_shift  (sign extended)
    let result = ok_block
        .append_operation(
            ods::llvm::ashr(context, left_shifted_value, bits_to_shift, location).into(),
        )
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_gas<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's at least space for one element in the stack
    let stack_size_flag = check_stack_has_space_for(context, &start_block, 1)?;

    // Check there's enough gas to compute the operation
    let gas_flag = consume_gas(context, &start_block, gas_cost::GAS)?;

    let ok_flag = start_block
        .append_operation(arith::andi(stack_size_flag, gas_flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        ok_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let gas = get_remaining_gas(context, &ok_block)?;

    let gas_extended = ok_block
        .append_operation(arith::extui(
            gas,
            IntegerType::new(context, 256).into(),
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &ok_block, gas_extended)?;

    Ok((start_block, ok_block))
}

fn codegen_slt<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);

    // Check there's enough elements in stack
    let stack_size_flag = check_stack_has_at_least(context, &start_block, 2)?;

    // Check there's enough gas to compute the operation
    let gas_flag = consume_gas(context, &start_block, gas_cost::SLT)?;

    let ok_flag = start_block
        .append_operation(arith::andi(stack_size_flag, gas_flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        ok_flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let lhs = stack_pop(context, &ok_block)?;
    let rhs = stack_pop(context, &ok_block)?;

    let result = ok_block
        .append_operation(arith::cmpi(
            context,
            arith::CmpiPredicate::Slt,
            lhs,
            rhs,
            location,
        ))
        .result(0)?
        .into();

    stack_push(context, &ok_block, result)?;

    Ok((start_block, ok_block))
}

fn codegen_mstore<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);
    let uint32 = IntegerType::new(context, 32);
    let uint8 = IntegerType::new(context, 8);
    let ptr_type = pointer(context, 0);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let offset = stack_pop(context, &ok_block)?;
    let value = stack_pop(context, &ok_block)?;

    // truncate offset to 32 bits
    let offset = ok_block
        .append_operation(arith::trunci(offset, uint32.into(), location))
        .result(0)
        .unwrap()
        .into();

    let value_width_in_bytes = 32;
    // value_size = 32
    let value_size = ok_block
        .append_operation(arith::constant(
            context,
            IntegerAttribute::new(uint32.into(), value_width_in_bytes).into(),
            location,
        ))
        .result(0)?
        .into();

    // required_size = offset + value_size
    let required_size = ok_block
        .append_operation(arith::addi(offset, value_size, location))
        .result(0)?
        .into();

    let memory_access_block = region.append_block(Block::new(&[]));

    extend_memory(
        op_ctx,
        &ok_block,
        &memory_access_block,
        region,
        required_size,
        gas_cost::MSTORE,
    )?;

    // Memory access
    let memory_ptr_ptr = memory_access_block
        .append_operation(llvm_mlir::addressof(
            context,
            MEMORY_PTR_GLOBAL,
            ptr_type,
            location,
        ))
        .result(0)?;

    let memory_ptr = memory_access_block
        .append_operation(llvm::load(
            context,
            memory_ptr_ptr.into(),
            ptr_type,
            location,
            LoadStoreOptions::default(),
        ))
        .result(0)?
        .into();

    // memory_destination = memory_ptr + offset
    let memory_destination = memory_access_block
        .append_operation(llvm::get_element_ptr_dynamic(
            context,
            memory_ptr,
            &[offset],
            uint8.into(),
            ptr_type,
            location,
        ))
        .result(0)?
        .into();

    let uint256 = IntegerType::new(context, 256);

    // check system endianness before storing the value
    let value = if cfg!(target_endian = "little") {
        // if the system is little endian, we convert the value to big endian
        memory_access_block
            .append_operation(llvm::intr_bswap(value, uint256.into(), location))
            .result(0)?
            .into()
    } else {
        // if the system is big endian, there is no need to convert the value
        value
    };

    // store the value in the memory
    memory_access_block.append_operation(llvm::store(
        context,
        value,
        memory_destination,
        location,
        LoadStoreOptions::new()
            .align(IntegerAttribute::new(IntegerType::new(context, 64).into(), 1).into()),
    ));

    Ok((start_block, memory_access_block))
}

fn codegen_mstore8<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);
    let uint32 = IntegerType::new(context, 32);
    let uint8 = IntegerType::new(context, 8);
    let ptr_type = pointer(context, 0);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 2)?;

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let offset = stack_pop(context, &ok_block)?;
    let value = stack_pop(context, &ok_block)?;

    // truncate value to the least significative byte of the 32-byte value
    let value = ok_block
        .append_operation(arith::trunci(
            value,
            r#IntegerType::new(context, 8).into(),
            location,
        ))
        .result(0)?
        .into();

    // truncate offset to 32 bits
    let offset = ok_block
        .append_operation(arith::trunci(offset, uint32.into(), location))
        .result(0)
        .unwrap()
        .into();

    let value_width_in_bytes = 1;
    // value_size = 1
    let value_size = ok_block
        .append_operation(arith::constant(
            context,
            IntegerAttribute::new(uint32.into(), value_width_in_bytes).into(),
            location,
        ))
        .result(0)?
        .into();

    // required_size = offset + size
    let required_size = ok_block
        .append_operation(arith::addi(offset, value_size, location))
        .result(0)?
        .into();

    let memory_access_block = region.append_block(Block::new(&[]));

    extend_memory(
        op_ctx,
        &ok_block,
        &memory_access_block,
        region,
        required_size,
        gas_cost::MSTORE8,
    )?;

    // Memory access
    let memory_ptr_ptr = memory_access_block
        .append_operation(llvm_mlir::addressof(
            context,
            MEMORY_PTR_GLOBAL,
            ptr_type,
            location,
        ))
        .result(0)?;

    let memory_ptr = memory_access_block
        .append_operation(llvm::load(
            context,
            memory_ptr_ptr.into(),
            ptr_type,
            location,
            LoadStoreOptions::default(),
        ))
        .result(0)?
        .into();

    // memory_destination = memory_ptr + offset
    let memory_destination = memory_access_block
        .append_operation(llvm::get_element_ptr_dynamic(
            context,
            memory_ptr,
            &[offset],
            uint8.into(),
            ptr_type,
            location,
        ))
        .result(0)?
        .into();

    memory_access_block.append_operation(llvm::store(
        context,
        value,
        memory_destination,
        location,
        LoadStoreOptions::new()
            .align(IntegerAttribute::new(IntegerType::new(context, 64).into(), 1).into()),
    ));

    Ok((start_block, memory_access_block))
}

fn codegen_mcopy<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);
    let uint32 = IntegerType::new(context, 32);
    let uint8 = IntegerType::new(context, 8);
    let ptr_type = pointer(context, 0);

    let flag = check_stack_has_at_least(context, &start_block, 3)?;

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    // where to copy
    let dest_offset = stack_pop(context, &ok_block)?;
    // where to copy from
    let offset = stack_pop(context, &ok_block)?;
    let size = stack_pop(context, &ok_block)?;

    // truncate offset and dest_offset to 32 bits
    let offset = ok_block
        .append_operation(arith::trunci(offset, uint32.into(), location))
        .result(0)?
        .into();

    let dest_offset = ok_block
        .append_operation(arith::trunci(dest_offset, uint32.into(), location))
        .result(0)?
        .into();

    let size = ok_block
        .append_operation(arith::trunci(size, uint32.into(), location))
        .result(0)?
        .into();

    // required_size = offset + size
    let src_required_size = ok_block
        .append_operation(arith::addi(offset, size, location))
        .result(0)?
        .into();

    // dest_required_size = dest_offset + size
    let dest_required_size = ok_block
        .append_operation(arith::addi(dest_offset, size, location))
        .result(0)?
        .into();

    let required_size = ok_block
        .append_operation(arith::maxui(
            src_required_size,
            dest_required_size,
            location,
        ))
        .result(0)?
        .into();

    let memory_access_block = region.append_block(Block::new(&[]));

    extend_memory(
        op_ctx,
        &ok_block,
        &memory_access_block,
        region,
        required_size,
        gas_cost::MCOPY,
    )?;

    // Memory access
    let memory_ptr_ptr = memory_access_block
        .append_operation(llvm_mlir::addressof(
            context,
            MEMORY_PTR_GLOBAL,
            ptr_type,
            location,
        ))
        .result(0)?;

    let memory_ptr = memory_access_block
        .append_operation(llvm::load(
            context,
            memory_ptr_ptr.into(),
            ptr_type,
            location,
            LoadStoreOptions::default(),
        ))
        .result(0)?
        .into();

    let source = memory_access_block
        .append_operation(llvm::get_element_ptr_dynamic(
            context,
            memory_ptr,
            &[offset],
            uint8.into(),
            ptr_type,
            location,
        ))
        .result(0)?
        .into();

    // memory_destination = memory_ptr + dest_offset
    let destination = memory_access_block
        .append_operation(llvm::get_element_ptr_dynamic(
            context,
            memory_ptr,
            &[dest_offset],
            uint8.into(),
            ptr_type,
            location,
        ))
        .result(0)?
        .into();

    memory_access_block.append_operation(
        ods::llvm::intr_memmove(
            context,
            destination,
            source,
            size,
            IntegerAttribute::new(IntegerType::new(context, 1).into(), 0),
            location,
        )
        .into(),
    );

    Ok((start_block, memory_access_block))
}

fn codegen_calldataload<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);
    let uint256 = IntegerType::new(context, 256);
    let uint8 = IntegerType::new(context, 8);
    let uint1 = IntegerType::new(context, 1);
    let ptr_type = pointer(context, 0);

    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, 1)?;
    // Check there's enough gas
    let gas_flag = consume_gas(context, &start_block, gas_cost::CALLDATALOAD)?;

    let condition = start_block
        .append_operation(arith::andi(gas_flag, flag, location))
        .result(0)?
        .into();

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        condition,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let offset = stack_pop(context, &ok_block)?;

    // TODO: add a calldata_ptr and size setup

    let calldata_ptr = op_ctx.get_calldata_ptr_syscall(&ok_block, location)?;

    // max_slice_width = 32
    let max_slice_width = ok_block
        .append_operation(arith::constant(
            context,
            integer_constant_from_i64(context, 32).into(),
            location,
        ))
        .result(0)?
        .into();

    let calldata_size_u32 = op_ctx.get_calldata_size_syscall(&ok_block, location)?;
    // convert calldata_size from u32 to u256
    let calldata_size = ok_block
        .append_operation(arith::extui(calldata_size_u32, uint256.into(), location))
        .result(0)?
        .into();

    let zero = ok_block
        .append_operation(arith::constant(
            context,
            IntegerAttribute::new(uint256.into(), 0).into(),
            location,
        ))
        .result(0)?
        .into();

    let offset_ok_block = region.append_block(Block::new(&[]));
    let offset_bad_block = region.append_block(Block::new(&[]));
    let end_block = region.append_block(Block::new(&[]));

    // offset < calldata_size =>  offset_ok
    let offset_ok = ok_block
        .append_operation(arith::cmpi(
            context,
            arith::CmpiPredicate::Ult,
            offset,
            calldata_size,
            location,
        ))
        .result(0)?
        .into();

    // if offset < calldata_size => offset_ok_block
    // else => offset_bad_block
    ok_block.append_operation(cf::cond_br(
        context,
        offset_ok,
        &offset_ok_block,
        &offset_bad_block,
        &[],
        &[],
        location,
    ));

    /******************** offset_bad_block *******************/

    // offset >= calldata_size => push 0
    stack_push(context, &offset_bad_block, zero)?;
    offset_bad_block.append_operation(cf::br(&end_block, &[], location));

    /******************** offset_bad_block *******************/

    /******************** offset_OK_block *******************/

    let stack_ptr = get_stack_pointer(context, &offset_ok_block)?;

    // fill the top of the stack with 0s to remove any garbage bytes it could have
    offset_ok_block.append_operation(llvm::store(
        context,
        zero,
        stack_ptr,
        location,
        LoadStoreOptions::new(),
    ));

    // calldata_ptr_at_offset = calldata_ptr + new_offset
    let calldata_ptr_at_offset = offset_ok_block
        .append_operation(llvm::get_element_ptr_dynamic(
            context,
            calldata_ptr,
            &[offset],
            uint8.into(),
            ptr_type,
            location,
        ))
        .result(0)?
        .into();

    // len is the length of the slice (len is maximum 32 bytes)
    let len = offset_ok_block
        .append_operation(arith::subi(calldata_size, offset, location))
        .result(0)?
        .into();

    // len = min(calldata_size - offset, 32 bytes)
    // this is done to fix the len so that  offset + len <= calldata_size
    let len = offset_ok_block
        .append_operation(arith::minui(len, max_slice_width, location))
        .result(0)?
        .into();

    // copy calldata[offset..offset + len] to the top of the stack
    offset_ok_block.append_operation(
        ods::llvm::intr_memcpy(
            context,
            stack_ptr,
            calldata_ptr_at_offset,
            len,
            IntegerAttribute::new(uint1.into(), 0),
            location,
        )
        .into(),
    );

    // increment the stack pointer so calldata[offset..len] is placed at the top of the stack
    inc_stack_pointer(context, &offset_ok_block)?;

    // if the system is little endian, we have to convert the result to big endian
    // pop calldata_slice, change to big endian and push it again
    if cfg!(target_endian = "little") {
        // pop the slice
        let calldata_slice = stack_pop(context, &offset_ok_block)?;
        // convert it to big endian
        let calldata_slice = offset_ok_block
            .append_operation(llvm::intr_bswap(calldata_slice, uint256.into(), location))
            .result(0)?
            .into();
        // push it back on the stack
        stack_push(context, &offset_ok_block, calldata_slice)?;
    }

    offset_ok_block.append_operation(cf::br(&end_block, &[], location));

    /******************** offset_OK_block *******************/

    Ok((start_block, end_block))
}

fn codegen_log<'c, 'r>(
    op_ctx: &mut OperationCtx<'c>,
    region: &'r Region<'c>,
    nth: u8,
) -> Result<(BlockRef<'c, 'r>, BlockRef<'c, 'r>), CodegenError> {
    debug_assert!(nth <= 4);
    // TODO: check if the current execution context is from a STATICCALL (since Byzantium fork).
    let start_block = region.append_block(Block::new(&[]));
    let context = &op_ctx.mlir_context;
    let location = Location::unknown(context);
    let uint32 = IntegerType::new(context, 32);
    let required_elements = 2 + nth;
    // Check there's enough elements in stack
    let flag = check_stack_has_at_least(context, &start_block, required_elements.into())?;

    let ok_block = region.append_block(Block::new(&[]));

    start_block.append_operation(cf::cond_br(
        context,
        flag,
        &ok_block,
        &op_ctx.revert_block,
        &[],
        &[],
        location,
    ));

    let offset_u256 = stack_pop(context, &ok_block)?;
    let size_u256 = stack_pop(context, &ok_block)?;

    let offset = ok_block
        .append_operation(arith::trunci(offset_u256, uint32.into(), location))
        .result(0)?
        .into();
    let size = ok_block
        .append_operation(arith::trunci(size_u256, uint32.into(), location))
        .result(0)?
        .into();

    // required_size = offset + value_size
    let required_size = ok_block
        .append_operation(arith::addi(offset, size, location))
        .result(0)?
        .into();

    let log_block = region.append_block(Block::new(&[]));
    let dynamic_gas = compute_log_dynamic_gas(op_ctx, &ok_block, nth, size_u256, location)?;
    consume_gas_as_value(context, &ok_block, dynamic_gas)?;
    extend_memory(
        op_ctx,
        &ok_block,
        &log_block,
        region,
        required_size,
        gas_cost::LOG,
    )?;

    let mut topic_pointers = vec![];
    for _i in 0..nth {
        let topic = stack_pop(context, &log_block)?;
        let topic_ptr = allocate_and_store_value(op_ctx, &log_block, topic, location)?;
        topic_pointers.push(topic_ptr);
    }

    match nth {
        0 => {
            op_ctx.append_log_syscall(&log_block, offset, size, location);
        }
        1 => {
            op_ctx.append_log_with_one_topic_syscall(
                &log_block,
                offset,
                size,
                topic_pointers[0],
                location,
            );
        }
        2 => {
            op_ctx.append_log_with_two_topics_syscall(
                &log_block,
                offset,
                size,
                topic_pointers[0],
                topic_pointers[1],
                location,
            );
        }
        3 => {
            op_ctx.append_log_with_three_topics_syscall(
                &log_block,
                offset,
                size,
                topic_pointers[0],
                topic_pointers[1],
                topic_pointers[2],
                location,
            );
        }
        4 => {
            op_ctx.append_log_with_four_topics_syscall(
                &log_block,
                offset,
                size,
                topic_pointers[0],
                topic_pointers[1],
                topic_pointers[2],
                topic_pointers[3],
                location,
            );
        }
        _ => unreachable!("nth should satisfy 0 <= nth <= 4"),
    }

    Ok((start_block, log_block))
}
