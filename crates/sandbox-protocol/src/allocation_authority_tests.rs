#![allow(clippy::unwrap_used)]
use super::*;

fn permit(host: HostId, serial: u64) -> Permit {
    Permit {
        host,
        project: ProjectId::generate(),
        sandbox: SandboxId::generate(),
        allocation: AllocationId::generate(),
        create_operation: OperationId::generate(),
        generation: 1,
        original_epoch: 1,
        serial,
    }
}
fn reload(a: &Authority) -> Authority {
    Authority::decode(&a.encode().unwrap(), a.0.host, a.through()).unwrap()
}
fn retire(a: &mut Authority, p: &Permit) {
    let retirement = OperationId::generate();
    a.fence(p, retirement).unwrap();
    *a = reload(a);
    assert_eq!(a.authorize(p), Err(Error::Conflict));
    a.complete(p, retirement).unwrap();
    *a = reload(a);
    a.forget(p, retirement).unwrap();
    *a = reload(a);
    assert_eq!(a.authorize(p), Err(Error::Closed));
}

#[test]
fn sustained_reuse_preserves_an_older_live_owner_and_bounded_state() {
    let host = HostId::generate();
    let mut a = Authority::new(host).unwrap();
    let oldest = permit(host, 1);
    a.register(std::slice::from_ref(&oldest)).unwrap();
    for serial in 2..=4097 {
        let p = permit(host, serial);
        a.register(std::slice::from_ref(&p)).unwrap();
        a.authorize(&p).unwrap();
        retire(&mut a, &p);
        a.authorize(&oldest).unwrap();
        assert_eq!(a.retained(), 1);
        assert!(a.encode().unwrap().len() < 1024);
        assert_eq!(a.register(&[p]), Err(Error::Closed));
    }
    assert_eq!(a.through(), 4097);
    a = reload(&a);
    a.authorize(&oldest).unwrap();
    retire(&mut a, &oldest);
    assert_eq!(a.retained(), 0);
    assert_eq!(a.through(), 4097);
}

#[test]
fn full_capacity_reclaims_a_slot_without_moving_or_reopening_the_frontier() {
    let host = HostId::generate();
    let mut a = Authority::new(host).unwrap();
    let all = (1..=MAX_ENTRIES as u64)
        .map(|serial| permit(host, serial))
        .collect::<Vec<_>>();
    for batch in all.chunks(MAX_BATCH) {
        a.register(batch).unwrap();
    }
    let newer = permit(host, MAX_ENTRIES as u64 + 1);
    let before = a.encode().unwrap();
    assert_eq!(
        a.register(std::slice::from_ref(&newer)),
        Err(Error::Capacity)
    );
    assert_eq!(a.encode().unwrap(), before);
    let retired = &all[500];
    let retirement = OperationId::generate();
    a.fence(retired, retirement).unwrap();
    a.complete(retired, retirement).unwrap();
    assert_eq!(
        a.register(std::slice::from_ref(&newer)),
        Err(Error::Capacity)
    );
    a.forget(retired, retirement).unwrap();
    a.register(std::slice::from_ref(&newer)).unwrap();
    assert_eq!(a.retained(), MAX_ENTRIES);
    a.authorize(&all[0]).unwrap();
    a.authorize(&all[MAX_ENTRIES - 1]).unwrap();
    a.authorize(&newer).unwrap();
    assert_eq!(a.authorize(retired), Err(Error::Closed));
    assert!(a.encode().unwrap().len() < MAX_BYTES);
}

#[test]
fn registration_is_atomic_contiguous_and_does_not_reactivate_retries() {
    let host = HostId::generate();
    let mut a = Authority::new(host).unwrap();
    let first = permit(host, 1);
    let second = permit(host, 2);
    let before = a.encode().unwrap();
    assert_eq!(
        a.register(std::slice::from_ref(&second)),
        Err(Error::RegistrationOrder)
    );
    assert_eq!(
        a.register(&[first.clone(), permit(host, 3)]),
        Err(Error::RegistrationOrder)
    );
    let mut duplicate = second.clone();
    duplicate.allocation = first.allocation;
    assert_eq!(a.register(&[first.clone(), duplicate]), Err(Error::Invalid));
    assert_eq!(a.encode().unwrap(), before);
    a.register(&[first.clone(), second.clone()]).unwrap();
    // Actual launches can arrive in either order after durable registration.
    a.authorize(&second).unwrap();
    a.authorize(&first).unwrap();
    let retirement = OperationId::generate();
    a.fence(&first, retirement).unwrap();
    a.register(&[first.clone(), second.clone()]).unwrap();
    assert_eq!(a.authorize(&first), Err(Error::Conflict));
    assert_eq!(a.state(&first).unwrap(), &State::Fenced { retirement });
    let before = a.encode().unwrap();
    assert_eq!(
        a.register(&[second, permit(host, 3)]),
        Err(Error::Unregistered)
    );
    assert_eq!(a.encode().unwrap(), before);
}

#[test]
fn every_identity_field_is_bound_and_denial_is_not_an_absence_receipt() {
    let host = HostId::generate();
    let p = permit(host, 1);
    let mut a = Authority::new(host).unwrap();
    a.register(std::slice::from_ref(&p)).unwrap();
    for mode in 0..8 {
        let mut wrong = p.clone();
        match mode {
            0 => wrong.host = HostId::generate(),
            1 => wrong.project = ProjectId::generate(),
            2 => wrong.sandbox = SandboxId::generate(),
            3 => wrong.allocation = AllocationId::generate(),
            4 => wrong.create_operation = OperationId::generate(),
            5 => wrong.generation += 1,
            6 => wrong.original_epoch += 1,
            _ => wrong.serial += 1,
        }
        let before = a.encode().unwrap();
        assert!(a.authorize(&wrong).is_err());
        assert!(a.fence(&wrong, OperationId::generate()).is_err());
        assert_eq!(a.encode().unwrap(), before);
    }
    retire(&mut a, &p);
    let unrelated_identity = permit(host, 1);
    // There is no original-owner or release proof left at serial 1. Both
    // requests are denied, not classified as ever having existed or stopped.
    assert_eq!(a.state(&p), Err(Error::Closed));
    assert_eq!(a.state(&unrelated_identity), Err(Error::Closed));
    assert_eq!(a.state(&permit(host, 2)), Err(Error::Unregistered));
}

#[test]
fn retirement_retries_bind_exact_intent_and_cannot_skip_a_durable_stage() {
    let host = HostId::generate();
    let p = permit(host, 1);
    let retirement = OperationId::generate();
    let other = OperationId::generate();
    let mut a = Authority::new(host).unwrap();
    a.register(std::slice::from_ref(&p)).unwrap();
    assert_eq!(a.complete(&p, retirement), Err(Error::Conflict));
    assert_eq!(a.forget(&p, retirement), Err(Error::Conflict));
    a.fence(&p, retirement).unwrap();
    a = reload(&a);
    a.fence(&p, retirement).unwrap();
    assert_eq!(a.fence(&p, other), Err(Error::Conflict));
    assert_eq!(a.complete(&p, other), Err(Error::Conflict));
    assert_eq!(a.forget(&p, retirement), Err(Error::Conflict));
    a.complete(&p, retirement).unwrap();
    a = reload(&a);
    // A lost completion response can be replayed without losing the receipt.
    a.fence(&p, retirement).unwrap();
    a.complete(&p, retirement).unwrap();
    assert_eq!(a.state(&p).unwrap(), &State::Complete { retirement });
    assert_eq!(a.forget(&p, other), Err(Error::Conflict));
    a.forget(&p, retirement).unwrap();
    a = reload(&a);
    assert_eq!(a.complete(&p, retirement), Err(Error::Closed));
    assert_eq!(a.forget(&p, retirement), Err(Error::Closed));
    assert_eq!(a.register(&[p]), Err(Error::Closed));
}

#[test]
fn malformed_and_rolled_back_storage_is_rejected_without_empty_fallback() {
    let host = HostId::generate();
    let p = permit(host, 1);
    let mut a = Authority::new(host).unwrap();
    a.register(std::slice::from_ref(&p)).unwrap();
    let good = a.encode().unwrap();
    assert!(Authority::decode(&good, HostId::generate(), 0).is_err());
    assert!(Authority::decode(&good, host, 2).is_err());
    for mode in 0..15 {
        let mut value: serde_json::Value = serde_json::from_slice(&good).unwrap();
        match mode {
            0 => value["version"] = 2.into(),
            1 => value["through"] = 0.into(),
            2 => value["unknown"] = true.into(),
            3 => value["entries"][0]["permit"]["generation"] = 0.into(),
            4 => value["entries"][0]["permit"]["original_epoch"] = 0.into(),
            5 => value["entries"][0]["permit"]["serial"] = 0.into(),
            6 => value["entries"][0]["permit"]["serial"] = 2.into(),
            7 => value["entries"][0]["permit"]["extra"] = true.into(),
            8 => value["entries"][0]["state"]["state"] = "revived".into(),
            9 => {
                value["entries"][0]["state"]["retirement"] =
                    OperationId::generate().to_string().into()
            }
            10 => value["entries"] = serde_json::json!([value["entries"][0], value["entries"][0]]),
            11 => value["entries"][0]["permit"]["host"] = HostId::generate().to_string().into(),
            12 => value["through"] = (MAX_SERIAL + 1).into(),
            13 => value["entries"][0]["state"]["state"] = "fenced".into(),
            _ => {
                value["entries"][0]["permit"]["allocation"] =
                    "alc_00000000-0000-7000-0000-000000000000".into()
            }
        }
        assert!(
            Authority::decode(&serde_json::to_vec(&value).unwrap(), host, 0).is_err(),
            "mode {mode}"
        );
    }
    assert!(Authority::decode(b"", host, 0).is_err());
    assert!(Authority::decode(&vec![b' '; MAX_BYTES + 1], host, 0).is_err());
    let text = String::from_utf8(good).unwrap();
    let duplicate = text.replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
    assert!(Authority::decode(duplicate.as_bytes(), host, 0).is_err());
}

#[test]
fn serial_exhaustion_never_wraps_and_invalid_batches_leave_state_unchanged() {
    let host = HostId::generate();
    let bytes = serde_json::to_vec(&Saved {
        version: 1,
        host,
        through: MAX_SERIAL - 1,
        entries: Vec::new(),
    })
    .unwrap();
    let mut a = Authority::decode(&bytes, host, MAX_SERIAL - 1).unwrap();
    let last = permit(host, MAX_SERIAL);
    a.register(std::slice::from_ref(&last)).unwrap();
    retire(&mut a, &last);
    let before = a.encode().unwrap();
    assert_eq!(
        a.register(&[permit(host, MAX_SERIAL + 1)]),
        Err(Error::Invalid)
    );
    assert_eq!(a.register(&[permit(host, 0)]), Err(Error::Invalid));
    assert_eq!(a.register(&[]), Err(Error::Invalid));
    let batch = (1..=MAX_BATCH as u64 + 1)
        .map(|n| permit(host, n))
        .collect::<Vec<_>>();
    assert_eq!(a.register(&batch), Err(Error::Invalid));
    assert_eq!(a.encode().unwrap(), before);
}

#[test]
fn duplicate_generations_and_operation_bindings_are_rejected_as_one_batch() {
    let host = HostId::generate();
    for mode in 0..3 {
        let mut a = Authority::new(host).unwrap();
        let p = permit(host, 1);
        a.register(std::slice::from_ref(&p)).unwrap();
        let mut next = permit(host, 2);
        match mode {
            0 => next.allocation = p.allocation,
            1 => next.create_operation = p.create_operation,
            _ => {
                next.sandbox = p.sandbox;
                next.generation = p.generation;
            }
        }
        let before = a.encode().unwrap();
        assert_eq!(a.register(&[next]), Err(Error::Invalid));
        assert_eq!(a.encode().unwrap(), before);
    }
}
