module billing_webhook_ingest_test {
  import billing_webhook_ingest.* from "./billing_webhook_ingest"

  run freshVerifiedDeliveryRecordsOnceTest =
    init
      .then(receive(true, DIGEST_A, TYPE_A))
      .then(verify)
      .then(record_fresh)
      .expect(and {
        s.phase == PHASE_RECORDED,
        s.stored,
        s.stored_digest == DIGEST_A,
        s.stored_event_type == TYPE_A,
        s.effects == 1,
        ingest_safety,
      })

  run exactAuthenticatedRedeliveryIsNoOpTest =
    init
      .then(seed_existing(DIGEST_A, TYPE_A))
      .then(receive(true, DIGEST_A, TYPE_A))
      .then(verify)
      .then(classify_exact)
      .expect(and {
        s.phase == PHASE_DEDUPED,
        s.effects == 0,
        exact_redelivery(s),
        ingest_safety,
      })

  run digestReuseConflictFailsClosedTest =
    init
      .then(seed_existing(DIGEST_A, TYPE_A))
      .then(receive(true, DIGEST_B, TYPE_A))
      .then(verify)
      .then(classify_conflict)
      .expect(and {
        s.phase == PHASE_CONFLICT,
        s.effects == 0,
        not(exact_redelivery(s)),
        ingest_safety,
      })

  run eventTypeReuseConflictFailsClosedTest =
    init
      .then(seed_existing(DIGEST_A, TYPE_A))
      .then(receive(true, DIGEST_A, TYPE_B))
      .then(verify)
      .then(classify_conflict)
      .expect(and {
        s.phase == PHASE_CONFLICT,
        s.effects == 0,
        not(exact_redelivery(s)),
        ingest_safety,
      })

  run invalidSignatureNeverRecordsTest =
    init
      .then(receive(false, DIGEST_A, TYPE_A))
      .then(reject)
      .expect(and {
        s.phase == PHASE_REJECTED,
        not(s.signature_valid),
        s.effects == 0,
        ingest_safety,
      })

  run storageFailureCreatesNoEffectTest =
    init
      .then(receive(true, DIGEST_A, TYPE_A))
      .then(verify)
      .then(storage_failure)
      .expect(and {
        s.phase == PHASE_STORAGE_FAILED,
        s.effects == 0,
        ingest_safety,
      })

  run retryAfterRecordIsNoOpTest =
    init
      .then(receive(true, DIGEST_A, TYPE_A))
      .then(verify)
      .then(record_fresh)
      .then(retry)
      .expect(and {
        s.phase == PHASE_RECORDED,
        s.effects == 1,
        ingest_safety,
      })

  run conflictCannotBeAcknowledgedAsExactTest =
    init
      .then(seed_existing(DIGEST_A, TYPE_A))
      .then(receive(true, DIGEST_B, TYPE_A))
      .then(verify)
      .then(classify_exact)
      .fail()
}
