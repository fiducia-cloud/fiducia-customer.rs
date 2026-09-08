// Bounded executable model of billing webhook verification and idempotent
// ingestion.
//
// The cryptographic verifier is abstracted as `signature_valid`; its
// implementation remains owned by fiducia-payments.rs. This model begins with
// an optional durable row and proves the classification performed after a
// provider event has been authenticated.
//
// Production mapping:
//   verify                  -> billing::verify
//   record_fresh            -> billing::record successful insert
//   classify_exact          -> billing::is_exact_redelivery == true
//   classify_conflict       -> billing::is_exact_redelivery == false
//   effects                 -> rows/effects created by the current delivery
module billing_webhook_ingest {
  type State = {
    phase: int,
    stored: bool,
    stored_digest: int,
    stored_event_type: int,
    incoming_digest: int,
    incoming_event_type: int,
    signature_valid: bool,
    effects: int,
  }

  pure val PHASE_IDLE = 0
  pure val PHASE_RECEIVED = 1
  pure val PHASE_VERIFIED = 2
  pure val PHASE_REJECTED = 3
  pure val PHASE_RECORDED = 4
  pure val PHASE_DEDUPED = 5
  pure val PHASE_CONFLICT = 6
  pure val PHASE_STORAGE_FAILED = 7

  pure val DIGEST_NONE = 0
  pure val DIGEST_A = 1
  pure val DIGEST_B = 2
  pure val TYPE_NONE = 0
  pure val TYPE_A = 1
  pure val TYPE_B = 2

  pure val DIGESTS: Set[int] = Set(DIGEST_A, DIGEST_B)
  pure val EVENT_TYPES: Set[int] = Set(TYPE_A, TYPE_B)
  pure val BOOLEANS: Set[bool] = Set(true, false)
  pure val TERMINAL_PHASES: Set[int] = Set(
    PHASE_REJECTED,
    PHASE_RECORDED,
    PHASE_DEDUPED,
    PHASE_CONFLICT,
    PHASE_STORAGE_FAILED
  )

  var s: State

  pure def exact_redelivery(state: State): bool = and {
    state.stored,
    state.stored_digest == state.incoming_digest,
    state.stored_event_type == state.incoming_event_type,
  }

  action init: bool = {
    s' = {
      phase: PHASE_IDLE,
      stored: false,
      stored_digest: DIGEST_NONE,
      stored_event_type: TYPE_NONE,
      incoming_digest: DIGEST_NONE,
      incoming_event_type: TYPE_NONE,
      signature_valid: false,
      effects: 0,
    }
  }

  // Represents a durable row created by an earlier authenticated delivery.
  action seed_existing(digest: int, event_type: int): bool = all {
    s.phase == PHASE_IDLE,
    not(s.stored),
    DIGESTS.contains(digest),
    EVENT_TYPES.contains(event_type),
    s' = {
      ...s,
      stored: true,
      stored_digest: digest,
      stored_event_type: event_type,
    },
  }

  action receive(signature_valid: bool, digest: int, event_type: int): bool = all {
    s.phase == PHASE_IDLE,
    DIGESTS.contains(digest),
    EVENT_TYPES.contains(event_type),
    s' = {
      ...s,
      phase: PHASE_RECEIVED,
      incoming_digest: digest,
      incoming_event_type: event_type,
      signature_valid: signature_valid,
    },
  }

  action verify: bool = all {
    s.phase == PHASE_RECEIVED,
    s.signature_valid,
    s' = { ...s, phase: PHASE_VERIFIED },
  }

  action reject: bool = all {
    s.phase == PHASE_RECEIVED,
    not(s.signature_valid),
    s' = { ...s, phase: PHASE_REJECTED },
  }

  action record_fresh: bool = all {
    s.phase == PHASE_VERIFIED,
    not(s.stored),
    s' = {
      ...s,
      phase: PHASE_RECORDED,
      stored: true,
      stored_digest: s.incoming_digest,
      stored_event_type: s.incoming_event_type,
      effects: s.effects + 1,
    },
  }

  action classify_exact: bool = all {
    s.phase == PHASE_VERIFIED,
    exact_redelivery(s),
    s' = { ...s, phase: PHASE_DEDUPED },
  }

  action classify_conflict: bool = all {
    s.phase == PHASE_VERIFIED,
    s.stored,
    not(exact_redelivery(s)),
    s' = { ...s, phase: PHASE_CONFLICT },
  }

  action storage_failure: bool = all {
    s.phase == PHASE_VERIFIED,
    s' = { ...s, phase: PHASE_STORAGE_FAILED },
  }

  // Provider retries after a terminal response cannot change durable state or
  // create another effect within this delivery classification.
  action retry: bool = all {
    TERMINAL_PHASES.contains(s.phase),
    s' = s,
  }

  action idle: bool = s' = s

  action step: bool = {
    nondet digest = DIGESTS.oneOf()
    nondet event_type = EVENT_TYPES.oneOf()
    nondet signature_valid = BOOLEANS.oneOf()
    any {
      seed_existing(digest, event_type),
      receive(signature_valid, digest, event_type),
      verify,
      reject,
      record_fresh,
      classify_exact,
      classify_conflict,
      storage_failure,
      retry,
      idle,
    }
  }

  val ingest_safety = and {
    Set(
      PHASE_IDLE,
      PHASE_RECEIVED,
      PHASE_VERIFIED,
      PHASE_REJECTED,
      PHASE_RECORDED,
      PHASE_DEDUPED,
      PHASE_CONFLICT,
      PHASE_STORAGE_FAILED
    ).contains(s.phase),
    s.effects >= 0,
    s.effects <= 1,
    s.stored.implies(and {
      DIGESTS.contains(s.stored_digest),
      EVENT_TYPES.contains(s.stored_event_type),
    }),
    not(s.stored).implies(and {
      s.stored_digest == DIGEST_NONE,
      s.stored_event_type == TYPE_NONE,
    }),
    Set(PHASE_RECEIVED, PHASE_VERIFIED).contains(s.phase).implies(and {
      DIGESTS.contains(s.incoming_digest),
      EVENT_TYPES.contains(s.incoming_event_type),
    }),
    (s.phase == PHASE_VERIFIED).implies(s.signature_valid),
    (s.phase == PHASE_REJECTED).implies(and {
      not(s.signature_valid),
      s.effects == 0,
    }),
    (s.phase == PHASE_RECORDED).implies(and {
      s.signature_valid,
      s.stored,
      exact_redelivery(s),
      s.effects == 1,
    }),
    (s.phase == PHASE_DEDUPED).implies(and {
      s.signature_valid,
      exact_redelivery(s),
      s.effects == 0,
    }),
    (s.phase == PHASE_CONFLICT).implies(and {
      s.signature_valid,
      s.stored,
      not(exact_redelivery(s)),
      s.effects == 0,
    }),
    (s.phase == PHASE_STORAGE_FAILED).implies(s.effects == 0),
    (s.effects == 1).implies(s.phase == PHASE_RECORDED),
  }

  val recorded_reached = s.phase == PHASE_RECORDED
  val exact_redelivery_reached = s.phase == PHASE_DEDUPED
  val identity_conflict_reached = s.phase == PHASE_CONFLICT
  val invalid_signature_reached = s.phase == PHASE_REJECTED
  val storage_failure_reached = s.phase == PHASE_STORAGE_FAILED
}
