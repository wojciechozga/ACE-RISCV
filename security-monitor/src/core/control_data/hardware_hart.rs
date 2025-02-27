// SPDX-FileCopyrightText: 2023 IBM Corporation
// SPDX-FileContributor: Wojciech Ozga <woz@zurich.ibm.com>, IBM Research - Zurich
// SPDX-License-Identifier: Apache-2.0
use crate::core::architecture::specification::*;
use crate::core::architecture::{
    are_bits_enabled, disable_bit, enable_bit, GeneralPurposeRegister, HartArchitecturalState, TrapCause, CSR,
};
use crate::core::control_data::ConfidentialHart;
use crate::core::memory_protector::HypervisorMemoryProtector;
use crate::core::page_allocator::{Allocated, Page, UnAllocated};
use crate::core::transformations::{
    EnabledInterrupts, ExposeToHypervisor, GuestLoadPageFaultRequest, GuestLoadPageFaultResult, InjectedInterrupts, InterruptRequest,
    MmioLoadRequest, MmioStoreRequest, OpensbiRequest, OpensbiResult, PromoteToConfidentialVm, ResumeRequest, SbiRequest, SbiResult,
    SbiVmRequest, SharePageResult, TerminateRequest,
};

pub const HART_STACK_ADDRESS_OFFSET: usize = memoffset::offset_of!(HardwareHart, stack_address);

#[repr(C)]
pub struct HardwareHart {
    // Safety: HardwareHart and ConfidentialHart must both start with the HartArchitecturalState element because based
    // on this we automatically calculate offsets of registers' and CSRs' for the context switch implemented in assembly.
    pub(super) non_confidential_hart_state: HartArchitecturalState,
    // Memory protector that configures the hardware memory isolation component to allow only memory accesses
    // to the memory region owned by the hypervisor.
    hypervisor_memory_protector: HypervisorMemoryProtector,
    // A page containing the stack of the code executing within the given hart.
    pub(super) stack: Page<Allocated>,
    // The stack_address is redundant (we can learn the stack_address from the page assigned to the stack) but we need
    // it because this is the way to expose it to assembly
    pub(super) stack_address: usize,
    // We need to store the OpenSBI's mscratch value because OpenSBI uses mscratch to track some of its internal
    // data structures and our security monitor also uses mscratch to keep track of the address of the hart state
    // in memory.
    previous_mscratch: usize,
    // We keep the virtual hart that is associated with this hardware hart. The virtual hart can be 1) a dummy hart
    // in case there is any confidential VM's virtual hart associated to it, or 2) an confidential VM's virtual hart.
    // In the latter case, the hardware hart and confidential VM's control data swap their virtual harts (a dummy
    // hart with the confidential VM's virtual hart)
    pub(super) confidential_hart: ConfidentialHart,
}

impl HardwareHart {
    pub fn init(id: usize, stack: Page<UnAllocated>, hypervisor_memory_protector: HypervisorMemoryProtector) -> Self {
        Self {
            non_confidential_hart_state: HartArchitecturalState::empty(id),
            hypervisor_memory_protector,
            stack_address: stack.end_address(),
            stack: stack.zeroize(),
            previous_mscratch: 0,
            confidential_hart: ConfidentialHart::dummy(id),
        }
    }

    pub fn address(&self) -> usize {
        core::ptr::addr_of!(self.non_confidential_hart_state) as usize
    }

    /// Calling OpenSBI handler to process the SBI call requires setting the mscratch register to a specific value which
    /// we replaced during the system initialization. We store the original mscratch value expected by the OpenSBI in
    /// the previous_mscratch field.
    pub fn swap_mscratch(&mut self) {
        let current_mscratch = CSR.mscratch.read();
        CSR.mscratch.set(self.previous_mscratch);
        self.previous_mscratch = current_mscratch;
    }

    pub fn confidential_hart(&self) -> &ConfidentialHart {
        &self.confidential_hart
    }

    pub fn confidential_hart_mut(&mut self) -> &mut ConfidentialHart {
        &mut self.confidential_hart
    }

    pub unsafe fn enable_hypervisor_memory_protector(&self) {
        self.hypervisor_memory_protector.enable(self.non_confidential_hart_state.hgatp)
    }

    /// Dumps control and status registers (CSRs) of the physical hart executing this code to the main memory.
    pub fn store_control_status_registers_in_main_memory(&mut self) -> InjectedInterrupts {
        self.non_confidential_hart_state.store_control_status_registers_in_main_memory();
        // TODO: when moving to CoVE, injecting interrupts becomes an explicit request from the hypervisor to security monitor. We should
        // adapt the same strategy, which would also better reflect out current approach for information declassification.
        self.interrupts_to_inject()
    }

    pub fn store_volatile_control_status_registers_in_main_memory(&mut self) {
        self.non_confidential_hart_state.mepc = CSR.mepc.read();
        self.non_confidential_hart_state.mstatus = CSR.mstatus.read();
    }

    /// Loads control and status registers (CSRs) from the main memory into the physical hart executing this code.
    pub fn load_control_status_registers_from_main_memory(&mut self, enabled_interrupts: EnabledInterrupts) {
        self.non_confidential_hart_state.load_control_status_registers_from_main_memory();
        // TODO: when moving to CoVE, exposing enabled interrupts becomes an explicit hypercall. We should adapt the same strategy, which
        // would also better reflect out current approach for information declassification.
        self.apply(&ExposeToHypervisor::EnabledInterrupts(enabled_interrupts));
    }

    /// Loads control and status registers (CSRs) that might have changed during execution of the security monitor. This function should be
    /// called just before exiting to the assembly context switch, so when we are sure that these CSRs have their final values.
    pub fn load_volatile_control_status_registers_from_main_memory(&self) {
        CSR.mepc.set(self.non_confidential_hart_state.mepc);
        CSR.mstatus.set(self.non_confidential_hart_state.mstatus);
    }
}

impl HardwareHart {
    pub fn apply(&mut self, transformation: &ExposeToHypervisor) {
        match transformation {
            ExposeToHypervisor::SbiRequest(v) => self.apply_sbi_request(v),
            ExposeToHypervisor::SbiVmRequest(v) => self.apply_sbi_vm_request(v),
            ExposeToHypervisor::SbiResult(v) => self.apply_sbi_result(v),
            ExposeToHypervisor::OpensbiResult(v) => self.apply_opensbi_result(v),
            ExposeToHypervisor::MmioLoadRequest(v) => self.apply_mmio_load_request(v),
            ExposeToHypervisor::MmioStoreRequest(v) => self.apply_mmio_store_request(v),
            ExposeToHypervisor::InterruptRequest(v) => self.apply_interrupt_request(v),
            ExposeToHypervisor::EnabledInterrupts(v) => self.apply_enabled_interrupts(v),
        }
    }

    fn apply_enabled_interrupts(&mut self, result: &EnabledInterrupts) {
        CSR.vsie.set(result.vsie);
    }

    fn apply_sbi_result(&mut self, result: &SbiResult) {
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a0, result.a0());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a1, result.a1());
        self.non_confidential_hart_state.mepc += result.pc_offset();
    }

    fn apply_opensbi_result(&mut self, result: &OpensbiResult) {
        self.non_confidential_hart_state.mstatus = result.trap_regs.mstatus.try_into().unwrap();
        self.non_confidential_hart_state.mepc = result.trap_regs.mepc.try_into().unwrap();
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a0, result.trap_regs.a0.try_into().unwrap());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a1, result.trap_regs.a1.try_into().unwrap());
    }

    fn apply_sbi_vm_request(&mut self, request: &SbiVmRequest) {
        CSR.scause.set(CAUSE_VIRTUAL_SUPERVISOR_ECALL.into());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a7, request.sbi_request().extension_id());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a6, request.sbi_request().function_id());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a0, request.sbi_request().a0());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a1, request.sbi_request().a1());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a2, request.sbi_request().a2());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a3, request.sbi_request().a3());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a4, request.sbi_request().a4());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a5, request.sbi_request().a5());
        self.apply_trap(false);
    }

    fn apply_sbi_request(&mut self, request: &SbiRequest) {
        CSR.scause.set(CAUSE_VIRTUAL_SUPERVISOR_ECALL.into());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a7, request.extension_id());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a6, request.function_id());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a0, request.a0());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a1, request.a1());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a2, request.a2());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a3, request.a3());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a4, request.a4());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a5, request.a5());
        self.apply_trap(false);
    }

    fn apply_mmio_load_request(&mut self, request: &MmioLoadRequest) {
        CSR.scause.set(request.code());
        // KVM uses htval and stval to recreate the fault address
        CSR.stval.set(request.stval());
        CSR.htval.set(request.htval());
        // Hack: we do not allow the hypervisor to look into the guest memory but we have to inform him about the instruction that caused
        // exception. our approach is to expose this instruction via vsscratch. In future, we should move to RISC-V NACL extensions.
        CSR.vsscratch.set(request.instruction());
        self.apply_trap(true);
    }

    fn apply_mmio_store_request(&mut self, request: &MmioStoreRequest) {
        CSR.scause.set(request.code());
        // KVM uses htval and stval to recreate the fault address
        CSR.stval.set(request.stval());
        CSR.htval.set(request.htval());
        self.non_confidential_hart_state.set_gpr(request.gpr(), request.gpr_value());
        // Hack: we do not allow the hypervisor to look into the guest memory but we have to inform him about the instruction that caused
        // exception. our approach is to expose this instruction via vsscratch. In future, we should move to RISC-V NACL extensions.
        CSR.vsscratch.set(request.instruction());
        self.apply_trap(true);
    }

    fn apply_interrupt_request(&mut self, request: &InterruptRequest) {
        CSR.scause.set(request.code() | SCAUSE_INTERRUPT_MASK);
        self.apply_trap(false);
    }

    #[inline]
    fn apply_trap(&mut self, encoded_guest_virtual_address: bool) {
        if are_bits_enabled(CSR.stvec.read(), STVEC_MODE_VECTORED) {
            panic!("Not supported functionality: vectored traps");
        }

        // Set next mode to HS (see Table 8.8 in Riscv privilege spec 20211203)
        disable_bit(&mut self.non_confidential_hart_state.mstatus, CSR_MSTATUS_MPV);
        enable_bit(&mut self.non_confidential_hart_state.mstatus, CSR_MSTATUS_MPP);
        disable_bit(&mut self.non_confidential_hart_state.mstatus, CSR_MSTATUS_MPIE);
        disable_bit(&mut self.non_confidential_hart_state.mstatus, CSR_MSTATUS_SIE);

        // Resume HS execution at its trap function
        CSR.sepc.set(self.non_confidential_hart_state.mepc);
        self.non_confidential_hart_state.mepc = CSR.stvec.read();

        // We trick the hypervisor to think that the trap comes directly from the VS-mode.
        enable_bit(&mut self.non_confidential_hart_state.mstatus, CSR_MSTATUS_SPP);
        CSR.hstatus.read_and_set_bit(CSR_HSTATUS_SPV);
        CSR.hstatus.read_and_set_bit(CSR_HSTATUS_SPVP);
        // According to the spec, hstatus:SPVP and sstatus.SPP have the same value when transitioning from VS to HS mode.
        CSR.sstatus.read_and_set_bit(CSR_SSTATUS_SPP);

        if encoded_guest_virtual_address {
            CSR.hstatus.read_and_set_bit(CSR_HSTATUS_GVA);
        } else {
            CSR.hstatus.read_and_clear_bit(CSR_HSTATUS_GVA);
        }
    }
}

impl HardwareHart {
    pub fn trap_reason(&mut self) -> TrapCause {
        use crate::core::architecture::SbiExtension;
        let cause = CSR.mcause.read();
        let extension_id = self.non_confidential_hart_state.gpr(GeneralPurposeRegister::a7);
        let function_id = self.non_confidential_hart_state.gpr(GeneralPurposeRegister::a6);
        let trap_reason = TrapCause::from(cause, extension_id, function_id);

        // `ecall` from the hypervisor carry additional information that must be restored.
        match trap_reason {
            TrapCause::HsEcall(SbiExtension::Ace(_)) => self.restore_original_gprs(),
            _ => {}
        }
        trap_reason
    }

    pub fn promote_to_confidential_vm_request(&self) -> PromoteToConfidentialVm {
        PromoteToConfidentialVm::new(&self.non_confidential_hart_state)
    }

    pub fn hypercall_result(&self) -> SbiResult {
        SbiResult::ecall(&self.non_confidential_hart_state)
    }

    pub fn guest_load_page_fault_result(&self, request: GuestLoadPageFaultRequest) -> GuestLoadPageFaultResult {
        GuestLoadPageFaultResult::new(&self.non_confidential_hart_state, request)
    }

    pub fn sbi_vm_request(&self) -> SbiVmRequest {
        SbiVmRequest::from_hart_state(&self.non_confidential_hart_state)
    }

    pub fn resume_request(&self) -> ResumeRequest {
        let (confidential_vm_id, confidential_hart_id) = self.read_security_monitor_call_arguments();
        ResumeRequest::new(confidential_vm_id, confidential_hart_id)
    }

    pub fn terminate_request(&self) -> TerminateRequest {
        let (confidential_vm_id, _) = self.read_security_monitor_call_arguments();
        TerminateRequest::new(confidential_vm_id)
    }

    pub fn share_page_result(&self) -> SharePageResult {
        let is_error = self.non_confidential_hart_state.gpr(GeneralPurposeRegister::a0);
        let hypervisor_page_address = self.non_confidential_hart_state.gpr(GeneralPurposeRegister::a1);
        SharePageResult::new(is_error, hypervisor_page_address)
    }

    pub fn opensbi_request(&self) -> OpensbiRequest {
        OpensbiRequest::new(&self.non_confidential_hart_state)
    }

    pub fn interrupts_to_inject(&self) -> InjectedInterrupts {
        InjectedInterrupts::new()
    }

    pub fn restore_original_gprs(&mut self) {
        // Arguments to security monitor calls are stored in vs* CSRs because we cannot use regular general purpose registers (GRPs).
        // GRPs might carry SBI- or MMIO-related reponses, so using GRPs would destroy the communication between the
        // hypervisor and confidential VM. This is a hackish (temporal?) solution, we should probably move to the RISC-V
        // NACL extension that solves these problems by using shared memory region in which the SBI- and MMIO-related
        // information is transfered. Below we restore the original `a7` and `a6`.
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a7, CSR.vstval.read());
        self.non_confidential_hart_state.set_gpr(GeneralPurposeRegister::a6, CSR.vsepc.read());
    }

    fn read_security_monitor_call_arguments(&self) -> (usize, usize) {
        // Arguments to security monitor calls are stored in vs* CSRs because we cannot use regular general purpose registers (GRPs). GRPs
        // might carry SBI- or MMIO-related reponses, so using GRPs would destroy the communication between the hypervisor and confidential
        // VM. This is a hackish (temporal?) solution, we should probably move to the RISC-V NACL extension that solves these problems by
        // using shared memory region in which the SBI- and MMIO-related information is transfered.
        (CSR.vstvec.read(), CSR.vsscratch.read())
    }
}
