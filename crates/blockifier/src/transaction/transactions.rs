use std::sync::Arc;

use starknet_api::core::{ClassHash, CompiledClassHash, ContractAddress, Nonce};
use starknet_api::deprecated_contract_class::EntryPointType;
use starknet_api::transaction::{
    AccountDeploymentData, Calldata, ContractAddressSalt, DeclareTransactionV2,
    DeclareTransactionV3, Fee, TransactionHash, TransactionSignature, TransactionVersion,
};

use crate::abi::abi_utils::selector_from_name;
use crate::block_context::BlockContext;
use crate::execution::call_info::CallInfo;
use crate::execution::contract_class::ContractClass;
use crate::execution::entry_point::{
    CallEntryPoint, CallType, ConstructorContext, EntryPointExecutionContext, ExecutionResources,
};
use crate::execution::execution_utils::execute_deployment;
use crate::state::cached_state::{CachedState, TransactionalState};
use crate::state::errors::StateError;
use crate::state::state_api::{State, StateReader};
use crate::transaction::constants;
use crate::transaction::errors::TransactionExecutionError;
use crate::transaction::objects::{
    AccountTransactionContext, CommonAccountFields, CurrentAccountTransactionContext,
    DeprecatedAccountTransactionContext, HasRelatedFeeType, TransactionExecutionInfo,
    TransactionExecutionResult,
};
use crate::transaction::transaction_utils::{update_remaining_gas, verify_contract_class_version};

#[cfg(test)]
#[path = "transactions_test.rs"]
mod test;

macro_rules! implement_inner_tx_getter_calls {
    ($(($field:ident, $field_type:ty)),*) => {
        $(pub fn $field(&self) -> $field_type {
            self.tx.$field().clone()
        })*
    };
}

pub trait ExecutableTransaction<S: StateReader>: Sized {
    /// Executes the transaction in a transactional manner
    /// (if it fails, given state does not modify).
    fn execute(
        self,
        state: &mut CachedState<S>,
        block_context: &BlockContext,
        charge_fee: bool,
        validate: bool,
    ) -> TransactionExecutionResult<TransactionExecutionInfo> {
        log::debug!("Executing Transaction...");
        let mut transactional_state = CachedState::create_transactional(state);
        let execution_result =
            self.execute_raw(&mut transactional_state, block_context, charge_fee, validate);

        match execution_result {
            Ok(value) => {
                transactional_state.commit();
                log::debug!("Transaction execution complete and committed.");
                Ok(value)
            }
            Err(error) => {
                log::debug!("Transaction execution failed with: {error}");
                transactional_state.abort();
                Err(error)
            }
        }
    }

    /// Executes the transaction in a transactional manner
    /// (if it fails, given state might become corrupted; i.e., changes until failure will appear).
    fn execute_raw(
        self,
        state: &mut TransactionalState<'_, S>,
        block_context: &BlockContext,
        charge_fee: bool,
        validate: bool,
    ) -> TransactionExecutionResult<TransactionExecutionInfo>;
}

pub trait Executable<S: State> {
    fn run_execute(
        &self,
        state: &mut S,
        resources: &mut ExecutionResources,
        context: &mut EntryPointExecutionContext,
        remaining_gas: &mut u64,
    ) -> TransactionExecutionResult<Option<CallInfo>>;
}

/// Intended for use in sequencer pre-execution flows, like in a gateway service.
pub trait ValidatableTransaction {
    fn validate_tx(
        &self,
        state: &mut dyn State,
        resources: &mut ExecutionResources,
        account_tx_context: &AccountTransactionContext,
        remaining_gas: &mut u64,
        block_context: &BlockContext,
        limit_steps_by_resources: bool,
    ) -> TransactionExecutionResult<Option<CallInfo>>;
}

#[derive(Debug)]
pub struct DeclareTransaction {
    tx: starknet_api::transaction::DeclareTransaction,
    tx_hash: TransactionHash,
    contract_class: ContractClass,
    // Indicates the presence of the only_query bit in the version.
    only_query: bool,
}

impl DeclareTransaction {
    fn create(
        declare_tx: starknet_api::transaction::DeclareTransaction,
        tx_hash: TransactionHash,
        contract_class: ContractClass,
        only_query: bool,
    ) -> TransactionExecutionResult<Self> {
        let declare_version = declare_tx.version();
        let contract_class = verify_contract_class_version(contract_class, declare_version)?;
        Ok(Self { tx: declare_tx, tx_hash, contract_class, only_query })
    }

    pub fn new(
        declare_tx: starknet_api::transaction::DeclareTransaction,
        tx_hash: TransactionHash,
        contract_class: ContractClass,
    ) -> TransactionExecutionResult<Self> {
        Self::create(declare_tx, tx_hash, contract_class, false)
    }

    pub fn new_for_query(
        declare_tx: starknet_api::transaction::DeclareTransaction,
        tx_hash: TransactionHash,
        contract_class: ContractClass,
    ) -> TransactionExecutionResult<Self> {
        Self::create(declare_tx, tx_hash, contract_class, true)
    }

    implement_inner_tx_getter_calls!((class_hash, ClassHash));

    pub fn tx(&self) -> &starknet_api::transaction::DeclareTransaction {
        &self.tx
    }

    pub fn tx_hash(&self) -> TransactionHash {
        self.tx_hash
    }

    pub fn contract_class(&self) -> ContractClass {
        self.contract_class.clone()
    }

    pub fn get_account_tx_context(&self) -> AccountTransactionContext {
        // TODO(Nir, 01/11/2023): Consider to move this (from all get_account_tx_context methods).
        let common_fields = CommonAccountFields {
            transaction_hash: self.tx_hash(),
            version: self.tx.version(),
            signature: self.tx.signature(),
            nonce: self.tx.nonce(),
            sender_address: self.tx.sender_address(),
            only_query: self.only_query,
        };

        match &self.tx {
            starknet_api::transaction::DeclareTransaction::V0(tx)
            | starknet_api::transaction::DeclareTransaction::V1(tx) => {
                AccountTransactionContext::Deprecated(DeprecatedAccountTransactionContext {
                    common_fields,
                    max_fee: tx.max_fee,
                })
            }
            starknet_api::transaction::DeclareTransaction::V2(tx) => {
                AccountTransactionContext::Deprecated(DeprecatedAccountTransactionContext {
                    common_fields,
                    max_fee: tx.max_fee,
                })
            }
            starknet_api::transaction::DeclareTransaction::V3(tx) => {
                AccountTransactionContext::Current(CurrentAccountTransactionContext {
                    common_fields,
                    resource_bounds: tx.resource_bounds.clone(),
                    tip: tx.tip,
                    nonce_data_availability_mode: tx.nonce_data_availability_mode,
                    fee_data_availability_mode: tx.fee_data_availability_mode,
                    paymaster_data: tx.paymaster_data.clone(),
                    account_deployment_data: tx.account_deployment_data.clone(),
                })
            }
        }
    }

    pub fn only_query(&self) -> bool {
        self.only_query
    }
}

impl<S: State> Executable<S> for DeclareTransaction {
    fn run_execute(
        &self,
        state: &mut S,
        _resources: &mut ExecutionResources,
        _context: &mut EntryPointExecutionContext,
        _remaining_gas: &mut u64,
    ) -> TransactionExecutionResult<Option<CallInfo>> {
        let class_hash = self.class_hash();

        match &self.tx {
            // No class commitment, so no need to check if the class is already declared.
            starknet_api::transaction::DeclareTransaction::V0(_)
            | starknet_api::transaction::DeclareTransaction::V1(_) => {
                state.set_contract_class(class_hash, self.contract_class.clone())?;
                state.set_compiled_class_hash(class_hash, CompiledClassHash(class_hash.0))?;
                Ok(None)
            }
            starknet_api::transaction::DeclareTransaction::V2(DeclareTransactionV2 {
                compiled_class_hash,
                ..
            })
            | starknet_api::transaction::DeclareTransaction::V3(DeclareTransactionV3 {
                compiled_class_hash,
                ..
            }) => {
                match state.get_compiled_contract_class(class_hash) {
                    Err(StateError::UndeclaredClassHash(_)) => {
                        // Class is undeclared; declare it.
                        state.set_contract_class(class_hash, self.contract_class.clone())?;
                        state.set_compiled_class_hash(class_hash, *compiled_class_hash)?;
                        Ok(None)
                    }
                    Err(error) => Err(error)?,
                    Ok(_) => {
                        // Class is already declared, cannot redeclare
                        // (i.e., make sure the leaf is uninitialized).
                        Err(TransactionExecutionError::DeclareTransactionError { class_hash })
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeployAccountTransaction {
    pub tx: starknet_api::transaction::DeployAccountTransaction,
    pub tx_hash: TransactionHash,
    pub contract_address: ContractAddress,
    // Indicates the presence of the only_query bit in the version.
    pub only_query: bool,
}

impl DeployAccountTransaction {
    pub fn new(
        deploy_account_tx: starknet_api::transaction::DeployAccountTransaction,
        tx_hash: TransactionHash,
        contract_address: ContractAddress,
    ) -> Self {
        Self { tx: deploy_account_tx, tx_hash, contract_address, only_query: false }
    }

    pub fn new_for_query(
        deploy_account_tx: starknet_api::transaction::DeployAccountTransaction,
        tx_hash: TransactionHash,
        contract_address: ContractAddress,
    ) -> Self {
        Self { tx: deploy_account_tx, tx_hash, contract_address, only_query: true }
    }

    implement_inner_tx_getter_calls!(
        (class_hash, ClassHash),
        (constructor_calldata, Calldata),
        (contract_address_salt, ContractAddressSalt),
        (nonce, Nonce),
        (signature, TransactionSignature)
    );

    pub fn tx(&self) -> &starknet_api::transaction::DeployAccountTransaction {
        &self.tx
    }

    pub fn get_account_tx_context(&self) -> AccountTransactionContext {
        let common_fields = CommonAccountFields {
            transaction_hash: self.tx_hash,
            version: self.tx.version(),
            signature: self.tx.signature(),
            nonce: self.tx.nonce(),
            sender_address: self.contract_address,
            only_query: self.only_query,
        };

        match &self.tx {
            starknet_api::transaction::DeployAccountTransaction::V1(tx) => {
                AccountTransactionContext::Deprecated(DeprecatedAccountTransactionContext {
                    common_fields,
                    max_fee: tx.max_fee,
                })
            }
            starknet_api::transaction::DeployAccountTransaction::V3(tx) => {
                AccountTransactionContext::Current(CurrentAccountTransactionContext {
                    common_fields,
                    resource_bounds: tx.resource_bounds.clone(),
                    tip: tx.tip,
                    nonce_data_availability_mode: tx.nonce_data_availability_mode,
                    fee_data_availability_mode: tx.fee_data_availability_mode,
                    paymaster_data: tx.paymaster_data.clone(),
                    account_deployment_data: AccountDeploymentData::default(),
                })
            }
        }
    }
}

impl<S: State> Executable<S> for DeployAccountTransaction {
    fn run_execute(
        &self,
        state: &mut S,
        resources: &mut ExecutionResources,
        context: &mut EntryPointExecutionContext,
        remaining_gas: &mut u64,
    ) -> TransactionExecutionResult<Option<CallInfo>> {
        let ctor_context = ConstructorContext {
            class_hash: self.class_hash(),
            code_address: None,
            storage_address: self.contract_address,
            caller_address: ContractAddress::default(),
        };
        let deployment_result = execute_deployment(
            state,
            resources,
            context,
            ctor_context,
            self.constructor_calldata(),
            *remaining_gas,
        );
        let call_info = deployment_result
            .map_err(TransactionExecutionError::ContractConstructorExecutionFailed)?;
        update_remaining_gas(remaining_gas, &call_info);

        Ok(Some(call_info))
    }
}

#[derive(Debug, Clone)]
pub struct InvokeTransaction {
    pub tx: starknet_api::transaction::InvokeTransaction,
    pub tx_hash: TransactionHash,
    // Indicates the presence of the only_query bit in the version.
    pub only_query: bool,
}

impl InvokeTransaction {
    pub fn new(
        invoke_tx: starknet_api::transaction::InvokeTransaction,
        tx_hash: TransactionHash,
    ) -> Self {
        Self { tx: invoke_tx, tx_hash, only_query: false }
    }

    pub fn new_for_query(
        invoke_tx: starknet_api::transaction::InvokeTransaction,
        tx_hash: TransactionHash,
    ) -> Self {
        Self { tx: invoke_tx, tx_hash, only_query: true }
    }

    implement_inner_tx_getter_calls!(
        (calldata, Calldata),
        (signature, TransactionSignature),
        (sender_address, ContractAddress)
    );

    pub fn get_account_tx_context(&self) -> AccountTransactionContext {
        let common_fields = CommonAccountFields {
            transaction_hash: self.tx_hash,
            version: self.tx.version(),
            signature: self.tx.signature(),
            nonce: self.tx.nonce(),
            sender_address: self.tx.sender_address(),
            only_query: self.only_query,
        };

        match &self.tx {
            starknet_api::transaction::InvokeTransaction::V0(tx) => {
                AccountTransactionContext::Deprecated(DeprecatedAccountTransactionContext {
                    common_fields,
                    max_fee: tx.max_fee,
                })
            }
            starknet_api::transaction::InvokeTransaction::V1(tx) => {
                AccountTransactionContext::Deprecated(DeprecatedAccountTransactionContext {
                    common_fields,
                    max_fee: tx.max_fee,
                })
            }
            starknet_api::transaction::InvokeTransaction::V3(tx) => {
                AccountTransactionContext::Current(CurrentAccountTransactionContext {
                    common_fields,
                    resource_bounds: tx.resource_bounds.clone(),
                    tip: tx.tip,
                    nonce_data_availability_mode: tx.nonce_data_availability_mode,
                    fee_data_availability_mode: tx.fee_data_availability_mode,
                    paymaster_data: tx.paymaster_data.clone(),
                    account_deployment_data: tx.account_deployment_data.clone(),
                })
            }
        }
    }
}

impl<S: State> Executable<S> for InvokeTransaction {
    fn run_execute(
        &self,
        state: &mut S,
        resources: &mut ExecutionResources,
        context: &mut EntryPointExecutionContext,
        remaining_gas: &mut u64,
    ) -> TransactionExecutionResult<Option<CallInfo>> {
        let entry_point_selector = match &self.tx {
            starknet_api::transaction::InvokeTransaction::V0(tx) => tx.entry_point_selector,
            starknet_api::transaction::InvokeTransaction::V1(_)
            | starknet_api::transaction::InvokeTransaction::V3(_) => {
                selector_from_name(constants::EXECUTE_ENTRY_POINT_NAME)
            }
        };
        let storage_address = context.account_tx_context.sender_address();
        let execute_call = CallEntryPoint {
            entry_point_type: EntryPointType::External,
            entry_point_selector,
            calldata: self.calldata(),
            class_hash: None,
            code_address: None,
            storage_address,
            caller_address: ContractAddress::default(),
            call_type: CallType::Call,
            initial_gas: *remaining_gas,
        };

        let call_info = execute_call
            .execute(state, resources, context)
            .map_err(TransactionExecutionError::ExecutionError)?;
        update_remaining_gas(remaining_gas, &call_info);

        Ok(Some(call_info))
    }
}

#[derive(Debug)]
pub struct L1HandlerTransaction {
    pub tx: starknet_api::transaction::L1HandlerTransaction,
    pub tx_hash: TransactionHash,
    pub paid_fee_on_l1: Fee,
}

impl L1HandlerTransaction {
    pub fn get_account_tx_context(&self) -> AccountTransactionContext {
        AccountTransactionContext::Deprecated(DeprecatedAccountTransactionContext {
            common_fields: CommonAccountFields {
                transaction_hash: self.tx_hash,
                version: self.tx.version,
                signature: TransactionSignature::default(),
                nonce: self.tx.nonce,
                sender_address: self.tx.contract_address,
                only_query: false,
            },
            max_fee: Fee::default(),
        })
    }
}

impl HasRelatedFeeType for L1HandlerTransaction {
    fn version(&self) -> TransactionVersion {
        self.tx.version
    }

    fn is_l1_handler(&self) -> bool {
        true
    }
}

impl<S: State> Executable<S> for L1HandlerTransaction {
    fn run_execute(
        &self,
        state: &mut S,
        resources: &mut ExecutionResources,
        context: &mut EntryPointExecutionContext,
        remaining_gas: &mut u64,
    ) -> TransactionExecutionResult<Option<CallInfo>> {
        let tx = &self.tx;
        let storage_address = tx.contract_address;
        let execute_call = CallEntryPoint {
            entry_point_type: EntryPointType::L1Handler,
            entry_point_selector: tx.entry_point_selector,
            calldata: Calldata(Arc::clone(&tx.calldata.0)),
            class_hash: None,
            code_address: None,
            storage_address,
            caller_address: ContractAddress::default(),
            call_type: CallType::Call,
            initial_gas: *remaining_gas,
        };

        execute_call
            .execute(state, resources, context)
            .map(Some)
            .map_err(TransactionExecutionError::ExecutionError)
    }
}
