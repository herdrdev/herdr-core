# Proposal: conditional terminal target guards

Status: proposed contract for maintainer discussion. This document changes no runtime behavior and establishes no implemented guarantee.

## Problem and scope

A stored terminal address can identify a different allocation after terminal-registry state is reset and a numeric ID is reused. Qualifying it with the core ID distinguishes a different core identity, but does not distinguish allocations recreated under a retained core identity. `created_ms` describes time; it does not promise uniqueness or prevent effects after a later target change.

There is a second distinction within an allocation: a kept terminal can acquire a replacement child program. A delayed input phase, signal escalation or cleanup intended for the earlier child must not silently select that replacement. Core state, holder connection ownership, terminal allocation and child/PTY lifetime are related but different boundaries.

The proposal adds conditional targeting to existing operations. It does not add agent-specific logic, a registry of agents, or a general action language. Identity guards identify the intended resource; authorization remains separate.

## Candidate contract

Expose opaque allocation and execution facts and accept an optional expected target on relevant requests. Names and encoding require maintainer agreement; an illustrative shape is:

```json
{
  "terminal": "<core-id>/t1",
  "expected": {
    "core": "<saved-core-id>",
    "allocation": "<saved-allocation-token>",
    "execution": "<saved-child-or-PTY-token>"
  }
}
```

Allocation-scoped operations compare the saved allocation. Process-directed effects also compare the saved execution or retain a proven equivalent child/PTY handle. A surviving allocation and child can keep their identities through an ordinary daemon restart; a new allocation or replacement child receives a distinct identity. Neither identity is reconstructed from a PID or wall-clock timestamp.

The destination validates the expected target while selecting the actual resource under the relevant synchronization. After queue waits, reconnects or other awaits, later effects retain the same expected target. They never refresh it from the current occupant of the address. Holder handover fencing must cover effectful commands and delayed completions, including work already read before the connection changes.

For multi-phase operations, earlier effects remain facts. If text reached the original child before a later phase is refused, report a partial or uncertain result with the completed phase. A lost reply is not proof of no effect and does not justify an automatic replay. This proposal does not promise exactly-once delivery.

## Persistence, staging and creation

The identity mechanism must prevent reuse across the supported registry-reset case, including equal timestamps. One candidate is durable allocation epochs plus non-reused ordinals, with retained records preserving their existing identities. Counter exhaustion, inconsistent state and failed durable publication must not silently advertise a restart-stable identity. Other representations with the same demonstrated properties remain possible.

File staging needs an operation-owned resource and an explicit guarded publication point. Reset or cancellation must not publish into, or clean up through, a replacement's reused numeric path. Process creation similarly distinguishes preparation, actual OS creation and publication/adoption. Late spawn results and timeout cleanup remain bound to their original operation and child handle. Once a process has run, cancellation cannot truthfully report a pre-effect refusal.

Supported whole-state restore and cloning need a defined identity-reset procedure before restored effects or endpoints resume. A rolled-back snapshot cannot prove its own historical high-water mark. A fresh identity under an external recovery fence is one approach; preserving identity would require independently established exclusivity and non-rollback fencing. Arbitrary privileged snapshot manipulation is not claimed detectable by the current core.

## Compatibility

Existing clients can retain their documented behavior. Guard-aware callers negotiate a versioned capability on the actual effect connection and destination, including loaded holder support. Older parsers may ignore unknown parameters, so sending an extra field alone cannot establish enforcement. Unsupported peers refuse a guarded operation before effects; there is no silent unguarded fallback.

Keep the schema and CLI derived from the existing method table. Specify the complete supported method set and mismatch/partial outcomes. Historical references can remain readable, but must not be silently upgraded by looking up the current occupant of an old address. A caller can explicitly acquire a fresh verified target under its own current authorization.

## Acceptance evidence before a guarantee is advertised

Use isolated scratch cores and real PTY fixtures where appropriate. Each relevant method needs a fresh-target success case and a discriminating control that fails when its guard is removed.

- Normal restart preserves a valid surviving target; full wipe and registry-only reset reject stale targets, including equal timestamps.
- A reset between observation and effect cannot redirect the action to a replacement.
- Replacing only the child under an unchanged allocation cannot receive delayed input, escalation or cleanup for the old child.
- Holder handover rejects stale effect frames and prevents late spawn completion from adopting or cleaning up the wrong resource.
- Reset during file preparation and around publication preserves resource ownership and truthful outcomes.
- Mixed-version reconnect and remote forwarding cannot silently discard the guard.
- Whole-snapshot rollback and a supported clone while the original is alive satisfy the selected recovery postconditions.
- Retained writable channels and client-held references preserve the same guarantees; missing replies remain uncertain.

## Relationship to current work

This proposal was prepared against master `cd12ddd88bc11490228343291dc1096c9ed3bf77`. It should compose with existing work rather than duplicate it: [#118](https://github.com/herdrdev/herdr-core/pull/118) addresses permanent-ID selector precedence; [#164](https://github.com/herdrdev/herdr-core/pull/164) extends status-conditional send; [#169](https://github.com/herdrdev/herdr-core/pull/169) clarifies send observations; and [#183](https://github.com/herdrdev/herdr-core/pull/183) addresses adoption status events. These open PRs are not asserted to implement this target-identity contract.

Maintainer alignment is requested on whether to pursue conditional targeting, the allocation/execution semantics and representation, the supported recovery boundary, and a coherent implementation sequence. No implementation, tests, maintainer approval, release or installation is claimed by this proposal.
