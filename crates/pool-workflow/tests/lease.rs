//! Slice-1 criterion G3: a lease claimant that lost its fence cannot commit a
//! late result (`architecture.md` §7.5 step 4, invariant 8).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_domain::Network;
use pool_test_support::TempDb;
use pool_workflow::lease::{self, LeaseError, LeaseKind};
use pool_workflow::workflow::{self, Owner, POOL_BOOTSTRAP_OWNER};
use sqlx::Connection;

const NET: Network = Network::Testnet;
const KIND: LeaseKind = LeaseKind::ProofBuild;

async fn workflow_row(pool: &sqlx::PgPool, id: &str) {
    workflow::create(
        pool,
        NET,
        id,
        Owner::PoolBootstrap,
        POOL_BOOTSTRAP_OWNER,
        90,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_reclaim_takes_a_higher_fence_and_the_old_owner_cannot_commit() {
    // G3, and the reason the fence exists. Expiry alone proves the old owner
    // *should* have stopped, not that it *did*: a process stalled in a network
    // call has no idea its lease lapsed. The fence is what stops it committing
    // over whoever took the work.
    let Some(db) = TempDb::migrated("lease_g3").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    workflow_row(&pool, "w1").await;

    // Claim, then let it lapse.
    let first = lease::claim(&pool, NET, "w1", KIND, "worker-a", 60)
        .await
        .unwrap();
    assert_eq!(first.fence_token, 1);
    sqlx::query("UPDATE pool.work_lease SET lease_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();

    // Another process reclaims, taking a higher fence.
    let second = lease::claim(&pool, NET, "w1", KIND, "worker-b", 60)
        .await
        .unwrap();
    assert_eq!(second.fence_token, 2, "a reclaim never reuses a fence");
    assert_eq!(second.lease_owner, "worker-b");

    // worker-a finishes its slow work and tries to commit. It cannot.
    let mut tx = sqlx::PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    let error = lease::require_fence(&mut tx, &first)
        .await
        .expect_err("worker-a lost the lease while it was working");
    assert!(
        matches!(
            error,
            LeaseError::FenceLost {
                presented: 1,
                current: 2,
                ..
            }
        ),
        "unexpected error: {error}"
    );

    // worker-b, which holds it, can.
    lease::require_fence(&mut tx, &second).await.unwrap();
}

#[tokio::test]
async fn a_live_lease_is_not_stolen() {
    // Step 4 says *after* expiry. Before it, a second claimant is refused and
    // told who holds it — taking it early would mean two processes advancing
    // one benchmark, which §7.5 exists to prevent.
    let Some(db) = TempDb::migrated("lease_held").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    workflow_row(&pool, "w1").await;

    lease::claim(&pool, NET, "w1", KIND, "worker-a", 300)
        .await
        .unwrap();
    let error = lease::claim(&pool, NET, "w1", KIND, "worker-b", 300)
        .await
        .expect_err("worker-a still holds it");
    assert!(
        matches!(error, LeaseError::StillHeld { ref holder, .. } if holder == "worker-a"),
        "unexpected error: {error}"
    );

    // And the fence did not move, so worker-a's own commit still works.
    let fence: i64 = sqlx::query_scalar("SELECT fence_token FROM pool.work_lease")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(fence, 1, "a refused claim must not advance the fence");
}

#[tokio::test]
async fn the_holder_may_renew_and_keeps_working() {
    // A process that is still alive extends its own lease. The fence advances,
    // so the renewed lease is what it must present afterwards — a renewal is a
    // new claim, not a no-op.
    let Some(db) = TempDb::migrated("lease_renew").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    workflow_row(&pool, "w1").await;

    let first = lease::claim(&pool, NET, "w1", KIND, "worker-a", 60)
        .await
        .unwrap();
    let renewed = lease::claim(&pool, NET, "w1", KIND, "worker-a", 60)
        .await
        .expect("its own lease is renewable");
    assert_eq!(renewed.fence_token, first.fence_token + 1);

    let mut tx = sqlx::PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    assert!(
        lease::require_fence(&mut tx, &first).await.is_err(),
        "the pre-renewal fence is stale, even for the same owner"
    );
    lease::require_fence(&mut tx, &renewed).await.unwrap();
}

#[tokio::test]
async fn releasing_advances_the_fence_too() {
    // A released holder must not be able to commit either. Releasing without
    // advancing would leave its token valid, which is the same hole as a
    // reset fence.
    let Some(db) = TempDb::migrated("lease_release").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    workflow_row(&pool, "w1").await;

    let held = lease::claim(&pool, NET, "w1", KIND, "worker-a", 300)
        .await
        .unwrap();
    lease::release(&pool, &held).await.unwrap();

    let mut tx = sqlx::PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    assert!(lease::require_fence(&mut tx, &held).await.is_err());

    // And the work is immediately claimable by someone else.
    let next = lease::claim(&pool, NET, "w1", KIND, "worker-b", 60)
        .await
        .unwrap();
    assert!(next.fence_token > held.fence_token);
}

#[tokio::test]
async fn the_fence_can_never_go_backwards() {
    // The one property a late writer's safety rests on. If a fence could be
    // reset — by a reclaim, an operator repair, or a future path that rebuilt
    // the row — a token from a lost lease would match again and §7.5 step 3's
    // compare-and-set would admit exactly the write invariant 8 forbids.
    let Some(db) = TempDb::migrated("lease_monotonic").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    workflow_row(&pool, "w1").await;
    lease::claim(&pool, NET, "w1", KIND, "worker-a", 60)
        .await
        .unwrap();
    lease::claim(&pool, NET, "w1", KIND, "worker-a", 60)
        .await
        .unwrap();

    // Backwards is refused outright.
    let error = sqlx::query("UPDATE pool.work_lease SET fence_token = 1")
        .execute(&pool)
        .await
        .expect_err("the fence never goes backwards");
    assert!(
        error.to_string().contains("never go backwards"),
        "unexpected error: {error}"
    );

    // Handing the work to someone else without advancing it is refused too:
    // the previous owner's token would still be valid.
    let error = sqlx::query("UPDATE pool.work_lease SET lease_owner = 'worker-b'")
        .execute(&pool)
        .await
        .expect_err("a new owner needs a new fence");
    assert!(
        error.to_string().contains("must advance the fence"),
        "unexpected error: {error}"
    );

    // But shortening a lease is allowed without spending a fence: it hands the
    // work to nobody.
    sqlx::query("UPDATE pool.work_lease SET lease_until = now()")
        .execute(&pool)
        .await
        .expect("forcing an expiry is not a hand-over");
}

#[tokio::test]
async fn a_missing_lease_row_is_a_lost_fence_not_a_free_pass() {
    // Treating "no lease" as "nobody objects" would let a late writer commit
    // precisely when the bookkeeping was in its worst state.
    let Some(db) = TempDb::migrated("lease_missing").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    workflow_row(&pool, "w1").await;
    let held = lease::claim(&pool, NET, "w1", KIND, "worker-a", 300)
        .await
        .unwrap();

    // Through the owner connection: the controller has no DELETE grant, which
    // is itself deliberate. This simulates the bookkeeping catastrophe, not a
    // path any component has.
    let mut owner = sqlx::PgConnection::connect_with(&db.as_superuser())
        .await
        .unwrap();
    sqlx::query("DELETE FROM pool.work_lease")
        .execute(&mut owner)
        .await
        .unwrap();

    let mut tx = sqlx::PgConnection::connect_with(&db.as_role("pool_controller"))
        .await
        .unwrap();
    assert!(
        lease::require_fence(&mut tx, &held).await.is_err(),
        "a vanished lease is not permission"
    );
}

#[tokio::test]
async fn a_lease_cannot_outlive_its_workflow_row() {
    // The lease names work on a workflow, so it cannot exist without one —
    // the same reason `tig_write_intent` carries its own foreign key.
    let Some(db) = TempDb::migrated("lease_orphan").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;

    let error = lease::claim(&pool, NET, "never_decided", KIND, "worker-a", 60)
        .await
        .expect_err("no such workflow");
    assert!(
        error.to_string().contains("work_lease_has_a_workflow"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_zero_length_lease_is_refused() {
    // A lease that expires the instant it is taken is not a lease: the next
    // claimant steals it immediately and two processes work the same job.
    let Some(db) = TempDb::migrated("lease_zero").await else {
        return;
    };
    let pool = db.pool_as("pool_controller").await;
    workflow_row(&pool, "w1").await;
    assert!(matches!(
        lease::claim(&pool, NET, "w1", KIND, "worker-a", 0).await,
        Err(LeaseError::ZeroDuration)
    ));
}
