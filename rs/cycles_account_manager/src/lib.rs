//! This module contains the `CyclesAccountManager` which is responsible for
//! updating the cycles account of canisters.
//!
//! A canister has an associated cycles balance, and may `send` a part of
//! this cycles balance to another canister
//! In addition to sending cycles to another canister, a canister `spend`s
//! cycles in the following three ways:
//! a) executing messages,
//! b) sending messages to other canisters,
//! c) storing data over time/rounds
//! Each of the above spending is done in three phases:
//! 1. reserving maximum cycles the operation can require
//! 2. executing the operation and return `cycles_spent`
//! 3. reimburse the canister with `cycles_reserved` - `cycles_spent`

use ic_base_types::NumSeconds;
use ic_config::subnet_config::CyclesAccountManagerConfig;
use ic_ic00_types::Method;
use ic_interfaces::execution_environment::CanisterOutOfCyclesError;
use ic_logger::{error, info, ReplicaLogger};
use ic_nns_constants::CYCLES_MINTING_CANISTER_ID;
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::{
    canister_state::system_state::CyclesUseCase, CanisterState, SystemState,
};
use ic_types::{
    canister_http::MAX_CANISTER_HTTP_RESPONSE_BYTES,
    messages::{Request, Response, SignedIngressContent, MAX_INTER_CANISTER_PAYLOAD_IN_BYTES},
    CanisterId, ComputeAllocation, Cycles, MemoryAllocation, NumBytes, NumInstructions, SubnetId,
};
use prometheus::IntCounter;
use serde::{Deserialize, Serialize};
use std::{str::FromStr, time::Duration};

pub const CRITICAL_ERROR_RESPONSE_CYCLES_REFUND: &str =
    "cycles_account_manager_response_cycles_refund_error";

pub const CRITICAL_ERROR_EXECUTION_CYCLES_REFUND: &str =
    "cycles_account_manager_execution_cycles_refund_error";

/// [EXC-1168] Flag to turn on cost scaling according to a subnet replication factor.
const USE_COST_SCALING_FLAG: bool = true;

/// Errors returned by the [`CyclesAccountManager`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CyclesAccountManagerError {
    /// One of the API contracts that the cycles account manager enforces was
    /// violated.
    ContractViolation(String),
}

impl std::error::Error for CyclesAccountManagerError {}

impl std::fmt::Display for CyclesAccountManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CyclesAccountManagerError::ContractViolation(msg) => {
                write!(f, "Contract violation: {}", msg)
            }
        }
    }
}

/// Measures how much a resource such as compute or storage is being used.
/// It will be used in resource reservation to scale reservation parameters
/// depending on the resource usage.
///
/// The default implementation corresponds to a no-op (empty) resource
/// saturation with `threshold = capacity = 0`.
///
/// This struct maintains an invariant that `usage <= capacity` and
/// `threshold <= capacity`.  There are no constraints between `usage` and
/// `threshold`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ResourceSaturation {
    usage: u64,
    threshold: u64,
    capacity: u64,
}

impl ResourceSaturation {
    /// Creates a new `ResourceSaturation` based on the given resource usages,
    /// threshold, and capacity. All arguments have the same unit that depends
    /// on the concrete resource:
    ///    - The unit of compute is percents.
    ///    - The unit of storage is bytes.
    ///
    /// See the comment of the `scale()` function for explanation of how the
    /// arguments are used.
    pub fn new(usage: u64, threshold: u64, capacity: u64) -> Self {
        let usage = usage.min(capacity);
        let threshold = threshold.min(capacity);
        Self {
            usage,
            threshold,
            capacity,
        }
    }

    /// Creates a new `ResourceSaturation` like the `new()` constructor, but also
    /// divides `usage`, `threshold`, and `capacity` by the given `scaling` factor.
    pub fn new_scaled(usage: u64, threshold: u64, capacity: u64, scaling: u64) -> Self {
        Self::new(usage / scaling, threshold / scaling, capacity / scaling)
    }

    /// Returns the part of the usage that is above the threshold.
    pub fn usage_above_threshold(&self) -> u64 {
        self.usage.saturating_sub(self.threshold)
    }

    /// Scales the given value proportionally to the resource saturation.
    /// More specifically, the value is scaled by `(U - T) / (C - T)`,
    /// where
    /// - `U` is the usage.
    /// - `T` is the threshold.
    /// - `C` is the capacity.
    ///
    /// The function returns `0` if `C == T`.
    ///
    /// Note that the invariant of this struct guarantees that `U <= C`,
    /// so the result of this function does not exceed the input value.
    pub fn reservation_factor(&self, value: u64) -> u64 {
        let capacity = self.capacity.saturating_sub(self.threshold);
        let usage = self.usage.saturating_sub(self.threshold);
        if capacity == 0 {
            0
        } else {
            let result = (value as u128 * usage as u128) / capacity as u128;
            // We know that the result fits in 64 bits because `value` fits in
            // 64 bits and `usage / capacity <= 1`.
            result.try_into().unwrap()
        }
    }

    /// Returns a new `ResourceSaturation` with the additional usage.
    pub fn add(&self, usage: u64) -> Self {
        Self {
            usage: (self.usage + usage).min(self.capacity),
            threshold: self.threshold,
            capacity: self.capacity,
        }
    }
}

/// Handles any operation related to cycles accounting, such as charging (due to
/// using system resources) or refunding unused cycles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CyclesAccountManager {
    /// The maximum allowed instructions to be spent on a single message
    /// execution.
    max_num_instructions: NumInstructions,

    /// The subnet type of this [`CyclesAccountManager`].
    own_subnet_type: SubnetType,

    /// The subnet id of this [`CyclesAccountManager`].
    own_subnet_id: SubnetId,

    /// The configuration of this [`CyclesAccountManager`] controlling the fees
    /// that are charged for various operations.
    config: CyclesAccountManagerConfig,

    /// [EXC-1168] Temporary development flag to enable cost scaling according to subnet size.
    use_cost_scaling_flag: bool,
}

impl CyclesAccountManager {
    pub fn new(
        // Note: `max_num_instructions` is passed from a different config.
        // Config.
        max_num_instructions: NumInstructions,
        own_subnet_type: SubnetType,
        own_subnet_id: SubnetId,
        config: CyclesAccountManagerConfig,
    ) -> Self {
        Self {
            max_num_instructions,
            own_subnet_type,
            own_subnet_id,
            config,
            use_cost_scaling_flag: USE_COST_SCALING_FLAG,
        }
    }

    /// [EXC-1168] Helper function to set the flag to enable cost scaling according to subnet size.
    pub fn set_using_cost_scaling(&mut self, use_cost_scaling_flag: bool) {
        self.use_cost_scaling_flag = use_cost_scaling_flag;
    }

    /// [EXC-1168] Helper function to read the flag to enable cost scaling according to subnet size.
    pub fn use_cost_scaling(&self) -> bool {
        self.use_cost_scaling_flag
    }

    /// Returns the subnet type of this [`CyclesAccountManager`].
    pub fn subnet_type(&self) -> SubnetType {
        self.own_subnet_type
    }

    /// Returns the Subnet Id of this [`CyclesAccountManager`].
    pub fn get_subnet_id(&self) -> SubnetId {
        self.own_subnet_id
    }

    // Scale cycles cost according to a subnet size.
    fn scale_cost(&self, cycles: Cycles, subnet_size: usize) -> Cycles {
        match self.use_cost_scaling_flag {
            false => cycles,
            true => (cycles * subnet_size) / self.config.reference_subnet_size,
        }
    }

    ////////////////////////////////////////////////////////////////////////////
    //
    // Execution/Computation
    //
    ////////////////////////////////////////////////////////////////////////////

    /// Returns the fee to create a canister in [`Cycles`].
    pub fn canister_creation_fee(&self, subnet_size: usize) -> Cycles {
        self.scale_cost(self.config.canister_creation_fee, subnet_size)
    }

    /// Returns the fee for receiving an ingress message in [`Cycles`].
    pub fn ingress_message_received_fee(&self, subnet_size: usize) -> Cycles {
        self.scale_cost(self.config.ingress_message_reception_fee, subnet_size)
    }

    /// Returns the fee for storing a GiB of data per second scaled by subnet size.
    pub fn gib_storage_per_second_fee(&self, subnet_size: usize) -> Cycles {
        self.scale_cost(self.config.gib_storage_per_second_fee, subnet_size)
    }

    /// Returns the fee per byte of ingress message received in [`Cycles`].
    pub fn ingress_byte_received_fee(&self, subnet_size: usize) -> Cycles {
        self.scale_cost(self.config.ingress_byte_reception_fee, subnet_size)
    }

    /// Returns the fee for performing a xnet call in [`Cycles`].
    pub fn xnet_call_performed_fee(&self, subnet_size: usize) -> Cycles {
        self.scale_cost(self.config.xnet_call_fee, subnet_size)
    }

    /// Returns the fee per byte of transmitted xnet call in [`Cycles`].
    pub fn xnet_call_bytes_transmitted_fee(
        &self,
        payload_size: NumBytes,
        subnet_size: usize,
    ) -> Cycles {
        self.scale_cost(
            self.config.xnet_byte_transmission_fee * payload_size.get(),
            subnet_size,
        )
    }

    // Returns the idle resource consumption rate in cycles per day.
    pub fn idle_cycles_burned_rate(
        &self,
        memory_allocation: MemoryAllocation,
        memory_usage: NumBytes,
        compute_allocation: ComputeAllocation,
        subnet_size: usize,
    ) -> Cycles {
        let memory = match memory_allocation {
            MemoryAllocation::Reserved(bytes) => bytes,
            MemoryAllocation::BestEffort => memory_usage,
        };
        let day = Duration::from_secs(24 * 60 * 60);
        self.memory_cost(memory, day, subnet_size)
            + self.compute_allocation_cost(compute_allocation, day, subnet_size)
    }

    /// Returns the freezing threshold for this canister in cycles after
    /// taking the reserved balance into account.
    pub fn freeze_threshold_cycles(
        &self,
        freeze_threshold: NumSeconds,
        memory_allocation: MemoryAllocation,
        memory_usage: NumBytes,
        compute_allocation: ComputeAllocation,
        subnet_size: usize,
        reserved_balance: Cycles,
    ) -> Cycles {
        let idle_cycles_burned_rate: u128 = self
            .idle_cycles_burned_rate(
                memory_allocation,
                memory_usage,
                compute_allocation,
                subnet_size,
            )
            .get();
        let seconds_per_day = 24 * 60 * 60;

        let threshold = Cycles::from(
            idle_cycles_burned_rate * freeze_threshold.get() as u128 / seconds_per_day,
        );

        // Here we rely on the saturating subtraction for Cycles.
        threshold - reserved_balance
    }

    /// Withdraws `cycles` worth of cycles from the canister's balance.
    ///
    /// NOTE: This method is intended for use in inter-canister transfers.
    ///       It doesn't report these cycles as consumed. To withdraw cycles
    ///       and have them reported as consumed, use `consume_cycles`.
    ///
    /// # Errors
    ///
    /// Returns a `CanisterOutOfCyclesError` if the
    /// requested amount is greater than the currently available.
    #[allow(clippy::too_many_arguments)]
    pub fn withdraw_cycles_for_transfer(
        &self,
        canister_id: CanisterId,
        freeze_threshold: NumSeconds,
        memory_allocation: MemoryAllocation,
        canister_current_memory_usage: NumBytes,
        canister_compute_allocation: ComputeAllocation,
        cycles_balance: &mut Cycles,
        cycles: Cycles,
        subnet_size: usize,
        reserved_balance: Cycles,
    ) -> Result<(), CanisterOutOfCyclesError> {
        self.withdraw_with_threshold(
            canister_id,
            cycles_balance,
            cycles,
            self.freeze_threshold_cycles(
                freeze_threshold,
                memory_allocation,
                canister_current_memory_usage,
                canister_compute_allocation,
                subnet_size,
                reserved_balance,
            ),
        )
    }

    /// Charges the canister for ingress induction cost.
    ///
    /// Note that this method reports the cycles withdrawn as consumed (i.e.
    /// burnt).
    ///
    /// # Errors
    ///
    /// Returns a `CanisterOutOfCyclesError` if the
    /// requested amount is greater than the currently available.
    pub fn charge_ingress_induction_cost(
        &self,
        canister: &mut CanisterState,
        canister_current_memory_usage: NumBytes,
        canister_compute_allocation: ComputeAllocation,
        cycles: Cycles,
        subnet_size: usize,
    ) -> Result<(), CanisterOutOfCyclesError> {
        let threshold = self.freeze_threshold_cycles(
            canister.system_state.freeze_threshold,
            canister.system_state.memory_allocation,
            canister_current_memory_usage,
            canister_compute_allocation,
            subnet_size,
            canister.system_state.reserved_balance(),
        );
        if canister.has_paused_execution() || canister.has_paused_install_code() {
            if canister.system_state.debited_balance() < cycles + threshold {
                return Err(CanisterOutOfCyclesError {
                    canister_id: canister.canister_id(),
                    available: canister.system_state.debited_balance(),
                    requested: cycles,
                    threshold,
                });
            }
            canister
                .system_state
                .add_postponed_charge_to_ingress_induction_cycles_debit(cycles);
            Ok(())
        } else {
            self.consume_with_threshold(
                &mut canister.system_state,
                cycles,
                threshold,
                CyclesUseCase::IngressInduction,
            )
        }
    }

    /// Withdraws and consumes cycles from the canister's balance.
    ///
    /// NOTE: This method reports the cycles withdrawn as consumed (i.e. burnt).
    ///       For withdrawals where cycles are not consumed, such as the case
    ///       for inter-canister transfers, use `withdraw_cycles_for_transfer`.
    ///
    /// # Errors
    ///
    /// Returns a `CanisterOutOfCyclesError` if the
    /// requested amount is greater than the currently available.
    pub fn consume_cycles(
        &self,
        system_state: &mut SystemState,
        canister_current_memory_usage: NumBytes,
        canister_compute_allocation: ComputeAllocation,
        cycles: Cycles,
        subnet_size: usize,
        use_case: CyclesUseCase,
    ) -> Result<(), CanisterOutOfCyclesError> {
        let threshold = self.freeze_threshold_cycles(
            system_state.freeze_threshold,
            system_state.memory_allocation,
            canister_current_memory_usage,
            canister_compute_allocation,
            subnet_size,
            system_state.reserved_balance(),
        );
        self.consume_with_threshold(system_state, cycles, threshold, use_case)
    }

    /// Prepays the cost of executing a message with the given number of
    /// instructions. See the comment of `execution_cost()` for details
    /// about the execution cost.
    ///
    /// Returns the prepaid cycles.
    ///
    /// # Errors
    ///
    /// Returns a `CanisterOutOfCyclesError` if there are not enough cycles in
    /// the canister balance above the freezing threshold.
    pub fn prepay_execution_cycles(
        &self,
        system_state: &mut SystemState,
        canister_current_memory_usage: NumBytes,
        canister_compute_allocation: ComputeAllocation,
        num_instructions: NumInstructions,
        subnet_size: usize,
    ) -> Result<Cycles, CanisterOutOfCyclesError> {
        let cost = self.execution_cost(num_instructions, subnet_size);
        self.consume_with_threshold(
            system_state,
            cost,
            self.freeze_threshold_cycles(
                system_state.freeze_threshold,
                system_state.memory_allocation,
                canister_current_memory_usage,
                canister_compute_allocation,
                subnet_size,
                system_state.reserved_balance(),
            ),
            CyclesUseCase::Instructions,
        )
        .map(|_| cost)
    }

    /// Refunds some part of the prepaid execution cost based on the number of
    /// actually executed instructions.
    pub fn refund_unused_execution_cycles(
        &self,
        system_state: &mut SystemState,
        num_instructions: NumInstructions,
        num_instructions_initially_charged: NumInstructions,
        prepaid_execution_cycles: Cycles,
        error_counter: &IntCounter,
        subnet_size: usize,
        log: &ReplicaLogger,
    ) {
        debug_assert!(num_instructions <= num_instructions_initially_charged);
        if num_instructions > num_instructions_initially_charged {
            error_counter.inc();
            error!(
                log,
                "{}: Unexpected amount of executed instructions: {} (max expected {})",
                CRITICAL_ERROR_EXECUTION_CYCLES_REFUND,
                num_instructions,
                num_instructions_initially_charged
            );
        }
        let num_instructions_to_refund =
            std::cmp::min(num_instructions, num_instructions_initially_charged);
        let cycles_to_refund = self
            .scale_cost(
                self.convert_instructions_to_cycles(num_instructions_to_refund),
                subnet_size,
            )
            .min(prepaid_execution_cycles);
        system_state.add_cycles(cycles_to_refund, CyclesUseCase::Instructions);
    }

    /// Charges the canister for its compute allocation
    ///
    /// # Errors
    ///
    /// Returns a `CanisterOutOfCyclesError` if the
    /// requested amount is greater than the currently available.
    pub fn charge_for_compute_allocation(
        &self,
        system_state: &mut SystemState,
        compute_allocation: ComputeAllocation,
        duration: Duration,
        subnet_size: usize,
    ) -> Result<(), CanisterOutOfCyclesError> {
        let cycles = self.compute_allocation_cost(compute_allocation, duration, subnet_size);

        // Can charge all the way to the empty account (zero cycles)
        self.consume_with_threshold(
            system_state,
            cycles,
            Cycles::zero(),
            CyclesUseCase::ComputeAllocation,
        )
    }

    /// The cost of compute allocation, per round
    #[doc(hidden)] // pub for usage in tests
    pub fn compute_allocation_cost(
        &self,
        compute_allocation: ComputeAllocation,
        duration: Duration,
        subnet_size: usize,
    ) -> Cycles {
        let cycles = self.config.compute_percent_allocated_per_second_fee
            * duration.as_secs()
            * compute_allocation.as_percent();
        self.scale_cost(cycles, subnet_size)
    }

    /// Computes the cost of inducting an ingress message.
    ///
    /// Returns a tuple containing:
    ///  - ID of the canister that should pay for the cost.
    ///  - The cost of inducting the message.
    pub fn ingress_induction_cost(
        &self,
        ingress: &SignedIngressContent,
        effective_canister_id: Option<CanisterId>,
        subnet_size: usize,
    ) -> IngressInductionCost {
        let paying_canister = match ingress.is_addressed_to_subnet(self.own_subnet_id) {
            // If a subnet message, get effective canister id who will pay for the message.
            true => {
                if let Ok(Method::UpdateSettings) = Method::from_str(ingress.method_name()) {
                    // The fee for `UpdateSettings` is charged after applying the settings
                    // to allow users to unfreeze canisters after accidentally setting
                    // the freezing threshold too high.
                    None
                } else {
                    effective_canister_id
                }
            }
            // A message to a canister is always paid for by the receiving canister.
            false => Some(ingress.canister_id()),
        };

        match paying_canister {
            Some(paying_canister) => {
                let bytes_to_charge = ingress.arg().len()
                    + ingress.method_name().len()
                    + ingress.nonce().map(|n| n.len()).unwrap_or(0);
                let cost = self.ingress_induction_cost_from_bytes(
                    NumBytes::from(bytes_to_charge as u64),
                    subnet_size,
                );
                IngressInductionCost::Fee {
                    payer: paying_canister,
                    cost,
                }
            }
            None => IngressInductionCost::Free,
        }
    }

    /// Returns the cost of an ingress message based on the message size.
    pub fn ingress_induction_cost_from_bytes(&self, bytes: NumBytes, subnet_size: usize) -> Cycles {
        self.scale_cost(
            self.config.ingress_message_reception_fee
                + self.config.ingress_byte_reception_fee * bytes.get(),
            subnet_size,
        )
    }

    /// How often canisters should be charged for memory and compute allocation.
    pub fn duration_between_allocation_charges(&self) -> Duration {
        self.config.duration_between_allocation_charges
    }

    /// Amount to charge for an ECDSA signature.
    pub fn ecdsa_signature_fee(&self, subnet_size: usize) -> Cycles {
        self.scale_cost(self.config.ecdsa_signature_fee, subnet_size)
    }

    ////////////////////////////////////////////////////////////////////////////
    //
    // Storage
    //
    ////////////////////////////////////////////////////////////////////////////

    /// Subtracts the cycles cost of using a `bytes` amount of memory.
    ///
    /// Note: The following charges for memory taken by the canister. It
    /// currently takes into account all the pages in the canister's heap and
    /// stable memory (among other things). This will be revised in the future
    /// to take into account charging for dirty/read pages by the canister.
    ///
    /// # Errors
    ///
    /// Returns a `CanisterOutOfCyclesError` if there's
    /// not enough cycles to charge for memory.
    pub fn charge_for_memory(
        &self,
        system_state: &mut SystemState,
        bytes: NumBytes,
        duration: Duration,
        subnet_size: usize,
    ) -> Result<(), CanisterOutOfCyclesError> {
        let cycles_amount = self.memory_cost(bytes, duration, subnet_size);

        // Can charge all the way to the empty account (zero cycles)
        self.consume_with_threshold(
            system_state,
            cycles_amount,
            Cycles::zero(),
            CyclesUseCase::Memory,
        )
    }

    /// The cost of using `bytes` worth of memory.
    #[doc(hidden)] // pub for usage in tests
    pub fn memory_cost(&self, bytes: NumBytes, duration: Duration, subnet_size: usize) -> Cycles {
        let one_gib = 1024 * 1024 * 1024;
        let cycles = Cycles::from(
            (bytes.get() as u128
                * self.config.gib_storage_per_second_fee.get()
                * duration.as_secs() as u128)
                / one_gib,
        );
        self.scale_cost(cycles, subnet_size)
    }

    /// Returns the amount of reserved cycles required for allocating the given
    /// number of bytes at the given resource saturation level.
    pub fn storage_reservation_cycles(
        &self,
        allocated_bytes: NumBytes,
        storage_saturation: &ResourceSaturation,
        subnet_size: usize,
    ) -> Cycles {
        // The reservation cycles for `allocated_bytes` can be computed as
        // the difference between
        // - the total reservation cycles from 0 to `usage + allocated_bytes` and
        // - the total reservation cycles from 0 to `usage`.
        self.total_storage_reservation_cycles(
            &storage_saturation.add(allocated_bytes.get()),
            subnet_size,
        ) - self.total_storage_reservation_cycles(storage_saturation, subnet_size)
    }

    /// Returns the total amount of reserved cycles for the given resource
    /// saturation level. In other words, it computes how many cycles would be
    /// reserved for a resource allocation that goes from 0 to the usage
    /// specified in the given resource saturation.
    fn total_storage_reservation_cycles(
        &self,
        storage_saturation: &ResourceSaturation,
        subnet_size: usize,
    ) -> Cycles {
        let duration = Duration::from_secs(
            storage_saturation
                .reservation_factor(self.config.max_storage_reservation_period.as_secs()),
        );
        // We need to compute the area of the triangle with
        // - base: (U - T) = usage_above_threshold(),
        // - height: duration * fee.
        // That is equal to `(base * height) / 2 = base * (height / 2)`.
        self.memory_cost(
            NumBytes::new(storage_saturation.usage_above_threshold()),
            duration / 2,
            subnet_size,
        )
    }

    ////////////////////////////////////////////////////////////////////////////
    //
    // Request
    //
    ////////////////////////////////////////////////////////////////////////////

    /// When sending a request it's necessary to pay for:
    ///   * The network cost of sending the request payload, which depends on
    ///     the size (bytes) of the request.
    ///   * The max cycles `max_num_instructions` that would be required to
    ///     process the `Response`.
    ///   * The max network cost of receiving the response, since we don't know
    ///     yet the exact size the response will have.
    ///
    /// The leftover cycles is reimbursed after the `Response` for this request
    /// is received and executed. Only at that point will be known how much
    /// cycles receiving and executing the `Response` costs exactly.
    ///
    /// # Errors
    ///
    /// Returns a `CanisterOutOfCyclesError` if there is
    /// not enough cycles available to send the `Request`.
    #[allow(clippy::too_many_arguments)]
    pub fn withdraw_request_cycles(
        &self,
        canister_id: CanisterId,
        cycles_balance: &mut Cycles,
        freeze_threshold: NumSeconds,
        memory_allocation: MemoryAllocation,
        canister_current_memory_usage: NumBytes,
        canister_compute_allocation: ComputeAllocation,
        request: &Request,
        prepayment_for_response_execution: Cycles,
        prepayment_for_response_transmission: Cycles,
        subnet_size: usize,
        reserved_balance: Cycles,
    ) -> Result<Vec<(CyclesUseCase, Cycles)>, CanisterOutOfCyclesError> {
        // The total amount charged consists of:
        //   - the fee to do the xnet call (request + response)
        //   - the fee to send the request (by size)
        //   - the fee for the largest possible response
        //   - the fee for executing the largest allowed response when it eventually arrives.
        let transmission_fee = self.scale_cost(
            self.config.xnet_call_fee
                + self.config.xnet_byte_transmission_fee * request.payload_size_bytes().get(),
            subnet_size,
        ) + prepayment_for_response_transmission;

        let fee = transmission_fee + prepayment_for_response_execution;

        self.withdraw_with_threshold(
            canister_id,
            cycles_balance,
            fee,
            self.freeze_threshold_cycles(
                freeze_threshold,
                memory_allocation,
                canister_current_memory_usage,
                canister_compute_allocation,
                subnet_size,
                reserved_balance,
            ),
        )?;

        Ok(Vec::from([
            (
                CyclesUseCase::Instructions,
                prepayment_for_response_execution,
            ),
            (
                CyclesUseCase::RequestAndResponseTransmission,
                transmission_fee,
            ),
        ]))
    }

    /// Returns the amount of cycles required for executing the longest-running
    /// response callback.
    pub fn prepayment_for_response_execution(&self, subnet_size: usize) -> Cycles {
        self.execution_cost(self.max_num_instructions, subnet_size)
    }

    /// Returns the amount of cycles required for transmitting the largest
    /// response message.
    pub fn prepayment_for_response_transmission(&self, subnet_size: usize) -> Cycles {
        self.scale_cost(
            self.config.xnet_byte_transmission_fee * MAX_INTER_CANISTER_PAYLOAD_IN_BYTES.get(),
            subnet_size,
        )
    }

    /// Returns the refund cycles for the response transmission bytes reserved at
    /// the initial call time.
    pub fn refund_for_response_transmission(
        &self,
        log: &ReplicaLogger,
        error_counter: &IntCounter,
        response: &Response,
        prepayment_for_response_transmission: Cycles,
        subnet_size: usize,
    ) -> Cycles {
        let max_expected_bytes = MAX_INTER_CANISTER_PAYLOAD_IN_BYTES.get();
        let transmitted_bytes = response.payload_size_bytes().get();
        debug_assert!(transmitted_bytes <= max_expected_bytes);
        if max_expected_bytes < transmitted_bytes {
            error_counter.inc();
            error!(
                log,
                "{}: Unexpected response payload size of {} bytes (max expected {})",
                CRITICAL_ERROR_RESPONSE_CYCLES_REFUND,
                transmitted_bytes,
                max_expected_bytes,
            );
        }
        let transmission_cost = self.scale_cost(
            self.config.xnet_byte_transmission_fee * transmitted_bytes,
            subnet_size,
        );
        prepayment_for_response_transmission
            - transmission_cost.min(prepayment_for_response_transmission)
    }

    ////////////////////////////////////////////////////////////////////////////
    //
    // Utility functions
    //
    ////////////////////////////////////////////////////////////////////////////

    /// Checks whether the requested amount of cycles can be withdrawn from the
    /// canister's balance while respecting the freezing threshold.
    ///
    /// Returns a `CanisterOutOfCyclesError` if the requested amount cannot be
    /// withdrawn.
    pub fn can_withdraw_cycles(
        &self,
        system_state: &SystemState,
        requested: Cycles,
        canister_current_memory_usage: NumBytes,
        canister_compute_allocation: ComputeAllocation,
        subnet_size: usize,
    ) -> Result<(), CanisterOutOfCyclesError> {
        let threshold = self.freeze_threshold_cycles(
            system_state.freeze_threshold,
            system_state.memory_allocation,
            canister_current_memory_usage,
            canister_compute_allocation,
            subnet_size,
            system_state.reserved_balance(),
        );

        if threshold + requested > system_state.balance() {
            Err(CanisterOutOfCyclesError {
                canister_id: system_state.canister_id(),
                available: system_state.balance(),
                requested,
                threshold,
            })
        } else {
            Ok(())
        }
    }

    /// Subtracts and consumes the cycles. This call should be used when the
    /// cycles are not being sent somewhere else.
    pub fn consume_with_threshold(
        &self,
        system_state: &mut SystemState,
        cycles: Cycles,
        threshold: Cycles,
        use_case: CyclesUseCase,
    ) -> Result<(), CanisterOutOfCyclesError> {
        let effective_cycles_balance = match use_case {
            CyclesUseCase::Memory | CyclesUseCase::ComputeAllocation | CyclesUseCase::Uninstall => {
                // The resource use cases first drain the `reserved_balance` and
                // after that the main balance.
                system_state.balance() + system_state.reserved_balance()
            }
            CyclesUseCase::IngressInduction
            | CyclesUseCase::Instructions
            | CyclesUseCase::RequestAndResponseTransmission
            | CyclesUseCase::CanisterCreation
            | CyclesUseCase::ECDSAOutcalls
            | CyclesUseCase::HTTPOutcalls
            | CyclesUseCase::DeletedCanisters
            | CyclesUseCase::NonConsumed => system_state.balance(),
        };

        self.verify_cycles_balance_with_threshold(
            system_state.canister_id,
            effective_cycles_balance,
            cycles,
            threshold,
        )?;

        debug_assert_ne!(use_case, CyclesUseCase::NonConsumed);
        system_state.remove_cycles(cycles, use_case);
        Ok(())
    }

    fn verify_cycles_balance_with_threshold(
        &self,
        canister_id: CanisterId,
        cycles_balance: Cycles,
        cycles: Cycles,
        threshold: Cycles,
    ) -> Result<(), CanisterOutOfCyclesError> {
        let cycles_available = if cycles_balance > threshold {
            cycles_balance - threshold
        } else {
            Cycles::zero()
        };

        if cycles > cycles_available {
            return Err(CanisterOutOfCyclesError {
                canister_id,
                available: cycles_balance,
                requested: cycles,
                threshold,
            });
        }
        Ok(())
    }

    /// Subtracts `cycles` worth of cycles from the canister's balance as long
    /// as there's enough above the provided `threshold`. This call should be
    /// used when the withdrawn cycles are sent somewhere else.
    ///
    /// # Errors
    ///
    /// Returns a `CanisterOutOfCyclesError` if the
    /// requested amount is greater than the currently available.
    // #[doc(hidden)] // pub for usage in tests
    pub fn withdraw_with_threshold(
        &self,
        canister_id: CanisterId,
        cycles_balance: &mut Cycles,
        cycles: Cycles,
        threshold: Cycles,
    ) -> Result<(), CanisterOutOfCyclesError> {
        self.verify_cycles_balance_with_threshold(canister_id, *cycles_balance, cycles, threshold)?;

        *cycles_balance -= cycles;
        Ok(())
    }

    /// Mints `amount_to_mint` [`Cycles`].
    ///
    /// # Errors
    /// Returns a `CyclesAccountManagerError::ContractViolation` if not on NNS
    /// subnet.
    pub fn mint_cycles(
        &self,
        canister_id: CanisterId,
        cycles_balance: &mut Cycles,
        amount_to_mint: Cycles,
    ) -> Result<(), CyclesAccountManagerError> {
        if canister_id != CYCLES_MINTING_CANISTER_ID {
            let error_str = format!(
                "ic0.mint_cycles cannot be executed on non Cycles Minting Canister: {} != {}",
                canister_id, CYCLES_MINTING_CANISTER_ID
            );
            Err(CyclesAccountManagerError::ContractViolation(error_str))
        } else {
            *cycles_balance += amount_to_mint;
            Ok(())
        }
    }

    /// Converts `num_instructions` in `Cycles`.
    ///
    /// Note that this function is made public to facilitate some logistic in
    /// tests.
    #[doc(hidden)]
    pub fn convert_instructions_to_cycles(&self, num_instructions: NumInstructions) -> Cycles {
        self.config.ten_update_instructions_execution_fee * (num_instructions.get() / 10)
    }

    /// Returns the cost of executing a message with the given number of
    /// instructions. The cost consists of:
    /// - the fixed fee to start executing a message.
    /// - the fee that depends on the number of instructions.
    pub fn execution_cost(&self, num_instructions: NumInstructions, subnet_size: usize) -> Cycles {
        self.scale_cost(
            self.config.update_message_execution_fee
                + self.convert_instructions_to_cycles(num_instructions),
            subnet_size,
        )
    }

    /// Charges a canister for its resource allocation and usage for the
    /// duration specified. If fees were successfully charged, then returns
    /// Ok(CanisterState) else returns Err(CanisterState).
    pub fn charge_canister_for_resource_allocation_and_usage(
        &self,
        log: &ReplicaLogger,
        canister: &mut CanisterState,
        duration_between_blocks: Duration,
        subnet_size: usize,
    ) -> Result<(), CanisterOutOfCyclesError> {
        let canister_memory_bytes_to_charge = match canister.memory_allocation() {
            // The canister has explicitly asked for a memory allocation, so charge
            // based on it accordingly.
            MemoryAllocation::Reserved(bytes) => bytes,
            // The canister uses best-effort memory allocation, so charge based on current usage.
            MemoryAllocation::BestEffort => canister.memory_usage(),
        };
        if let Err(err) = self.charge_for_memory(
            &mut canister.system_state,
            canister_memory_bytes_to_charge,
            duration_between_blocks,
            subnet_size,
        ) {
            info!(
                log,
                "Charging canister {} for memory allocation/usage failed with {}",
                canister.canister_id(),
                err
            );
            return Err(err);
        }

        let message_memory_bytes_to_charge = canister.message_memory_usage();
        if let Err(err) = self.charge_for_memory(
            &mut canister.system_state,
            message_memory_bytes_to_charge,
            duration_between_blocks,
            subnet_size,
        ) {
            info!(
                log,
                "Charging canister {} for message memory usage failed with {}",
                canister.canister_id(),
                err
            );
            return Err(err);
        }

        let compute_allocation = canister.compute_allocation();
        if let Err(err) = self.charge_for_compute_allocation(
            &mut canister.system_state,
            compute_allocation,
            duration_between_blocks,
            subnet_size,
        ) {
            info!(
                log,
                "Charging canister {} for compute allocation failed with {}",
                canister.canister_id(),
                err
            );
            return Err(err);
        }
        Ok(())
    }

    pub fn http_request_fee(
        &self,
        request_size: NumBytes,
        response_size_limit: Option<NumBytes>,
        subnet_size: usize,
    ) -> Cycles {
        let response_size = match response_size_limit {
            Some(response_size) => response_size.get(),
            // Defaults to maximum response size.
            None => MAX_CANISTER_HTTP_RESPONSE_BYTES,
        };

        (self.config.http_request_linear_baseline_fee
            + self.config.http_request_quadratic_baseline_fee * (subnet_size as u64)
            + self.config.http_request_per_byte_fee * request_size.get()
            + self.config.http_response_per_byte_fee * response_size)
            * (subnet_size as u64)
    }

    /// Returns the default value of the reserved balance limit for the case
    /// when the canister doesn't have it set in the settings.
    pub fn default_reserved_balance_limit(&self) -> Cycles {
        self.config.default_reserved_balance_limit
    }
}

/// Encapsulates the payer and cost of inducting an ingress messages.
#[derive(Debug, Eq, PartialEq)]
pub enum IngressInductionCost {
    /// Induction is free.
    Free,
    /// Induction cost and the canister to pay for it.
    Fee { payer: CanisterId, cost: Cycles },
}

impl IngressInductionCost {
    /// Returns the cost of inducting an ingress message in [`Cycles`].
    pub fn cost(&self) -> Cycles {
        match self {
            Self::Free => Cycles::zero(),
            Self::Fee { cost, .. } => *cost,
        }
    }
}

/// Errors returned when computing the cost of receiving an ingress.
#[derive(Debug, Eq, PartialEq)]
pub enum IngressInductionCostError {
    /// The requested subnet method is not available.
    UnknownSubnetMethod,
    /// Failed to parse method payload.
    InvalidSubnetPayload(String),
    /// The subnet method can be called only by a canister.
    SubnetMethodNotAllowed,
}

// TODO(EXC-1168): cleanup, move unit tests from lib.rs into dedicated src/module_name/tests.rs.
#[cfg(test)]
mod tests {
    use super::*;
    use ic_test_utilities::types::ids::subnet_test_id;

    fn create_cycles_account_manager(reference_subnet_size: usize) -> CyclesAccountManager {
        let mut config = CyclesAccountManagerConfig::application_subnet();
        config.reference_subnet_size = reference_subnet_size;

        CyclesAccountManager {
            max_num_instructions: NumInstructions::from(1_000_000_000),
            own_subnet_type: SubnetType::Application,
            own_subnet_id: subnet_test_id(0),
            config,
            use_cost_scaling_flag: true,
        }
    }

    #[test]
    fn test_scale_cost() {
        let reference_subnet_size = 13;
        let cam = create_cycles_account_manager(reference_subnet_size);

        let cost = Cycles::new(13_000);
        assert_eq!(cam.scale_cost(cost, 0), Cycles::new(0));
        assert_eq!(cam.scale_cost(cost, 1), Cycles::new(1_000));
        assert_eq!(cam.scale_cost(cost, 6), Cycles::new(6_000));
        assert_eq!(cam.scale_cost(cost, 13), Cycles::new(13_000));
        assert_eq!(cam.scale_cost(cost, 26), Cycles::new(26_000));

        // Check overflow case.
        assert_eq!(
            cam.scale_cost(Cycles::new(std::u128::MAX), 1_000_000),
            Cycles::new(std::u128::MAX) / reference_subnet_size
        );
    }

    #[test]
    fn test_reference_subnet_size_is_not_zero() {
        // `reference_subnet_size` is used to scale cost according to a subnet size.
        // It should never be equal to zero.
        assert_ne!(
            CyclesAccountManagerConfig::application_subnet().reference_subnet_size,
            0
        );
        assert_ne!(
            CyclesAccountManagerConfig::verified_application_subnet().reference_subnet_size,
            0
        );
        assert_ne!(
            CyclesAccountManagerConfig::system_subnet().reference_subnet_size,
            0
        );
    }

    #[test]
    fn http_requests_fee_scale() {
        let subnet_size: u64 = 34;
        let reference_subnet_size: u64 = 13;
        let request_size = NumBytes::from(17);
        let cycles_account_manager = create_cycles_account_manager(reference_subnet_size as usize);

        // Check the fee for a 13-node subnet.
        assert_eq!(
            cycles_account_manager.http_request_fee(
                request_size,
                None,
                reference_subnet_size as usize,
            ),
            Cycles::from(1_603_786_800u64) * reference_subnet_size
        );

        // Check the fee for a 34-node subnet.
        assert_eq!(
            cycles_account_manager.http_request_fee(request_size, None, subnet_size as usize),
            Cycles::from(1_605_046_800u64) * subnet_size
        );
    }
}
