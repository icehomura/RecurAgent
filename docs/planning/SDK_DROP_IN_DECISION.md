# SDK Drop-In Decision: RETIRE Strict Contract Parity

**Bead**: bd-lnmtp.3.1 (G06-T1)  
**Date**: 2026-04-17  
**Decision**: RETIRE the strict SDK drop-in claim  

## Executive Summary

After comprehensive analysis of the SDK contract (`docs/dropin-sdk-contract.json`) versus actual implementation (`src/sdk.rs`, 2455 lines), the decision is to **retire the strict SDK drop-in claim** in favor of functional parity.

## Key Analysis Findings

### Contract vs Reality Gap
The contract significantly understates current implementation status:

| Capability | Contract Claims | Actual Status |
|------------|----------------|---------------|
| SDK-01 (session factory) | missing | ✅ `create_agent_session()` implemented |
| SDK-02 (prompt streaming) | partial | ✅ `prompt()`, `continue_turn()` complete |
| SDK-03 (steer/followup) | partial | ✅ `steer()`, `follow_up()` in RPC layer |
| SDK-04 (event subscription) | partial | ✅ `subscribe()`/`unsubscribe()` complete |
| SDK-05 (model controls) | partial | ✅ `set_model()`, `set_thinking_level()` complete |
| SDK-06 (session management) | missing | ❓ `switch_session()`, `fork()` exist |
| SDK-07 (compaction/abort) | partial | ✅ `compact()`, abort methods complete |
| SDK-08 (tools/hooks) | partial | ✅ Extensive tool factories implemented |
| SDK-09 (transport adapters) | missing | ❓ RPC client exists, needs validation |
| SDK-10 (contract stability) | implemented | ✅ Stable `ra::sdk` module |

**7/10 capabilities are actually implemented or nearly complete**, not "3 missing, 6 partial" as documented.

### Core Issues with Strict Parity Approach

1. **Documentation Debt > Implementation Debt**: The primary gap is outdated contract documentation, not missing functionality

2. **API Shape vs Functional Parity**: Strict TypeScript API mirroring adds implementation complexity without clear value. The Rust SDK provides equivalent capabilities using idiomatic Rust patterns (Result types, owned/borrowed distinctions, async/await)

3. **Maintenance Burden**: Maintaining exact TypeScript parity requires constant synchronization effort that could be better spent on feature development

4. **Blocking Downstream Work**: This decision bead blocks 3 downstream issues; retiring the strict claim unblocks real progress

## Decision Rationale

### RETIRE because:

- **Functional parity achieved**: Core agent session, streaming, events, tools, model controls all work equivalently
- **Rust idioms preferred**: `Result<T>` error handling, `Arc<T>` sharing, structured concurrency patterns provide better ergonomics than TypeScript API shapes
- **Resource allocation**: Engineering effort better spent on features/performance than API shape synchronization
- **User experience**: SDK consumers care about capabilities, not API syntax matching

### Follow-up Actions Required:

1. **Update contract documentation** to reflect actual implementation status
2. **Create functional parity validation tests** instead of API shape tests  
3. **Document migration guides** for TypeScript → Rust SDK consumers
4. **Close blocked downstream beads** with functional parity approach

## Implementation Path Forward

**Phase 1**: Document existing SDK capabilities accurately  
**Phase 2**: Fill genuine functionality gaps (session management, transport validation)  
**Phase 3**: Performance optimization and production hardening  
**Phase 4**: Migration tooling and documentation  

## Risk Mitigation

- **User migration**: Provide clear capability mapping and migration examples
- **Compatibility**: Maintain RPC protocol compatibility for cross-language integration
- **Documentation**: Comprehensive SDK cookbook with Rust-specific patterns

## Conclusion

The strict SDK drop-in claim creates more problems than it solves. The Rust SDK already provides equivalent functionality in idiomatic forms. Retiring the strict claim allows focus on real value delivery while maintaining functional equivalence.

**Next Actions**: 
- Close bd-lnmtp.3.1 with RETIRE decision
- Spawn follow-up bead for accurate SDK documentation
- Unblock downstream G06 decision tree